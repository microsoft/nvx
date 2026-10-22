//! Why a disk failed.

use std::fmt;
use std::io;

use super::{
    ENTRY_SIZE, HEADER_FIELDS_SIZE, MAX_ENTRY_ARRAY_BYTES, MAX_LOGICAL_BLOCK_SIZE, MAX_PARTITIONS,
    MIN_LOGICAL_BLOCK_SIZE, MIN_PARTITIONS,
};
use crate::uuid::Uuid;

/// Why a disk failed: a read of its source, a check of the format, or an expectation of the
/// caller.
///
/// Apart from a failed read, the checks run in the order of the variants, those of one partition
/// for each partition in turn, so a disk fails with the first check that it breaks.
#[derive(Debug)]
#[non_exhaustive]
pub enum GptError {
    /// Reading the source failed.
    Io(io::Error),
    /// The logical block size is not a power of two from [`MIN_LOGICAL_BLOCK_SIZE`] through
    /// [`MAX_LOGICAL_BLOCK_SIZE`] bytes.
    LogicalBlockSize(u32),
    /// The disk's size is not a whole number of logical blocks, or leaves no block after the
    /// MBR for the header.
    DiskSize {
        /// The disk's size, in bytes.
        size: u64,
        /// The size of a logical block, in bytes.
        logical_block_size: u32,
    },
    /// LBA 0 lacks the MBR signature.
    MbrSignature,
    /// The MBR has no protective record, of type `0xEE`, so Linux would not read a GPT.
    NoProtectiveRecord,
    /// The MBR's protective record starts at this LBA, rather than at the header's, LBA 1.
    ProtectiveRecordStart(u32),
    /// The MBR has records other than its protective one, as a hybrid MBR does.
    MbrRecords,
    /// LBA 1 lacks the header's signature, `EFI PART`.
    Signature,
    /// The header's revision is this one, rather than 1.0.
    Revision(u32),
    /// The header's size is this one, less than 92 bytes or more than one logical block.
    HeaderSize(u32),
    /// The CRC-32 that the header records differs from the one of its contents.
    HeaderCrc {
        /// The CRC that the header records.
        stored: u32,
        /// The CRC of the header's contents.
        computed: u32,
    },
    /// The header's reserved field holds this value, rather than zero.
    HeaderReserved(u32),
    /// The header's own LBA is this one, rather than 1.
    MyLba(u64),
    /// The partition entries are this many bytes long, rather than [`ENTRY_SIZE`].
    EntrySize(u32),
    /// The header declares this many partition entries: none, or more than
    /// [`MAX_ENTRY_ARRAY_BYTES`] hold.
    EntryCount(u32),
    /// The partition entry array, at this LBA, does not lie after the header and before the
    /// disk's last block.
    EntryArrayLba(u64),
    /// The first usable LBA is this one, which precedes the end of the partition entry array.
    FirstUsableLba(u64),
    /// The last usable LBA is this one, which precedes the first usable LBA or leaves the disk no
    /// block for the alternate header.
    LastUsableLba(u64),
    /// The alternate header's LBA is this one, which is not after the usable blocks and inside
    /// the disk.
    AlternateLba(u64),
    /// The disk GUID is nil.
    NilDiskGuid,
    /// The CRC-32 that the header records for the partition entry array differs from the one of
    /// the array's contents.
    EntryArrayCrc {
        /// The CRC that the header records.
        stored: u32,
        /// The CRC of the array's contents.
        computed: u32,
    },
    /// A used partition entry follows an unused one.
    Hole {
        /// The number of the unused entry, from 1.
        unused: u32,
        /// The number of the used entry after it, from 1.
        used: u32,
    },
    /// The disk has this many partitions, rather than [`MIN_PARTITIONS`] through
    /// [`MAX_PARTITIONS`].
    PartitionCount(usize),
    /// A partition does not span an ascending range of usable blocks.
    Extent {
        /// The partition's number.
        number: u8,
        /// The partition's first LBA.
        first_lba: u64,
        /// The partition's last LBA.
        last_lba: u64,
    },
    /// A partition's unique GUID is nil.
    NilPartitionGuid {
        /// The partition's number.
        number: u8,
    },
    /// A partition's unique GUID repeats the disk GUID or another partition's unique GUID.
    DuplicateGuid {
        /// The partition's number.
        number: u8,
    },
    /// Two partitions share blocks.
    Overlap {
        /// The lower number of the two.
        first: u8,
        /// The higher number of the two.
        second: u8,
    },
    /// The disk GUID differs from the expected one.
    DiskGuidMismatch {
        /// The expected GUID.
        expected: Uuid,
        /// The disk's GUID.
        found: Uuid,
    },
    /// The disk has a different number of partitions than expected.
    PartitionCountMismatch {
        /// The expected number.
        expected: usize,
        /// The disk's number.
        found: usize,
    },
    /// A partition's unique GUID differs from the expected one.
    PartitionGuidMismatch {
        /// The partition's number.
        number: u8,
        /// The expected GUID.
        expected: Uuid,
        /// The partition's GUID.
        found: Uuid,
    },
    /// A partition's size differs from the expected one.
    PartitionSizeMismatch {
        /// The partition's number.
        number: u8,
        /// The expected size, in bytes.
        expected: u64,
        /// The partition's size, in bytes.
        found: u64,
    },
    /// The first partition is larger than the caller's bound.
    FirstPartitionTooLarge {
        /// The size of the first partition, in bytes.
        size: u64,
        /// The caller's bound, in bytes.
        bound: usize,
    },
}

