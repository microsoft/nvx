//! The devices and mount checks of a platform snapshot.
//!
//! Each line of `/proc/self/mountinfo` describes one mount with fields separated by spaces: its
//! ID, its parent's ID, the device number of its file system, `MAJ:MIN`, the root of the mount
//! in its file system, the mount point, the mount's options, optional fields, a `-` that ends
//! them, then the file system's type, the mount's source, and the file system's options. The
//! kernel writes a space, a tab, a newline, or a backslash within a field as a backslash and
//! three octal digits.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use super::activation::{CaptureCause, CaptureStage, System};
use super::time::PreCaptureError;
use crate::guest::parse::parse_decimal;

/// The most bytes of the caller's mount table that capture accepts: 1 MiB.
pub const MOUNT_TABLE_LIMIT: usize = 1 << 20;

/// The devices from which a platform capture must find nothing mounted.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Devices {
    /// The distro device's MMIO base, including all of its partitions.
    pub distro: u64,
    /// The scratch device's MMIO base.
    pub scratch: u64,
    /// The host filesystem device's virtio-fs tag.
    pub filesystem_tag: String,
}

/// The file system type of an overlay, as the mount table shows it.
const OVERLAY: &[u8] = b"overlay";
/// The file system type of a virtio-fs device's mounts, whose source is the device's tag.
const VIRTIOFS: &[u8] = b"virtiofs";
/// The field that ends a line's optional fields.
const SEPARATOR: &[u8] = b"-";
/// The number of fields before a line's optional fields.
const FIXED_FIELDS: usize = 6;

/// The number of a device, `MAJ:MIN` in the mount table, as its file system's files report it in
/// their `st_dev`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct DeviceNumber {
    /// The major number. A file system without a block device, such as an overlay, a virtio-fs
    /// device's, or a tmpfs, has an anonymous device, whose major number is 0.
    pub(super) major: u32,
    /// The minor number.
    pub(super) minor: u32,
}

impl DeviceNumber {
    /// Parses the device number of a mount table's line, two canonical decimal numbers separated
    /// by `:`, or returns `None` for anything else.
    fn parse(field: &[u8]) -> Option<Self> {
        let colon = field.iter().position(|&byte| byte == b':')?;
        let number = |text| parse_decimal(text).and_then(|number| u32::try_from(number).ok());
        Some(Self {
            major: number(&field[..colon])?,
            minor: number(&field[colon + 1..])?,
        })
    }
}

impl fmt::Display for DeviceNumber {
    /// Writes the number as `MAJ:MIN`, as the mount table and sysfs name a device.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.major, self.minor)
    }
}

/// What a line of the mount table tells of a mount: where it is, and what it is from.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Line {
    /// The device number of the mount's file system.
    device: DeviceNumber,
    /// The mount point.
    mount_point: PathBuf,
    /// The file system's type.
    filesystem: Vec<u8>,
    /// The mount's source, as the mounter named it: a block device's node, a virtio-fs device's
    /// tag, or any other text.
    source: Vec<u8>,
}

impl Line {
    /// Parses one line of the mount table, without its newline, or returns `None` for a line of
    /// another form than the kernel's.
    fn parse(line: &[u8]) -> Option<Self> {
        let fields = line.split(|&byte| byte == b' ').collect::<Vec<_>>();
        if fields.len() < FIXED_FIELDS {
            return None;
        }
        parse_decimal(fields[0])?;
        parse_decimal(fields[1])?;
        let device = DeviceNumber::parse(fields[2])?;
        let separator = FIXED_FIELDS
            + fields[FIXED_FIELDS..]
                .iter()
                .position(|&field| field == SEPARATOR)?;
        let [filesystem, source, ..] = &fields[separator + 1..] else {
            return None;
        };
        // The kernel writes each field with a byte at least, but the source, which is empty when
        // the mounter named an empty one.
        if fields[..separator]
            .iter()
            .chain([filesystem])
            .any(|field| field.is_empty())
        {
            return None;
        }
        Some(Self {
            device,
            mount_point: PathBuf::from(OsString::from_vec(unescape(fields[4]))),
            filesystem: unescape(filesystem),
            source: unescape(source),
        })
    }
}

