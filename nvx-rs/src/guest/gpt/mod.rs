//! GPT disks that the guest's kernel attached as block devices: a disk's partition table, which
//! [`crate::gpt`] reads and checks, validated against the partitions that the kernel created
//! from it.
//!
//! A guest that boots from such a disk, such as one whose first partition configures an adapter
//! and whose later partitions hold the read-only layers of its root, first resolves the disk's
//! block device by its role, checks the device's access mode and size, flushes its cached
//! buffers, and opens it. [`BlockDevice::validate`] then:
//!
//! 1. reads the disk's protective MBR, primary GPT header, and primary partition entry array
//!    through the device, at its logical block size and size, and checks them and the caller's
//!    [`Expected`] values, as [`Disk::read`] describes;
//! 2. compares the kernel's partitions, as sysfs describes them, with the table's: the kernel
//!    must have one partition for each of the table's, with its number, its first byte, and its
//!    size, and no other. The kernel created its partitions from the disk's table when it found
//!    the disk, so they usually match, and the validation ends there;
//! 3. only when they differ, asks the kernel to reread the disk's partition table, which drops
//!    the disk's partitions and creates them again from what the device holds, and compares them
//!    again, until they match or a deadline passes.
//!
//! It returns the block devices of the partitions: the first partition's, whose bytes the caller
//! reads as opaque configuration with [`Disk::read_first_partition`], and the layers', which the
//! caller mounts. When the kernel's partitions still differ from the table that the validation
//! read after the reread, for example because the table changed after the read, or because the
//! kernel read an MBR in its place, the validation fails, so the caller never uses a partition
//! that the table does not describe.
//!
//! The kernel creates a disk's partitions before its reread returns, but another program, such as
//! a device manager that probes the disk, may make it drop and create them again at any time. The
//! comparison therefore repeats, after pauses that grow from 1 ms to 4 ms, until the deadline,
//! which also bounds a caller that waits for partitions that the kernel creates late. Each
//! comparison takes partitions that the kernel held together: it reads each partition's
//! description through one descriptor of the partition's sysfs directory, then lists the disk's
//! directories again, and checks each one again through its first descriptor, so that it never
//! combines partitions, or the attributes of one partition, from before and after a reread.
//!
//! The reread needs `CAP_SYS_ADMIN`, and fails while any partition of the disk is open, for
//! example while a layer is mounted. The validation of a disk whose partitions already match its
//! table needs neither.
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
//! let configuration = validated.disk().read_first_partition(&file, 64 * 1024)?;
//! println!("the first partition holds {} bytes of configuration", configuration.len());
//! for layer in validated.lowers() {
//!     println!("layer {:?} is device {}", layer.name(), layer.device_number());
//! }
//! # Ok(())
//! # }
//! ```

mod error;
#[cfg(test)]
mod tests;

