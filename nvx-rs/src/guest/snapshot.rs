//! A Fresh platform capture and restore repair in the calling guest process.
//!
//! [`capture`] checks mounts and the time ABI, suppresses the stall detectors, reads the
//! generation ID, and requests capture. All pre-request steps, locks, waits and rollback
//! share one caller deadline. The request has no deadline while the host saves stopped
//! processors. A strict time violation is returned with its status, never powered off here.
//!
//! The caller keeps its executions still. After answering the capture request, its transport
//! sends nothing until this call returns. After [`Captured::Continued`] or a failure with
//! [`Disposition::Resumed`], it reports on the same session. After [`Captured::Restored`], it
//! takes the session's reset and attaches again before receiving the activation record.
//!
//! On a restore, the existing repair runs before this call returns: it checks the restore and
//! its seed, activates its processor target and then its memory target, reseeds the kernel's
//! random number generator, drops the seed, and finishes the restore with its acknowledgement.
//! The finish hands deferred time work, including detector release, to the time daemon. The
//! returned [`Repaired`] reports the generation ID, online CPUs and memory, and acknowledgement
//! instant. A repair failure is always [`Disposition::MustEnd`], with [`RepairError::status`].
//! Fresh restore network settings are explicitly refused.
//!
//! Each line of `/proc/self/mountinfo` describes one mount with fields separated by spaces: its
//! ID, its parent's ID, the device number of its file system, `MAJ:MIN`, the root of the mount
//! in its file system, the mount point, the mount's options, optional fields, a `-` that ends
//! them, then the file system's type, the mount's source, and the file system's options. The
//! kernel writes a space, a tab, a newline, or a backslash within a field as a backslash and
//! three octal digits.

pub(super) mod repair;

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::activation::{self, System};
use super::sys;
use super::time::{CaptureOutcome, CaptureRequest, PreCaptureError, ScratchPolicy, TimeError};
use crate::guest::parse::parse_decimal;
use crate::protocol::status;

pub use self::repair::{
    RangeFault, RepairCause, RepairError, RepairProgress, RepairStage, Repaired,
};
pub use super::activation::{CaptureCause, CaptureStage};

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

/// The disk policy of a capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Disks {
    /// Capture before mounting the disks that each clone replaces.
    Fresh,
}

/// How a capture ended in this guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Captured {
    /// No snapshot was taken; the stall detectors were restored and this guest may continue.
    Continued,
    /// A restore started this clone, and its repair completed before capture returned.
    Restored(Repaired),
}

/// Whether the caller may resume the guest or must end it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Disposition {
    /// No detector suppression owned by this call remains.
    Resumed,
    /// The guest cannot safely continue.
    MustEnd {
        /// The exit status, preserving a strict time violation's actual status.
        status: u8,
    },
}

/// A capture failure and what the caller must do with the guest.
#[derive(Debug)]
pub struct CaptureFailure {
    /// The stage that failed.
    pub stage: CaptureStage,
    /// The failure, or a failed detector restoration.
    pub cause: CaptureCause,
    /// Whether the guest may resume or must end.
    pub disposition: Disposition,
}

impl CaptureFailure {
    /// Classifies a failure, making request and release failures final.
    fn new(stage: CaptureStage, cause: CaptureCause) -> Self {
        let must_end = matches!(stage, CaptureStage::Capture | CaptureStage::Release);
        let status = match &cause {
            CaptureCause::Time(TimeError::Violation(violation)) => Some(violation.status()),
            CaptureCause::Time(TimeError::PortRelease(_)) => Some(status::TIME_REPAIR),
            _ => must_end.then_some(status::TIME_REPAIR),
        };
        Self {
            stage,
            cause,
            disposition: status.map_or(Disposition::Resumed, |status| Disposition::MustEnd {
                status,
            }),
        }
    }

    /// Retains the existing repair's detail and the status with which the guest must end.
    fn unrepaired(error: RepairError) -> Self {
        Self {
            stage: CaptureStage::Repair,
            disposition: Disposition::MustEnd {
                status: error.status(),
            },
            cause: CaptureCause::Repair(error),
        }
    }

    /// Restores only this call's suppression, retaining a port-release cause and fatal status.
    fn cleanup(mut self, system: &mut dyn System, suppressed: bool) -> Self {
        if suppressed
            && let Err(cause) = release(system)
            && !matches!(self.cause, CaptureCause::Time(TimeError::PortRelease(_)))
        {
            let failure = Self::new(CaptureStage::Release, cause);
            self.stage = failure.stage;
            self.cause = failure.cause;
            if self.disposition == Disposition::Resumed {
                self.disposition = failure.disposition;
            }
        }
        self
    }
}

impl fmt::Display for CaptureFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed to {}: {}", self.stage, self.cause)
    }
}

impl std::error::Error for CaptureFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            CaptureCause::Io(error) => Some(error),
            CaptureCause::Time(error) => Some(error),
            CaptureCause::Repair(error) => Some(error),
            CaptureCause::Mounted(_) | CaptureCause::Pending => None,
        }
    }
}

/// Captures a Fresh template without freezing or inspecting the caller's executions.
///
/// The deadline bounds pre-request work only. This call drives no transport and never powers off
/// the VM. A restore is repaired inline, and failures return its disposition. Only a report-only
/// finish without a running time daemon starts helper processes, as
/// [`restore_finish`](super::time::TimeAbi::restore_finish) describes.
pub fn capture(
    devices: &Devices,
    disks: Disks,
    deadline: Instant,
) -> Result<Captured, CaptureFailure> {
    let Disks::Fresh = disks;
    sys::check_released().map_err(|error| {
        CaptureFailure::new(
            CaptureStage::CaptureChecks,
            CaptureCause::Time(error.into()),
        )
    })?;
    before(CaptureStage::Mounts, deadline)?;
    capture_with(
        activation::capture_system(Some(deadline)).as_mut(),
        devices,
        deadline,
        repair::live_system(),
    )
}

