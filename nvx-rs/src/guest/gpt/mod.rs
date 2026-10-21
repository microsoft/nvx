//! GPT disks that the guest's kernel attached as block devices: a disk's partition table, which
//! [`crate::gpt`] reads and checks, validated against the partitions that the kernel created
//! from it.
//!
//! A guest that boots from such a disk, such as one whose first partition, p1, configures an
//! adapter and whose later partitions hold the read-only layers of its root, first resolves the
//! disk's block device by its role, checks the device's access mode and size, flushes its cached
//! buffers, and opens it. [`BlockDevice::validate`] then:
//!
//! 1. reads the disk's protective MBR, primary GPT header, and primary partition entry array
//!    through the device, at its logical block size and size, and checks them and the caller's
//!    [`Expected`] values, as [`Disk::read`] describes;
//! 2. asks the kernel to reread the disk's partition table, which drops the disk's partitions and
//!    creates them again from what the device holds;
//! 3. compares the kernel's partitions, as sysfs describes them, with the table's, until they
//!    match or a deadline passes: the kernel must have one partition for each of the table's, with
//!    its number, its first byte, and its size, and no other.
//!
//! It returns the block devices of the partitions: p1's, whose bytes the caller reads as opaque
//! configuration with [`Disk::read_p1`], and the layers', which the caller mounts. When the
//! kernel's partitions differ from the table that the validation read, for example because the
//! table changed after the read, or because the kernel read an MBR in its place, the validation
//! fails, so the caller never uses a partition that the table does not describe.
//!
//! The kernel creates a disk's partitions before its reread returns, but another program, such as
//! a device manager that probes the disk, may make it drop and create them again at any time. The
//! comparison therefore repeats, after pauses that grow from 1 ms to 50 ms, until the deadline,
//! which also bounds a caller that waits for partitions that the kernel creates late.
//!
//! The reread needs `CAP_SYS_ADMIN`, and fails while any partition of the disk is open, for
//! example while a layer is mounted.
//!
//! ```no_run
//! use std::fs::File;
//! use std::time::Duration;
//!
//! use nvx::gpt::Expected;
//! use nvx::guest::gpt::BlockDevice;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let file = File::open("/dev/vda")?;
//! let expected = Expected::new().disk_guid("8d2f3a4e-1c5b-4e7a-9f60-2b3c4d5e6f70".parse()?);
//! let validated = BlockDevice::new(&file)?.validate(&expected, Duration::from_secs(5))?;
//! let configuration = validated.disk().read_p1(&file, 64 * 1024)?;
//! println!("p1 holds {} bytes of configuration", configuration.len());
//! for layer in validated.lowers() {
//!     println!("layer {:?} is device {}", layer.name(), layer.device_number());
//! }
//! # Ok(())
//! # }
//! ```

mod error;
#[cfg(test)]
mod tests;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

pub use error::{DeviceError, DeviceOperation, Mismatch};

use super::parse::parse_decimal;
use super::sys::{self, Errno};
use crate::gpt::{Disk, Expected, Partition};

/// The first pause between two comparisons of the kernel's partitions with a table.
const FIRST_PAUSE: Duration = Duration::from_millis(1);

/// The longest pause between two comparisons of the kernel's partitions with a table.
const LONGEST_PAUSE: Duration = Duration::from_millis(50);

/// A device number: the major and minor numbers that name a device to the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceNumber {
    /// The major number, which names the device's driver.
    major: u32,
    /// The minor number, which names the device among the driver's.
    minor: u32,
}

impl DeviceNumber {
    /// Returns the major number, which names the device's driver.
    pub fn major(self) -> u32 {
        self.major
    }

    /// Returns the minor number, which names the device among the driver's.
    pub fn minor(self) -> u32 {
        self.minor
    }
}

/// Formats the number as sysfs does, `MAJOR:MINOR`.
impl fmt::Display for DeviceNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.major, self.minor)
    }
}

/// A whole-disk block device that the caller opened, and its geometry, to validate the GPT that
/// it holds.
#[derive(Debug)]
pub struct BlockDevice<'a> {
    /// The device, as the caller opened it.
    file: &'a File,
    /// The device's number.
    number: DeviceNumber,
    /// The device's logical block size, in bytes.
    logical_block_size: u32,
    /// The device's size, in bytes.
    size: u64,
    /// The device's sysfs directory, which holds a directory for each of its partitions.
    directory: PathBuf,
}