use std::ffi::{CStr, OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

pub use error::{DeviceError, DeviceOperation, Mismatch};

use super::parse::parse_decimal;
use super::sys::{self, Errno, FileDescriptor, Mode, OpenFlags};
use crate::gpt::{Disk, Expected, Partition};

/// The first pause between two comparisons of the kernel's partitions with a table.
const FIRST_PAUSE: Duration = Duration::from_millis(1);

/// The longest pause between two comparisons of the kernel's partitions with a table, and so about
/// the longest that a wait takes to see partitions that match once the kernel has created them. A
/// comparison of a few partitions takes well under a millisecond, so the wait stays cheap.
const LONGEST_PAUSE: Duration = Duration::from_millis(4);

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
        // The disk's own description, read through one descriptor of its sysfs directory,
        // confirms that sysfs describes this device, and that the device is a whole disk.
        let absent = |path: &Path| DeviceError::sysfs(path, io::ErrorKind::NotFound.into());
        let Some(description) = SysfsDirectory::open(directory.clone())? else {
            return Err(absent(&directory));
        };
        match description.device_number(c"dev")? {
            Some(described) if described == number => {}
            Some(_) => return Err(malformed(&description.path_of(c"dev"))),
            None => return Err(absent(&description.path_of(c"dev"))),
        }
        if description.attribute(c"partition")?.is_some() {
            return Err(DeviceError::NotWholeDisk);
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
    /// 4 ms, until they match or the timeout passes. It never asks the kernel to reread the
    /// table, as [`BlockDevice::validate`] does when the partitions differ.
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
    /// must also meet `expected`, and compares the kernel's partitions with it. Only when they
    /// differ does it ask the kernel to reread the table, and wait until the partitions match it,
    /// for at most `timeout`.
    pub fn validate(
        &self,
        expected: &Expected,
        timeout: Duration,
    ) -> Result<ValidatedDisk, DeviceError> {
        let disk = self.read_disk(expected)?;
        match self.await_partitions(&disk, Duration::ZERO) {
            Err(DeviceError::Partitions(_)) => {}
            compared => return compared,
        }
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
    /// Returns the disk's partition table, with which the caller reads the first partition
    /// through [`Disk::read_first_partition`].
    pub fn disk(&self) -> &Disk {
        &self.disk
    }

    /// Returns the block device of every partition, in the order of their numbers, which run
    /// from 1 without a gap.
    pub fn partitions(&self) -> &[PartitionDevice] {
        &self.partitions
    }

    /// Returns the block device of the first partition, whose bytes configure the adapter.
    pub fn first_partition(&self) -> &PartitionDevice {
        &self.partitions[0]
    }

    /// Returns the block devices of the partitions after the first, which hold the read-only
    /// layers of the root: at least one.
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
///
/// An observation of `None` means that the partitions changed while they were read, and counts
/// as [`Mismatch::Changed`].
fn wait_for_partitions(
    disk: &Disk,
    timeout: Duration,
    mut observe: impl FnMut() -> Result<Option<Vec<KernelPartition>>, DeviceError>,
) -> Result<Vec<PartitionDevice>, DeviceError> {
    let start = Instant::now();
    let mut pause = FIRST_PAUSE;
    loop {
        let mismatch = match observe()? {
            Some(kernel) => match compare(disk, &kernel) {
                Ok(devices) => return Ok(devices),
                Err(mismatch) => mismatch,
            },
            None => Mismatch::Changed,
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
/// of their numbers, or returns `None` if they changed while this read them, as
/// [`confirm_partitions`] describes.
fn observe_partitions(directory: &Path) -> Result<Option<Vec<KernelPartition>>, DeviceError> {
    confirm_partitions(directory, read_subdirectories(directory)?)
}

/// A subdirectory of a disk's sysfs directory, open, with the partition that it described when it
/// was read.
struct Subdirectory {
    /// The subdirectory's name.
    name: OsString,
    /// The open subdirectory.
    directory: SysfsDirectory,
    /// The partition that the subdirectory described, or `None` if it described none, as the
    /// disk's other directories do.
    partition: Option<KernelPartition>,
}

impl Subdirectory {
    /// Opens and reads the subdirectory `name` of the disk's sysfs directory `directory`, or
    /// returns `None` if it doesn't exist.
    ///
    /// It reads the partition's attributes through one descriptor of the subdirectory, so that
    /// they all describe one partition, even when the kernel drops the partition and creates
    /// another under its name while they're read. A partition that the kernel drops while this
    /// reads it describes none, as if the kernel had dropped it before.
    fn read(directory: &Path, name: OsString) -> Result<Option<Self>, DeviceError> {
        let Some(opened) = SysfsDirectory::open(directory.join(&name))? else {
            return Ok(None);
        };
        let partition = opened.partition(name.clone())?;
        Ok(Some(Self {
            name,
            directory: opened,
            partition,
        }))
    }
}

/// Opens and reads each subdirectory of the disk's sysfs directory `directory`, in the order of
/// their names, and leaves out any that the kernel dropped before this opened it.
fn read_subdirectories(directory: &Path) -> Result<Vec<Subdirectory>, DeviceError> {
    let mut subdirectories = Vec::new();
    for name in list_subdirectories(directory)? {
        subdirectories.extend(Subdirectory::read(directory, name)?);
    }
    Ok(subdirectories)
}

/// Lists the names of the subdirectories of the disk's sysfs directory `directory`, in order.
fn list_subdirectories(directory: &Path) -> Result<Vec<OsString>, DeviceError> {
    let entries =
        fs::read_dir(directory).map_err(|source| DeviceError::sysfs(directory, source))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| DeviceError::sysfs(directory, source))?;
        // Each partition has a directory, unlike the disk's attributes and links.
        let is_directory = entry
            .file_type()
            .map_err(|source| DeviceError::sysfs(&entry.path(), source))?
            .is_dir();
        if is_directory {
            names.push(entry.file_name());
        }
    }
    names.sort();
    Ok(names)
}

/// Returns the partitions that the subdirectories `first`, which [`read_subdirectories`] read
/// from the disk's sysfs directory `directory`, described, in the order of their numbers, or
/// `None` if the kernel's partitions changed while they were read.
///
/// It lists the disk's subdirectories again, which must be those of `first`, then reads each
/// subdirectory of `first` again through the descriptor that it holds: a partition's `partition`
/// attribute must give the same number, and another subdirectory must still describe no
/// partition. The kernel never revives a directory that it dropped, even to create another under
/// its name, so each partition that passes existed from its first reading to its last, throughout
/// the second listing, and a partition keeps its number, first byte, and device number while it
/// exists. A reread drops every partition of the disk before it creates any, so the partitions
/// that pass all come from one reading of the table, never some from before a reread and some
/// from after it. The second listing shows that the kernel created no partition under another
/// name since the first reading, and each other subdirectory, that the kernel did not finish
/// creating a partition there. Only a program that deletes, adds, or resizes partitions one at a
/// time could still change them meanwhile, as it can after the comparison anyway.
fn confirm_partitions(
    directory: &Path,
    first: Vec<Subdirectory>,
) -> Result<Option<Vec<KernelPartition>>, DeviceError> {
    let names = list_subdirectories(directory)?;
    if !names
        .iter()
        .eq(first.iter().map(|subdirectory| &subdirectory.name))
    {
        return Ok(None);
    }
    let mut partitions = Vec::new();
    for subdirectory in first {
        let unchanged = match &subdirectory.partition {
            Some(partition) => {
                subdirectory.directory.number(c"partition")? == Some(partition.number.into())
            }
            None => subdirectory
                .directory
                .partition(subdirectory.name)?
                .is_none(),
        };
        if !unchanged {
            return Ok(None);
        }
        partitions.extend(subdirectory.partition);
    }
    partitions.sort_by_key(|partition| partition.number);
    Ok(Some(partitions))
}

/// The sysfs directory of a disk or of one of its partitions, open, so that every attribute read
/// through it describes the same device: once the kernel drops the device, even to create another
/// under the same name, the directory holds no attributes.
struct SysfsDirectory {
    /// The open directory.
    descriptor: FileDescriptor,
    /// The directory's path, which errors name.
    path: PathBuf,
}

impl SysfsDirectory {
    /// Opens the directory at `path`, or returns `None` if it doesn't exist.
    fn open(path: PathBuf) -> Result<Option<Self>, DeviceError> {
        let name =
            sys::path_c_string(&path).map_err(|errno| DeviceError::sysfs(&path, errno.into()))?;
        let flags = OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY | OpenFlags::O_CLOEXEC;
        match sys::open(&name, flags, Mode::empty()) {
            Ok(descriptor) => Ok(Some(Self { descriptor, path })),
            Err(Errno::ENOENT | Errno::ENODEV) => Ok(None),
            Err(errno) => Err(DeviceError::sysfs(&path, errno.into())),
        }
    }

    /// Returns the path of the attribute `name`, which errors name.
    fn path_of(&self, name: &CStr) -> PathBuf {
        self.path.join(OsStr::from_bytes(name.to_bytes()))
    }

    /// Reads the partition that the directory describes, named `name`, or returns `None` if the
    /// directory describes no partition, as the disk's other directories do, or the kernel
    /// dropped it.
    fn partition(&self, name: OsString) -> Result<Option<KernelPartition>, DeviceError> {
        let Some(number) = self.number(c"partition")? else {
            return Ok(None);
        };
        let number = u8::try_from(number)
            .ok()
            .filter(|number| *number != 0)
            .ok_or_else(|| malformed(&self.path_of(c"partition")))?;
        let Some(start) = self.bytes(c"start")? else {
            return Ok(None);
        };
        let Some(size) = self.bytes(c"size")? else {
            return Ok(None);
        };
        let Some(device_number) = self.device_number(c"dev")? else {
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

    /// Reads the number of 512-byte sectors that the attribute `name` holds, as sysfs counts a
    /// partition's start and size whatever the disk's logical block size, and returns it in
    /// bytes, or returns `None` if the attribute doesn't exist.
    fn bytes(&self, name: &CStr) -> Result<Option<u64>, DeviceError> {
        let Some(sectors) = self.number(name)? else {
            return Ok(None);
        };
        sectors
            .checked_mul(sys::SYSFS_SECTOR_SIZE)
            .map(Some)
            .ok_or_else(|| malformed(&self.path_of(name)))
    }

    /// Reads the device number, `MAJOR:MINOR`, that the attribute `name` holds, or returns
    /// `None` if the attribute doesn't exist.
    fn device_number(&self, name: &CStr) -> Result<Option<DeviceNumber>, DeviceError> {
        let Some(text) = self.attribute(name)? else {
            return Ok(None);
        };
        let number =
            |text: &[u8]| parse_decimal(text).and_then(|number| u32::try_from(number).ok());
        let mut parts = text.splitn(2, |byte| *byte == b':');
        match (parts.next().and_then(number), parts.next().and_then(number)) {
            (Some(major), Some(minor)) => Ok(Some(DeviceNumber { major, minor })),
            _ => Err(malformed(&self.path_of(name))),
        }
    }

    /// Reads the decimal number that the attribute `name` holds, or returns `None` if the
    /// attribute doesn't exist.
    fn number(&self, name: &CStr) -> Result<Option<u64>, DeviceError> {
        let Some(text) = self.attribute(name)? else {
            return Ok(None);
        };
        parse_decimal(&text)
            .map(Some)
            .ok_or_else(|| malformed(&self.path_of(name)))
    }

    /// Reads the value of the attribute `name`, the one line that the kernel writes, without its
    /// newline, or returns `None` if the attribute doesn't exist.
    fn attribute(&self, name: &CStr) -> Result<Option<Vec<u8>>, DeviceError> {
        let flags = OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC;
        // Opening an attribute of a device that the kernel dropped fails with `ENOENT`, or with
        // `ENODEV` while the kernel drops it, and reading one that it dropped after the open
        // fails with `ENODEV`.
        let mut file = match sys::open_at(&self.descriptor, name, flags, Mode::empty()) {
            Ok(file) => file,
            Err(Errno::ENOENT | Errno::ENODEV) => return Ok(None),
            Err(errno) => return Err(DeviceError::sysfs(&self.path_of(name), errno.into())),
        };
        let mut text = Vec::new();
        match file.read_to_end(&mut text) {
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(Errno::ENODEV as i32) => return Ok(None),
            Err(source) => return Err(DeviceError::sysfs(&self.path_of(name), source)),
        }
        if text.pop() != Some(b'\n') || text.contains(&b'\n') {
            return Err(malformed(&self.path_of(name)));
        }
        Ok(Some(text))
    }
}

/// Returns the error for the sysfs attribute at `path`, whose text is not in the form that the
/// kernel writes.
fn malformed(path: &Path) -> DeviceError {
    let source = io::Error::new(io::ErrorKind::InvalidData, "unexpected text");
    DeviceError::sysfs(path, source)
}