impl fmt::Display for GptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "cannot read the disk: {error}"),
            Self::LogicalBlockSize(size) => write!(
                formatter,
                "a logical block size of {size} bytes is not a power of two from \
                 {MIN_LOGICAL_BLOCK_SIZE} through {MAX_LOGICAL_BLOCK_SIZE}"
            ),
            Self::DiskSize {
                size,
                logical_block_size,
            } => write!(
                formatter,
                "a disk of {size} bytes is not a whole number of at least two \
                 {logical_block_size}-byte blocks"
            ),
            Self::MbrSignature => formatter.write_str("LBA 0 has no MBR signature"),
            Self::NoProtectiveRecord => formatter.write_str(
                "the MBR has no protective record of type 0xEE, so the disk is not a GPT disk",
            ),
            Self::ProtectiveRecordStart(lba) => write!(
                formatter,
                "the MBR's protective record starts at LBA {lba}, not 1"
            ),
            Self::MbrRecords => {
                formatter.write_str("the MBR has records besides its one protective record")
            }
            Self::Signature => formatter.write_str("LBA 1 has no GPT header signature"),
            Self::Revision(revision) => write!(
                formatter,
                "the GPT header's revision is {revision:#010x}, not 1.0 (0x00010000)"
            ),
            Self::HeaderSize(size) => write!(
                formatter,
                "the GPT header's size of {size} bytes is less than {HEADER_FIELDS_SIZE} or more \
                 than one logical block"
            ),
            Self::HeaderCrc { stored, computed } => write!(
                formatter,
                "the GPT header records the CRC-32 {stored:#010x}, but its contents have \
                 {computed:#010x}"
            ),
            Self::HeaderReserved(value) => write!(
                formatter,
                "the GPT header's reserved field is {value:#010x}, not zero"
            ),
            Self::MyLba(lba) => write!(
                formatter,
                "the primary GPT header records its own LBA as {lba}, not 1"
            ),
            Self::EntrySize(size) => write!(
                formatter,
                "the partition entries are {size} bytes long, not {ENTRY_SIZE}"
            ),
            Self::EntryCount(count) => write!(
                formatter,
                "the GPT header declares {count} partition entries, not 1 through {}",
                MAX_ENTRY_ARRAY_BYTES / ENTRY_SIZE
            ),
            Self::EntryArrayLba(lba) => write!(
                formatter,
                "the partition entry array at LBA {lba} does not lie after the GPT header and \
                 before the disk's last block"
            ),
            Self::FirstUsableLba(lba) => write!(
                formatter,
                "the first usable LBA, {lba}, precedes the end of the partition entry array"
            ),
            Self::LastUsableLba(lba) => write!(
                formatter,
                "the last usable LBA, {lba}, precedes the first usable LBA or leaves no block \
                 for the alternate GPT header"
            ),
            Self::AlternateLba(lba) => write!(
                formatter,
                "the alternate GPT header's LBA, {lba}, is not after the usable blocks and \
                 inside the disk"
            ),
            Self::NilDiskGuid => formatter.write_str("the disk GUID is nil"),
            Self::EntryArrayCrc { stored, computed } => write!(
                formatter,
                "the GPT header records the CRC-32 {stored:#010x} for the partition entry \
                 array, but its contents have {computed:#010x}"
            ),
            Self::Hole { unused, used } => write!(
                formatter,
                "partition entry {used} is used, but entry {unused} before it is not"
            ),
            Self::PartitionCount(count) => write!(
                formatter,
                "the disk has {count} partitions, not {MIN_PARTITIONS} through {MAX_PARTITIONS}: \
                 the first partition and at least one layer"
            ),
            Self::Extent {
                number,
                first_lba,
                last_lba,
            } => write!(
                formatter,
                "partition {number}'s blocks, {first_lba} through {last_lba}, are not an \
                 ascending range of usable blocks"
            ),
            Self::NilPartitionGuid { number } => {
                write!(formatter, "partition {number}'s unique GUID is nil")
            }
            Self::DuplicateGuid { number } => write!(
                formatter,
                "partition {number}'s unique GUID repeats another GUID of the disk"
            ),
            Self::Overlap { first, second } => {
                write!(formatter, "partitions {first} and {second} share blocks")
            }
            Self::DiskGuidMismatch { expected, found } => write!(
                formatter,
                "the disk GUID is {found}, not the expected {expected}"
            ),
            Self::PartitionCountMismatch { expected, found } => write!(
                formatter,
                "the disk has {found} partitions, not the expected {expected}"
            ),
            Self::PartitionGuidMismatch {
                number,
                expected,
                found,
            } => write!(
                formatter,
                "partition {number}'s unique GUID is {found}, not the expected {expected}"
            ),
            Self::PartitionSizeMismatch {
                number,
                expected,
                found,
            } => write!(
                formatter,
                "partition {number} has {found} bytes, not the expected {expected}"
            ),
            Self::FirstPartitionTooLarge { size, bound } => write!(
                formatter,
                "the first partition has {size} bytes, more than the bound of {bound}"
            ),
        }
    }
}

impl std::error::Error for GptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for GptError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