impl<'a> BlockDevice<'a> {
    /// Takes the block device that `file` opens, which must be a whole disk rather than one of
    /// its partitions, and reads its number, logical block size, and size.
    ///
    /// The device may be open for reading only: the validation never writes it.
    pub fn new(file: &'a File) -> Result<Self, DeviceError> {
        let status = file
            .metadata()
            .map_err(|source| DeviceError::device(DeviceOperation::Status, source))?;
        if !status.file_type().is_block_device() {
            return Err(DeviceError::NotBlockDevice);
        }
        let (major, minor) = sys::split_device_number(status.rdev());
        let (Ok(major), Ok(minor)) = (u32::try_from(major), u32::try_from(minor)) else {
            let source = io::Error::new(io::ErrorKind::InvalidData, "a device number overflows");
            return Err(DeviceError::device(DeviceOperation::Status, source));
        };
        let number = DeviceNumber { major, minor };
        let directory = Path::new(sys::BLOCK_DEVICE_DIRECTORY).join(number.to_string());
        // The disk's own description confirms that sysfs describes this device.
        let path = directory.join("dev");
        match read_device_number(&path)? {
            Some(described) if described == number => {}
            Some(_) => return Err(malformed(&path)),
            None => {
                let source = io::Error::from(io::ErrorKind::NotFound);
                return Err(DeviceError::sysfs(&path, source));
            }
        }
        let path = directory.join("partition");
        match fs::metadata(&path) {
            Ok(_) => return Err(DeviceError::NotWholeDisk),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(DeviceError::sysfs(&path, source)),
        }
        let logical_block_size = sys::logical_block_size(file.as_fd()).map_err(|errno| {
            DeviceError::device(DeviceOperation::LogicalBlockSize, errno.into())
        })?;
        let size = sys::block_device_size(file.as_fd())
            .map_err(|errno| DeviceError::device(DeviceOperation::Size, errno.into()))?;
        Ok(Self {
            file,
            number,
            logical_block_size,
            size,
            directory,
        })
    }

    /// Returns the device's number.
    pub fn number(&self) -> DeviceNumber {
        self.number
    }

    /// Returns the device's logical block size, in bytes.
    pub fn logical_block_size(&self) -> u32 {
        self.logical_block_size
    }

    /// Returns the device's size, in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Reads and checks the disk's partition table through the device, which must also meet
    /// `expected`, as [`Disk::read`] describes.
    pub fn read_disk(&self, expected: &Expected) -> Result<Disk, DeviceError> {
        Ok(Disk::read(
            self.file,
            self.logical_block_size,
            self.size,
            expected,
        )?)
    }

    /// Asks the kernel to reread the disk's partition table, and to create the disk's partitions
    /// again from what the device holds, before it returns.
    ///
    /// The kernel requires `CAP_SYS_ADMIN`, and fails while any partition of the disk is open.
    pub fn reread_partitions(&self) -> Result<(), DeviceError> {
        sys::reread_partitions(self.file.as_fd())
            .map_err(|errno| DeviceError::device(DeviceOperation::RereadPartitions, errno.into()))
    }

    /// Waits until the kernel's partitions of the device are exactly those of `disk`, which
    /// [`BlockDevice::read_disk`] read, and returns their block devices, or fails with the first
    /// difference that remains once `timeout` has passed.
    ///
    /// It compares the partitions once, then again after each pause, which grows from 1 ms to
    /// 50 ms, until they match or the timeout passes. Unlike [`BlockDevice::validate`], it does
    /// not ask the kernel to reread the table.
    pub fn await_partitions(
        &self,
        disk: &Disk,
        timeout: Duration,
    ) -> Result<ValidatedDisk, DeviceError> {
        let partitions =
            wait_for_partitions(disk, timeout, || observe_partitions(&self.directory))?;
        Ok(ValidatedDisk {
            disk: disk.clone(),
            partitions,
        })
    }

    /// Validates the disk, as the module describes: reads and checks its partition table, which
    /// must also meet `expected`, asks the kernel to reread it, and waits until the kernel's
    /// partitions match it, for at most `timeout`.
    pub fn validate(
        &self,
        expected: &Expected,
        timeout: Duration,
    ) -> Result<ValidatedDisk, DeviceError> {
        let disk = self.read_disk(expected)?;
        self.reread_partitions()?;
        self.await_partitions(&disk, timeout)
    }
}

