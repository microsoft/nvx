//! Why a disk's block device failed its validation.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::gpt::GptError;

/// An operation on a block device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeviceOperation {
    /// Reading the device's status: its file type and device number.
    Status,
    /// Reading the device's logical block size.
    LogicalBlockSize,
    /// Reading the device's size.
    Size,
    /// Asking the kernel to reread the device's partition table.
    RereadPartitions,
}

/// Describes the operation as what failed, such as "reread the device's partition table".
impl fmt::Display for DeviceOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Status => "read the device's status",
            Self::LogicalBlockSize => "read the device's logical block size",
            Self::Size => "read the device's size",
            Self::RereadPartitions => "reread the device's partition table",
        })
    }
}

/// How the partitions that the kernel created for a disk differ from the partitions of its table:
/// the first difference, in the order of the partition numbers, or partitions that changed while
/// they were read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mismatch {
    /// The kernel has no partition of this number, which the table has.
    Missing {
        /// The partition's number.
        number: u8,
    },
    /// The kernel has a partition of this number, which the table lacks.
    Unexpected {
        /// The partition's number.
        number: u8,
    },
    /// The kernel's partition of this number spans other bytes of the disk than the table's.
    Extent {
        /// The partition's number.
        number: u8,
        /// The first byte of the kernel's partition.
        start: u64,
        /// The size of the kernel's partition, in bytes.
        size: u64,
        /// The first byte of the table's partition.
        expected_start: u64,
        /// The size of the table's partition, in bytes.
        expected_size: u64,
    },
    /// The kernel's partitions of the disk changed while they were read, as they do when the
    /// kernel rereads the partition table, so they could not be compared.
    Changed,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { number } => write!(
                formatter,
                "the kernel has no partition {number}, which the table has"
            ),
            Self::Unexpected { number } => write!(
                formatter,
                "the kernel has a partition {number}, which the table lacks"
            ),
            Self::Extent {
                number,
                start,
                size,
                expected_start,
                expected_size,
            } => write!(
                formatter,
                "the kernel's partition {number} has {size} bytes from byte {start}, but the \
                 table's has {expected_size} bytes from byte {expected_start}"
            ),
            Self::Changed => formatter.write_str("they changed while they were read"),
        }
    }
}

/// Why a disk's block device failed its validation: the device, its sysfs description, its
/// partition table, or the partitions that the kernel created from the table.
#[derive(Debug)]
#[non_exhaustive]
pub enum DeviceError {
    /// The file is not a block device.
    NotBlockDevice,
    /// The block device is a partition rather than a whole disk.
    NotWholeDisk,
    /// An operation on the device failed.
    Device {
        /// The operation.
        operation: DeviceOperation,
        /// Why it failed.
        source: io::Error,
    },
    /// A file of the device's sysfs description, or of one of its partitions', could not be read,
    /// or held text in another form than the kernel writes, which fails with
    /// [`io::ErrorKind::InvalidData`].
    Sysfs {
        /// The file.
        path: PathBuf,
        /// Why it could not be read.
        source: io::Error,
    },
    /// The disk's partition table is invalid, or does not meet the caller's expected values.
    Table(GptError),
    /// The kernel's partitions still did not match the table's when the timeout passed.
    Partitions(Mismatch),
}

impl DeviceError {
    /// Returns the error for `operation` on the device, which failed with `source`.
    pub(super) fn device(operation: DeviceOperation, source: io::Error) -> Self {
        Self::Device { operation, source }
    }

    /// Returns the error for the sysfs file at `path`, which could not be read.
    pub(super) fn sysfs(path: &Path, source: io::Error) -> Self {
        Self::Sysfs {
            path: path.to_owned(),
            source,
        }
    }
}

impl fmt::Display for DeviceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotBlockDevice => formatter.write_str("the file is not a block device"),
            Self::NotWholeDisk => {
                formatter.write_str("the block device is a partition, not a whole disk")
            }
            Self::Device { operation, source } => {
                write!(formatter, "cannot {operation}: {source}")
            }
            Self::Sysfs { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Table(error) => write!(formatter, "invalid partition table: {error}"),
            Self::Partitions(mismatch) => write!(
                formatter,
                "the kernel's partitions do not match the table's: {mismatch}"
            ),
        }
    }
}

impl std::error::Error for DeviceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Device { source, .. } | Self::Sysfs { source, .. } => Some(source),
            Self::Table(error) => Some(error),
            _ => None,
        }
    }
}

impl From<GptError> for DeviceError {
    fn from(error: GptError) -> Self {
        Self::Table(error)
    }
}
