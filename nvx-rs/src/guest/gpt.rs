//! GPT disks that the guest's kernel attached as block devices.
//!
//! This module is NVX's internal reference code of the guest's space, hidden from the
//! documentation and unstable: it becomes crate-private once nothing outside NVX uses it.
//! [`crate::gpt`], which reads and checks the table, stays public.
//!
//! [`read_disk`] reads the partition table of a disk through the disk's block device, such as
//! `/dev/vda`, that the caller opened. It asks the device for its logical block size and its
//! size, as `blockdev --getss` and `blockdev --getsize64` do, and reads and checks the table at
//! that geometry with [`Disk::read`], as [`crate::gpt`] describes, with the caller's [`Expected`]
//! values, such as the disk GUID that a boot parameter names.
//!
//! It only reads the device, which may be open for reading only, at explicit offsets, so it never
//! moves the file's position. The kernel created the disk's partitions, such as `/dev/vda1`, from
//! the table that the disk held when the kernel found it; [`read_disk`] neither compares them
//! with the table that it reads nor asks the kernel to reread the table.
//!
//! ```no_run
//! use std::fs::File;
//!
//! use nvx::gpt::Expected;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let device = File::open("/dev/vda")?;
//! let expected = Expected::new().disk_guid("8d2f3a4e-1c5b-4e7a-9f60-2b3c4d5e6f70".parse()?);
//! let disk = nvx::guest::gpt::read_disk(&device, &expected)?;
//! let configuration = disk.read_first_partition(&device, 64 * 1024)?;
//! println!("the first partition holds {} bytes of configuration", configuration.len());
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;

use super::sys;
use crate::gpt::{Disk, Expected, GptError};

/// Reads the partition table of the block device that `device` opened, at the device's logical
/// block size and size, and checks it and the caller's `expected` values, as the module
/// describes.
pub fn read_disk(device: &File, expected: &Expected) -> Result<Disk, DeviceError> {
    let logical_block_size = sys::logical_block_size(device.as_fd())
        .map_err(|error| DeviceError::LogicalBlockSize(error.into()))?;
    let size =
        sys::block_device_size(device.as_fd()).map_err(|error| DeviceError::Size(error.into()))?;
    Disk::read(device, logical_block_size, size, expected).map_err(DeviceError::Table)
}

/// Why [`read_disk`] failed: an operation on the block device, or its partition table.
#[derive(Debug)]
#[non_exhaustive]
pub enum DeviceError {
    /// The device's logical block size could not be read, as for a file that is not a block
    /// device, which fails with `ENOTTY`.
    LogicalBlockSize(io::Error),
    /// The device's size could not be read.
    Size(io::Error),
    /// The disk's partition table is invalid, or does not meet the caller's expected values.
    Table(GptError),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LogicalBlockSize(error) => write!(
                formatter,
                "failed to read the device's logical block size: {error}"
            ),
            Self::Size(error) => write!(formatter, "failed to read the device's size: {error}"),
            Self::Table(error) => write!(formatter, "invalid partition table: {error}"),
        }
    }
}

impl std::error::Error for DeviceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LogicalBlockSize(error) | Self::Size(error) => Some(error),
            Self::Table(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests;