/// A disk whose partition table [`BlockDevice`] checked, and the block devices that the kernel
/// created for its partitions, which matched the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedDisk {
    /// The disk's partition table.
    disk: Disk,
    /// The partitions' block devices, in the order of their numbers.
    partitions: Vec<PartitionDevice>,
}

impl ValidatedDisk {
    /// Returns the disk's partition table, with which the caller reads p1 through
    /// [`Disk::read_p1`].
    pub fn disk(&self) -> &Disk {
        &self.disk
    }

    /// Returns the block device of every partition, p1 first, in the order of their numbers,
    /// which run from 1 without a gap.
    pub fn partitions(&self) -> &[PartitionDevice] {
        &self.partitions
    }

    /// Returns the block device of p1, the partition whose bytes configure the adapter.
    pub fn p1(&self) -> &PartitionDevice {
        &self.partitions[0]
    }

    /// Returns the block devices of the partitions after p1, which hold the read-only layers of
    /// the root: at least one.
    pub fn lowers(&self) -> &[PartitionDevice] {
        &self.partitions[1..]
    }
}

/// A partition of a validated disk: its table entry, and the block device that the kernel
/// created for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionDevice {
    /// The partition's table entry.
    partition: Partition,
    /// The kernel's name of the partition's block device.
    name: OsString,
    /// The device number of the partition's block device.
    device_number: DeviceNumber,
}

impl PartitionDevice {
    /// Returns the partition's table entry: its number, GUIDs, and blocks.
    pub fn partition(&self) -> &Partition {
        &self.partition
    }

    /// Returns the kernel's name of the partition's block device, such as `vda2`, under which
    /// sysfs lists it and devtmpfs creates its node in `/dev`.
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// Returns the device number of the partition's block device.
    pub fn device_number(&self) -> DeviceNumber {
        self.device_number
    }
}

/// A partition that the kernel created for a disk, as its sysfs directory describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct KernelPartition {
    /// The partition's number.
    number: u8,
    /// The kernel's name of the partition's block device.
    name: OsString,
    /// The device number of the partition's block device.
    device_number: DeviceNumber,
    /// The partition's first byte on the disk.
    start: u64,
    /// The partition's size, in bytes.
    size: u64,
}

/// Compares the kernel's partitions of a disk, `kernel`, with the partitions of its table,
/// `disk`, in the order of their numbers, and returns their block devices, or the first
/// difference.
fn compare(disk: &Disk, kernel: &[KernelPartition]) -> Result<Vec<PartitionDevice>, Mismatch> {
    let table = disk.partitions();
    let highest = table
        .iter()
        .map(Partition::number)
        .chain(kernel.iter().map(|partition| partition.number))
        .max()
        .unwrap_or(0);
    let mut devices = Vec::with_capacity(table.len());
    for number in 1..=highest {
        let entry = table.iter().find(|entry| entry.number() == number);
        let observed = kernel.iter().find(|partition| partition.number == number);
        match (entry, observed) {
            (Some(entry), Some(observed)) => {
                if (observed.start, observed.size) != (entry.offset(), entry.size()) {
                    return Err(Mismatch::Extent {
                        number,
                        start: observed.start,
                        size: observed.size,
                        expected_start: entry.offset(),
                        expected_size: entry.size(),
                    });
                }
                devices.push(PartitionDevice {
                    partition: *entry,
                    name: observed.name.clone(),
                    device_number: observed.device_number,
                });
            }
            (Some(_), None) => return Err(Mismatch::Missing { number }),
            (None, Some(_)) => return Err(Mismatch::Unexpected { number }),
            (None, None) => {}
        }
    }
    Ok(devices)
}

/// Observes the kernel's partitions of a disk with `observe`, and compares them with the
/// partitions of the disk's table, `disk`, until they match or `timeout` has passed, as
/// [`BlockDevice::await_partitions`] describes.
fn wait_for_partitions(
    disk: &Disk,
    timeout: Duration,
    mut observe: impl FnMut() -> Result<Vec<KernelPartition>, DeviceError>,
) -> Result<Vec<PartitionDevice>, DeviceError> {
    let start = Instant::now();
    let mut pause = FIRST_PAUSE;
    loop {
        let mismatch = match compare(disk, &observe()?) {
            Ok(devices) => return Ok(devices),
            Err(mismatch) => mismatch,
        };
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(DeviceError::Partitions(mismatch));
        }
        thread::sleep(pause.min(remaining));
        pause = pause.saturating_mul(2).min(LONGEST_PAUSE);
    }
}

