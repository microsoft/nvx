//! The kernel's mount table, and the mounts in it that keep a template's guest from capturing.
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

use super::{Devices, System};
use crate::guest::parse::parse_decimal;

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

/// Returns the first mount of the mount table `table` that is from `devices` or an overlay, bind
/// mounts included, or `None` if there is none.
///
/// A mount is from the distro device or the scratch device when its file system's device, which
/// `system` finds in sysfs by its number, is below the sysfs directory of the device's MMIO
/// window, where the kernel creates the device's block device and partitions. A mount is from the
/// filesystem tag when its file system is virtio-fs and its source is the tag, and from an overlay
/// when its file system is one. A line that is not of the kernel's form, or a file system on a
/// block device that sysfs does not show, fails the check, which never passes a mount that it
/// cannot tell apart.
pub(super) fn first_blocking(
    system: &mut dyn System,
    devices: &Devices,
    table: &[u8],
) -> io::Result<Option<Mounted>> {
    let distro = system.windows(devices.distro.mmio)?;
    let scratch = system.windows(devices.scratch.mmio)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the line that the kernel writes for a mount of `filesystem` from `source` at
    /// `mount_point`, on the device `device`, with the optional fields `optional`.
    fn line(
        device: &str,
        mount_point: &str,
        optional: &str,
        filesystem: &str,
        source: &str,
    ) -> String {
        format!("36 25 {device} / {mount_point} rw,relatime{optional} - {filesystem} {source} rw")
    }

    #[test]
    fn a_line_tells_the_device_mount_point_type_and_source_of_its_mount() {
        let text = line(
            "254:2",
            "/run/nvx/layers/p2",
            " shared:7 master:1",
            "ext4",
            "/dev/vda2",
        );
        assert_eq!(
            Line::parse(text.as_bytes()),
            Some(Line {
                device: DeviceNumber {
                    major: 254,
                    minor: 2
                },
                mount_point: PathBuf::from("/run/nvx/layers/p2"),
                filesystem: b"ext4".to_vec(),
                source: b"/dev/vda2".to_vec(),
            })
        );
        // Without optional fields or the file system's options.
        let parsed = Line::parse(b"1 1 0:2 / / rw - rootfs rootfs").unwrap();
        assert_eq!(parsed.device, DeviceNumber { major: 0, minor: 2 });
        assert_eq!(parsed.filesystem, b"rootfs");
        assert_eq!(parsed.source, b"rootfs");
        // A source that the mounter left empty, and file system options of any form.
        let parsed = Line::parse(b"36 25 0:50 / /mnt rw - tmpfs  rw  x").unwrap();
        assert_eq!(parsed.filesystem, b"tmpfs");
        assert_eq!(parsed.source, b"");
        assert_eq!(
            DeviceNumber {
                major: 259,
                minor: 0
            }
            .to_string(),
            "259:0"
        );
    }

    #[test]
    fn escaped_fields_are_unescaped() {
        let text = line(
            "0:31",
            "/mnt/a\\040b\\011c\\012d\\134e",
            "",
            "virt\\134iofs",
            "tag\\040x",
        );
        let parsed = Line::parse(text.as_bytes()).unwrap();
        assert_eq!(parsed.mount_point, Path::new("/mnt/a b\tc\nd\\e"));
        assert_eq!(parsed.filesystem, b"virt\\iofs");
        assert_eq!(parsed.source, b"tag x");
        // A backslash that starts no escape stays, and so does one before a byte above 0o377.
        assert_eq!(unescape(b"a\\b\\04\\400\\377"), b"a\\b\\04\\400\xff");
    }

    #[test]
    fn lines_of_another_form_are_refused() {
        for text in [
            "",
            "36 25 254:2 / /mnt rw - ext4",
            "36 25 254:2 / /mnt rw shared:1 ext4 /dev/vda rw",
            "36 25 254:2 / /mnt rw",
            "x 25 254:2 / /mnt rw - ext4 /dev/vda rw",
            "36 -1 254:2 / /mnt rw - ext4 /dev/vda rw",
            "36 25 254 / /mnt rw - ext4 /dev/vda rw",
            "36 25 254:x / /mnt rw - ext4 /dev/vda rw",
            "36 25 4294967296:0 / /mnt rw - ext4 /dev/vda rw",
            "36 25 +254:0 / /mnt rw - ext4 /dev/vda rw",
            "36 25  254:0 / /mnt rw - ext4 /dev/vda rw",
            "36 25 254:0 /  /mnt rw - ext4 /dev/vda rw",
            "36 25 254:0 / /mnt rw  shared:1 - ext4 /dev/vda rw",
            "36 25 254:0 / /mnt rw -  ext4 /dev/vda rw",
        ] {
            assert_eq!(Line::parse(text.as_bytes()), None, "{text:?}");
        }
    }
}