/// Composes the existing kernel/time operations with one explicit pre-request deadline.
fn capture_with(
    system: &mut dyn System,
    devices: &Devices,
    deadline: Instant,
    repair_system: Box<dyn repair::RepairSystem>,
) -> Result<Captured, CaptureFailure> {
    let mut suppressed = false;
    let prepared: Result<_, CaptureFailure> = (|| {
        check_mounts(system, devices, Some(deadline))
            .map_err(|cause| CaptureFailure::new(CaptureStage::Mounts, cause))?;
        system
            .pre_capture_until(deadline)
            .map_err(|(error, held)| {
                suppressed = held;
                let (stage, cause, _) = preparation_failure(error);
                CaptureFailure::new(stage, cause)
            })?;
        suppressed = true;
        before(CaptureStage::GenerationId, deadline)?;
        let generation = system.generation_id().map_err(|error| {
            CaptureFailure::new(CaptureStage::GenerationId, CaptureCause::Time(error))
        })?;
        before(CaptureStage::GenerationId, deadline)?;
        Ok(CaptureRequest::in_memory(ScratchPolicy::Fresh, generation))
    })();
    let request = prepared.map_err(|failure| failure.cleanup(system, suppressed))?;
    let outcome = system.capture(request).map_err(|(error, unbounded)| {
        let stage = if unbounded {
            CaptureStage::Capture
        } else {
            CaptureStage::CaptureChecks
        };
        CaptureFailure::new(stage, CaptureCause::Time(error)).cleanup(system, suppressed)
    })?;
    match outcome {
        CaptureOutcome::Continued => {
            release(system).map_err(|cause| CaptureFailure::new(CaptureStage::Release, cause))?;
            Ok(Captured::Continued)
        }
        CaptureOutcome::Restored(restore) => {
            if restore.network().is_some() {
                return Err(CaptureFailure::new(
                    CaptureStage::Capture,
                    CaptureCause::Io(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "a Fresh platform restore must not carry network settings",
                    )),
                )
                .cleanup(system, suppressed));
            }
            repair::Repair::with_system(restore, repair_system)
                .run()
                .map(Captured::Restored)
                .map_err(CaptureFailure::unrepaired)
        }
    }
}

/// Checks the same caller deadline before the next pre-request step.
fn before(stage: CaptureStage, deadline: Instant) -> Result<(), CaptureFailure> {
    sys::FileLock::check_deadline(Some(deadline))
        .map_err(|error| CaptureFailure::new(stage, CaptureCause::Io(error.into())))
}

/// Restores detectors and prioritizes any port permission that the thread still holds.
fn release(system: &mut dyn System) -> Result<(), CaptureCause> {
    let result = system.cancel_capture();
    sys::check_released().map_err(|error| CaptureCause::Time(error.into()))?;
    result.map_err(|detail| {
        CaptureCause::Io(io::Error::other(
            String::from_utf8_lossy(&detail).into_owned(),
        ))
    })
}

/// Checks the one bounded mount table for both capture interfaces.
pub(super) fn check_mounts(
    system: &mut dyn System,
    devices: &Devices,
    deadline: Option<Instant>,
) -> Result<(), CaptureCause> {
    sys::FileLock::check_deadline(deadline).map_err(|error| CaptureCause::Io(error.into()))?;
    let table = system.mount_table().map_err(CaptureCause::Io)?;
    if table.len() > MOUNT_TABLE_LIMIT {
        return Err(CaptureCause::Io(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("the mount table is longer than {MOUNT_TABLE_LIMIT} bytes"),
        )));
    }
    match first_blocking(system, devices, &table, deadline).map_err(CaptureCause::Io)? {
        Some(mounted) => Err(CaptureCause::Mounted(mounted)),
        None => Ok(()),
    }
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
        let mut fields = line.split(|&byte| byte == b' ');
        let fixed: [Option<&[u8]>; FIXED_FIELDS] = std::array::from_fn(|_| fields.next());
        let [
            Some(id),
            Some(parent),
            Some(device),
            Some(root),
            Some(mount_point),
            Some(options),
        ] = fixed
        else {
            return None;
        };
        parse_decimal(id)?;
        parse_decimal(parent)?;
        let device = DeviceNumber::parse(device)?;
        // The kernel writes each field with a byte at least, but the source, which is empty when
        // the mounter named an empty one.
        if root.is_empty() || mount_point.is_empty() || options.is_empty() {
            return None;
        }
        for field in fields.by_ref() {
            if field == SEPARATOR {
                break;
            }
            if field.is_empty() {
                return None;
            }
        }
        let filesystem = fields.next()?;
        let source = fields.next()?;
        if filesystem.is_empty() {
            return None;
        }
        Some(Self {
            device,
            mount_point: PathBuf::from(OsString::from_vec(unescape(mount_point))),
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
    deadline: Option<Instant>,
) -> io::Result<Option<Mounted>> {
    sys::FileLock::check_deadline(deadline)?;
    let distro = system.windows(devices.distro)?;
    sys::FileLock::check_deadline(deadline)?;
    let scratch = system.windows(devices.scratch)?;
    sys::FileLock::check_deadline(deadline)?;
    let tag = devices.filesystem_tag.as_bytes();
    for (index, text) in table.split(|&byte| byte == b'\n').enumerate() {
        sys::FileLock::check_deadline(deadline)?;
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
            sys::FileLock::check_deadline(deadline)?;
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
pub(super) mod tests;