/// Reads the kernel's partitions of the disk whose sysfs directory is `directory`, in the order
/// of their numbers.
///
/// A partition that the kernel drops while this reads it is left out, as if the kernel had
/// dropped it before.
fn observe_partitions(directory: &Path) -> Result<Vec<KernelPartition>, DeviceError> {
    let entries =
        fs::read_dir(directory).map_err(|source| DeviceError::sysfs(directory, source))?;
    let mut partitions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| DeviceError::sysfs(directory, source))?;
        let path = entry.path();
        // Each partition has a directory, unlike the disk's attributes and links.
        let is_directory = entry
            .file_type()
            .map_err(|source| DeviceError::sysfs(&path, source))?
            .is_dir();
        if is_directory && let Some(partition) = read_partition(&path, entry.file_name())? {
            partitions.push(partition);
        }
    }
    partitions.sort_by_key(|partition| partition.number);
    Ok(partitions)
}

/// Reads the partition named `name` whose sysfs directory is `directory`, or returns `None` if
/// the directory describes no partition, as the disk's other directories do, or no longer
/// exists.
fn read_partition(
    directory: &Path,
    name: OsString,
) -> Result<Option<KernelPartition>, DeviceError> {
    let path = directory.join("partition");
    let Some(number) = read_number(&path)? else {
        return Ok(None);
    };
    let number = u8::try_from(number)
        .ok()
        .filter(|number| *number != 0)
        .ok_or_else(|| malformed(&path))?;
    let path = directory.join("start");
    let Some(start) = read_number(&path)? else {
        return Ok(None);
    };
    let start = start
        .checked_mul(sys::SYSFS_SECTOR_SIZE)
        .ok_or_else(|| malformed(&path))?;
    let path = directory.join("size");
    let Some(size) = read_number(&path)? else {
        return Ok(None);
    };
    let size = size
        .checked_mul(sys::SYSFS_SECTOR_SIZE)
        .ok_or_else(|| malformed(&path))?;
    let Some(device_number) = read_device_number(&directory.join("dev"))? else {
        return Ok(None);
    };
    Ok(Some(KernelPartition {
        number,
        name,
        device_number,
        start,
        size,
    }))
}

/// Reads the device number, `MAJOR:MINOR`, that the sysfs attribute at `path` holds, or returns
/// `None` if the attribute no longer exists.
fn read_device_number(path: &Path) -> Result<Option<DeviceNumber>, DeviceError> {
    let Some(text) = read_attribute(path)? else {
        return Ok(None);
    };
    let number = |text: &[u8]| parse_decimal(text).and_then(|number| u32::try_from(number).ok());
    let mut parts = text.splitn(2, |byte| *byte == b':');
    match (parts.next().and_then(number), parts.next().and_then(number)) {
        (Some(major), Some(minor)) => Ok(Some(DeviceNumber { major, minor })),
        _ => Err(malformed(path)),
    }
}

/// Reads the decimal number that the sysfs attribute at `path` holds, or returns `None` if the
/// attribute no longer exists.
fn read_number(path: &Path) -> Result<Option<u64>, DeviceError> {
    let Some(text) = read_attribute(path)? else {
        return Ok(None);
    };
    parse_decimal(&text)
        .map(Some)
        .ok_or_else(|| malformed(path))
}

/// Reads the value of the sysfs attribute at `path`, the one line that the kernel writes, without
/// its newline, or returns `None` if the attribute no longer exists.
fn read_attribute(path: &Path) -> Result<Option<Vec<u8>>, DeviceError> {
    match fs::read(path) {
        Ok(mut text) => {
            if text.pop() != Some(b'\n') || text.contains(&b'\n') {
                return Err(malformed(path));
            }
            Ok(Some(text))
        }
        // Opening an attribute of a device that the kernel dropped fails with `ENOENT`, and
        // reading one that it dropped after the open with `ENODEV`.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::ENODEV as i32) =>
        {
            Ok(None)
        }
        Err(source) => Err(DeviceError::sysfs(path, source)),
    }
}

/// Returns the error for the sysfs attribute at `path`, whose text is not in the form that the
/// kernel writes.
fn malformed(path: &Path) -> DeviceError {
    let source = io::Error::new(io::ErrorKind::InvalidData, "unexpected text");
    DeviceError::sysfs(path, source)
}