/// Returns `field` of the mount table with the kernel's escapes replaced by the bytes that they
/// stand for: a backslash and three octal digits, of a byte up to 0o377. Any other backslash
/// stays.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(field.len());
    let mut rest = field;
    while let Some((&byte, after)) = rest.split_first() {
        if let (
            b'\\',
            [
                high @ b'0'..=b'3',
                middle @ b'0'..=b'7',
                low @ b'0'..=b'7',
                tail @ ..,
            ],
        ) = (byte, after)
        {
            bytes.push(((high - b'0') << 6) | ((middle - b'0') << 3) | (low - b'0'));
            rest = tail;
        } else {
            bytes.push(byte);
            rest = after;
        }
    }
    bytes
}

/// What a mount that keeps a template's guest from capturing is mounted from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Origin {
    /// The distro block device, or one of its partitions.
    Distro,
    /// The scratch block device.
    Scratch,
    /// An overlay, such as a layered root's.
    Overlay,
    /// The host filesystem device, whose virtio-fs tag the mount names as its source.
    Filesystem,
}

impl fmt::Display for Origin {
    /// Writes what the mount is from, such as `the distro device`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Distro => "the distro device",
            Self::Scratch => "the scratch device",
            Self::Overlay => "an overlay",
            Self::Filesystem => "the filesystem tag",
        })
    }
}

/// A mount that keeps a template's guest from capturing, as the mount table shows it.
///
/// Its `Display` form names its mount point and its origin, such as `/run/nvx/rootfs is mounted
/// from an overlay`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Mounted {
    /// What the mount is from.
    origin: Origin,
    /// The mount point.
    mount_point: PathBuf,
}

impl Mounted {
    /// Returns what the mount is from.
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// Returns the mount point, as the mount table shows it.
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }
}

impl fmt::Display for Mounted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} is mounted from {}",
            self.mount_point.display(),
            self.origin
        )
    }
}

/// Returns the first mount from the named devices or an overlay, bind mounts included.
///
/// Device numbers are resolved through sysfs; malformed lines and unresolved block devices
/// fail closed rather than passing a mount that cannot be classified.
pub(super) fn first_blocking(
    system: &mut dyn System,
    devices: &Devices,
    table: &[u8],
) -> io::Result<Option<Mounted>> {
    let distro = system.windows(devices.distro)?;
    let scratch = system.windows(devices.scratch)?;
    let tag = devices.filesystem_tag.as_bytes();
    for (index, text) in table.split(|&byte| byte == b'\n').enumerate() {
        if text.is_empty() {
            continue;
        }
        let line = Line::parse(text).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line {} of the mount table is malformed", index + 1),
            )
        })?;
        let origin = if line.filesystem == OVERLAY {
            Some(Origin::Overlay)
        } else if line.filesystem == VIRTIOFS && line.source == tag {
            Some(Origin::Filesystem)
        } else if line.device.major == 0 {
            None
        } else {
            let device = system.block_device(line.device)?;
            let below =
                |windows: &[PathBuf]| windows.iter().any(|window| device.starts_with(window));
            if below(&distro) {
                Some(Origin::Distro)
            } else if below(&scratch) {
                Some(Origin::Scratch)
            } else {
                None
            }
        };
        if let Some(origin) = origin {
            return Ok(Some(Mounted {
                origin,
                mount_point: line.mount_point,
            }));
        }
    }
    Ok(None)
}

/// The existing preparation's stage and cause, and whether it owns suppression to undo.
pub(crate) type PreparationFailure = (CaptureStage, CaptureCause, bool);

/// Classifies preparation without changing its suppression ownership.
pub(super) fn preparation_failure(error: PreCaptureError) -> PreparationFailure {
    let suppressed = error.holds_suppression();
    let (stage, cause) = match error {
        PreCaptureError::BootChecks(error) => (CaptureStage::BootChecks, CaptureCause::Time(error)),
        PreCaptureError::CaptureChecks(error) | PreCaptureError::PortRelease { error, .. } => {
            (CaptureStage::CaptureChecks, CaptureCause::Time(error))
        }
        PreCaptureError::Pending => (CaptureStage::Suppression, CaptureCause::Pending),
        PreCaptureError::Suppression(error) | PreCaptureError::Unrestored(error) => {
            (CaptureStage::Suppression, CaptureCause::Io(error.into()))
        }
    };
    (stage, cause, suppressed)
}

#[cfg(test)]
mod tests;
