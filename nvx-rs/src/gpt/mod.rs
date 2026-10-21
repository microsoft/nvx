//! GUID Partition Table (GPT) disks that hold a layered root: a first partition, p1, whose bytes
//! configure an adapter, and the read-only filesystem layers of the root in the partitions after
//! it.
//!
//! [`Disk::read`] reads a disk's protective MBR, primary GPT header, and primary partition entry
//! array through a [`ReadAt`] source, and checks them strictly, as UEFI 2.10 specifies them and
//! as Linux's own GPT reader requires them, so that a disk that Linux would not read as this
//! table fails here, early and clearly:
//!
//! - **Protective MBR:** LBA 0 has the MBR signature and exactly one partition record, of type
//!   `0xEE` and starting at LBA 1, and its other three records are zero, so a hybrid MBR fails.
//!   Linux reads no GPT without a protective record.
//! - **Header:** LBA 1 holds the signature `EFI PART`, revision 1.0, a size from 92 bytes through
//!   one logical block, a matching CRC-32, a zero reserved field, and its own LBA, 1. The entry
//!   array starts after the header and ends before the first usable LBA, the usable blocks end
//!   before the alternate header, and the alternate header lies inside the disk.
//! - **Entry array:** entries of [`ENTRY_SIZE`] bytes, the only size that Linux reads, at most
//!   [`MAX_ENTRY_ARRAY_BYTES`] of them in all, and a matching CRC-32.
//! - **Partitions:** the used entries, those whose type GUID is not nil, come first, with no unused
//!   entry among them, and number from [`MIN_PARTITIONS`], p1 and at least one layer, through
//!   [`MAX_PARTITIONS`], the most that Linux creates. Each lies in the usable blocks, shares no
//!   block with another, and has a unique GUID that is not nil and repeats no other GUID of the
//!   disk. Linux numbers each partition one more than its entry's index, as [`Partition::number`]
//!   does.
//!
//! The backup header and entry array are never read: a disk whose primary table is invalid fails
//! even when its backup is intact. Linux, too, falls back to the backup only when its command
//! line forces GPT.
//!
//! The work is bounded. The parser reads the 512 bytes of the MBR, one block for the header, and
//! the entry array, which it allocates only once the header has bounded its size, and computes
//! positions with checked arithmetic, in the disk's logical blocks. [`Expected`] adds the
//! identities and sizes that a caller knows, such as the disk GUID that a boot parameter names.
//! [`Disk::read_p1`] then reads p1 as opaque bytes, up to a bound that the caller sets, which it
//! neither interprets nor probes as a filesystem.
//!
//! GUIDs are [`Uuid`]s, which GPT stores in the layout that [`Uuid::from_bytes_le`] reads.
//!
//! The module makes no I/O other than the reads of its source, and depends on nothing outside the
//! standard library, so a guest that validates a block device and a host that checks an image
//! that it stages share it.
//!
//! ```no_run
//! use std::fs::File;
//!
//! use nvx::gpt::{Disk, Expected};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let image = File::open("disk.img")?;
//! let disk = Disk::read(&image, 512, image.metadata()?.len(), &Expected::new())?;
//! let configuration = disk.read_p1(&image, 64 * 1024)?;
//! println!("p1 holds {} bytes of configuration", configuration.len());
//! for layer in disk.lowers() {
//!     println!("p{}: {} bytes at {}", layer.number(), layer.size(), layer.offset());
//! }
//! # Ok(())
//! # }
//! ```

mod crc;
mod error;
mod source;
#[cfg(test)]
pub(crate) mod tests;

use crc::{Crc32, crc32};
pub use error::GptError;
pub use source::ReadAt;

use crate::uuid::Uuid;

/// The smallest logical block size of a disk, in bytes.
pub const MIN_LOGICAL_BLOCK_SIZE: u32 = 512;

/// The largest logical block size of a disk, in bytes: the largest that Linux gives a block
/// device.
pub const MAX_LOGICAL_BLOCK_SIZE: u32 = 64 * 1024;

/// The size of a partition entry, in bytes: the only one that Linux reads, and the smallest that
/// UEFI allows.
pub const ENTRY_SIZE: u32 = 128;

/// The largest partition entry array that a disk may declare, in bytes, which bounds the array's
/// allocation and the work of its CRC: 8,192 entries. UEFI's default array holds 128.
pub const MAX_ENTRY_ARRAY_BYTES: u32 = 1024 * 1024;

/// The fewest partitions of a disk: p1 and one layer.
pub const MIN_PARTITIONS: usize = 2;

/// The most partitions of a disk: Linux creates partitions 1 through 255 only.
pub const MAX_PARTITIONS: usize = 255;

/// The size of the MBR at the start of LBA 0, in bytes, whatever the logical block size.
const MBR_SIZE: usize = 512;

/// The offset of the MBR's four partition records.
const MBR_RECORDS_OFFSET: usize = 446;

/// The size of an MBR partition record.
const MBR_RECORD_SIZE: usize = 16;

/// The offset of the MBR's signature, its last two bytes.
const MBR_SIGNATURE_OFFSET: usize = 510;

/// The MBR's signature.
const MBR_SIGNATURE: [u8; 2] = [0x55, 0xAA];

/// The offset of an MBR partition record's type.
const MBR_RECORD_TYPE_OFFSET: usize = 4;

/// The offset of an MBR partition record's starting LBA.
const MBR_RECORD_START_OFFSET: usize = 8;

/// The MBR partition type of a protective record.
const PROTECTIVE_TYPE: u8 = 0xEE;

/// The LBA of the primary header, which a protective record starts at.
const PRIMARY_HEADER_LBA: u64 = 1;

/// The header's signature.
const SIGNATURE: [u8; 8] = *b"EFI PART";

/// The header revision that UEFI defines: 1.0.
const REVISION: u32 = 0x0001_0000;

/// The size of the header's fields, in bytes: the smallest header. A header may be larger, up
/// to one logical block, and its CRC covers its whole size.
const HEADER_FIELDS_SIZE: u32 = 92;

/// The offsets of the fields of a GPT header, which UEFI 2.10 table 5.5 defines.
mod header {
    /// The signature, 8 bytes.
    pub(super) const SIGNATURE: usize = 0;
    /// The revision, 4 bytes.
    pub(super) const REVISION: usize = 8;
    /// The header's size, 4 bytes.
    pub(super) const SIZE: usize = 12;
    /// The header's CRC-32, 4 bytes, which the CRC covers as zero.
    pub(super) const CRC: usize = 16;
    /// The reserved field, 4 bytes.
    pub(super) const RESERVED: usize = 20;
    /// The header's own LBA, 8 bytes.
    pub(super) const MY_LBA: usize = 24;
    /// The alternate header's LBA, 8 bytes.
    pub(super) const ALTERNATE_LBA: usize = 32;
    /// The first usable LBA, 8 bytes.
    pub(super) const FIRST_USABLE_LBA: usize = 40;
    /// The last usable LBA, 8 bytes.
    pub(super) const LAST_USABLE_LBA: usize = 48;
    /// The disk GUID, 16 bytes.
    pub(super) const DISK_GUID: usize = 56;
    /// The LBA of the partition entry array, 8 bytes.
    pub(super) const ENTRY_LBA: usize = 72;
    /// The number of partition entries, 4 bytes.
    pub(super) const ENTRY_COUNT: usize = 80;
    /// The size of a partition entry, 4 bytes.
    pub(super) const ENTRY_SIZE: usize = 84;
    /// The CRC-32 of the partition entry array, 4 bytes.
    pub(super) const ENTRY_ARRAY_CRC: usize = 88;
}

/// The offsets of the fields of a GPT partition entry, which UEFI 2.10 table 5.6 defines.
mod entry {
    /// The partition type GUID, 16 bytes; nil in an unused entry.
    pub(super) const TYPE_GUID: usize = 0;
    /// The unique partition GUID, 16 bytes.
    pub(super) const UNIQUE_GUID: usize = 16;
    /// The first LBA, 8 bytes.
    pub(super) const FIRST_LBA: usize = 32;
    /// The last LBA, inclusive, 8 bytes.
    pub(super) const LAST_LBA: usize = 40;
    /// The attributes, 8 bytes.
    pub(super) const ATTRIBUTES: usize = 48;
}

/// What a caller knows of a disk beforehand: its GUID, and the unique GUIDs and sizes of its
/// partitions.
///
/// Every expectation is optional. [`Disk::read`] refuses a disk that differs from one that is
/// set, once the disk has passed the format's own checks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expected {
    /// The disk's GUID, if the caller expects one.
    disk_guid: Option<Uuid>,
    /// The partitions, p1 first, if the caller expects them.
    partitions: Option<Vec<ExpectedPartition>>,
}

impl Expected {
    /// Expects nothing beyond the format.
    pub fn new() -> Self {
        Self::default()
    }

    /// Expects the disk's GUID to be `guid`.
    pub fn disk_guid(mut self, guid: Uuid) -> Self {
        self.disk_guid = Some(guid);
        self
    }

    /// Expects exactly as many partitions as `partitions` holds, p1 first, and each to have the
    /// unique GUID and size that its expectation sets.
    pub fn partitions(mut self, partitions: Vec<ExpectedPartition>) -> Self {
        self.partitions = Some(partitions);
        self
    }

    /// Returns the first expectation that `disk` fails, if any.
    fn check(&self, disk: &Disk) -> Result<(), GptError> {
        if let Some(expected) = self.disk_guid
            && expected != disk.guid
        {
            return Err(GptError::DiskGuidMismatch {
                expected,
                found: disk.guid,
            });
        }
        let Some(partitions) = &self.partitions else {
            return Ok(());
        };
        if partitions.len() != disk.partitions.len() {
            return Err(GptError::PartitionCountMismatch {
                expected: partitions.len(),
                found: disk.partitions.len(),
            });
        }
        for (expected, partition) in partitions.iter().zip(&disk.partitions) {
            if let Some(guid) = expected.unique_guid
                && guid != partition.unique_guid
            {
                return Err(GptError::PartitionGuidMismatch {
                    number: partition.number,
                    expected: guid,
                    found: partition.unique_guid,
                });
            }
            if let Some(size) = expected.size
                && size != partition.size
            {
                return Err(GptError::PartitionSizeMismatch {
                    number: partition.number,
                    expected: size,
                    found: partition.size,
                });
            }
        }
        Ok(())
    }
}

/// What a caller knows of one partition beforehand. Every expectation is optional.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExpectedPartition {
    /// The partition's unique GUID, if the caller expects one.
    unique_guid: Option<Uuid>,
    /// The partition's size in bytes, if the caller expects one.
    size: Option<u64>,
}

impl ExpectedPartition {
    /// Expects nothing of the partition but that it exists.
    pub fn new() -> Self {
        Self::default()
    }

    /// Expects the partition's unique GUID to be `guid`.
    pub fn unique_guid(mut self, guid: Uuid) -> Self {
        self.unique_guid = Some(guid);
        self
    }

    /// Expects the partition to be `bytes` long.
    pub fn size(mut self, bytes: u64) -> Self {
        self.size = Some(bytes);
        self
    }
}

/// A GPT disk whose protective MBR, primary header, and primary partition entry array
/// [`Disk::read`] checked, and its partitions: p1 and the layers after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    /// The size of a logical block, in bytes.
    logical_block_size: u32,
    /// The disk's size, in bytes.
    size: u64,
    /// The disk's GUID.
    guid: Uuid,
    /// The partitions, in the order of their numbers, which run from 1 without a gap.
    partitions: Vec<Partition>,
}

impl Disk {
    /// Reads and checks the disk that `source` holds, whose logical blocks have
    /// `logical_block_size` bytes and which has `size` bytes in all, as the module describes.
    ///
    /// The caller states the geometry as the device reports it, so that the checks bound every
    /// position to the disk. The partitions must also meet `expected`.
    pub fn read<S: ReadAt + ?Sized>(
        source: &S,
        logical_block_size: u32,
        size: u64,
        expected: &Expected,
    ) -> Result<Self, GptError> {
        let geometry = Geometry::new(logical_block_size, size)?;
        check_protective_mbr(source)?;
        let header = read_header(source, geometry)?;
        let partitions = read_partitions(source, geometry, &header)?;
        let disk = Self {
            logical_block_size,
            size,
            guid: header.disk_guid,
            partitions,
        };
        expected.check(&disk)?;
        Ok(disk)
    }

    /// Returns the size of a logical block, in bytes.
    pub fn logical_block_size(&self) -> u32 {
        self.logical_block_size
    }

    /// Returns the disk's size, in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns the disk's GUID.
    pub fn guid(&self) -> Uuid {
        self.guid
    }

    /// Returns every partition, p1 first, in the order of their numbers, which run from 1
    /// without a gap.
    pub fn partitions(&self) -> &[Partition] {
        &self.partitions
    }

    /// Returns p1, the partition whose bytes configure the adapter.
    pub fn p1(&self) -> &Partition {
        &self.partitions[0]
    }

    /// Returns the partitions after p1, which hold the read-only layers of the root: at least
    /// one.
    pub fn lowers(&self) -> &[Partition] {
        &self.partitions[1..]
    }

    /// Reads p1 from `source`, the disk that [`Disk::read`] read: every byte of the partition,
    /// less the trailing NUL bytes and ASCII whitespace (space, tab, line feed, form feed, and
    /// carriage return), as opaque bytes that need not be UTF-8.
    ///
    /// A p1 larger than `bound` bytes fails before any of it is read, so that no more than
    /// `bound` bytes are read or allocated.
    pub fn read_p1<S: ReadAt + ?Sized>(
        &self,
        source: &S,
        bound: usize,
    ) -> Result<Vec<u8>, GptError> {
        let p1 = self.p1();
        let length = usize::try_from(p1.size)
            .ok()
            .filter(|length| *length <= bound)
            .ok_or(GptError::P1TooLarge {
                size: p1.size,
                bound,
            })?;
        let mut bytes = vec![0; length];
        source.read_exact_at(p1.offset, &mut bytes)?;
        let end = bytes
            .iter()
            .rposition(|byte| *byte != 0 && !byte.is_ascii_whitespace())
            .map_or(0, |last| last + 1);
        bytes.truncate(end);
        Ok(bytes)
    }
}

/// A partition of a [`Disk`]: a used partition entry, and the blocks that it spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// The partition's number, one more than its entry's index.
    number: u8,
    /// The partition type GUID.
    type_guid: Uuid,
    /// The unique partition GUID.
    unique_guid: Uuid,
    /// The attribute bits, which the parser does not interpret.
    attributes: u64,
    /// The first logical block of the partition.
    first_lba: u64,
    /// The last logical block of the partition, inclusive.
    last_lba: u64,
    /// The byte offset of the partition in the disk.
    offset: u64,
    /// The partition's size, in bytes.
    size: u64,
}

impl Partition {
    /// Returns the partition's number, as Linux numbers it: one more than its entry's index, so
    /// p1 is 1, and the first layer 2.
    pub fn number(&self) -> u8 {
        self.number
    }

    /// Returns the partition type GUID.
    pub fn type_guid(&self) -> Uuid {
        self.type_guid
    }

    /// Returns the unique partition GUID.
    pub fn unique_guid(&self) -> Uuid {
        self.unique_guid
    }

    /// Returns the partition's attribute bits, which the parser does not interpret.
    pub fn attributes(&self) -> u64 {
        self.attributes
    }

    /// Returns the partition's first logical block.
    pub fn first_lba(&self) -> u64 {
        self.first_lba
    }

    /// Returns the partition's last logical block, which it includes.
    pub fn last_lba(&self) -> u64 {
        self.last_lba
    }

    /// Returns the partition's byte offset in the disk.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the partition's size, in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// A disk's logical block size and number of blocks, as its caller states them.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    /// The size of a logical block, in bytes: a power of two in the supported range.
    block_size: u64,
    /// The number of logical blocks: at least two, for the MBR and the header.
    blocks: u64,
}

impl Geometry {
    /// Checks the block size and the disk's size, in bytes.
    fn new(logical_block_size: u32, size: u64) -> Result<Self, GptError> {
        if !logical_block_size.is_power_of_two()
            || !(MIN_LOGICAL_BLOCK_SIZE..=MAX_LOGICAL_BLOCK_SIZE).contains(&logical_block_size)
        {
            return Err(GptError::LogicalBlockSize(logical_block_size));
        }
        let block_size = u64::from(logical_block_size);
        let blocks = size / block_size;
        if !size.is_multiple_of(block_size) || blocks <= PRIMARY_HEADER_LBA {
            return Err(GptError::DiskSize {
                size,
                logical_block_size,
            });
        }
        Ok(Self { block_size, blocks })
    }

    /// Returns the disk's last LBA.
    fn last_lba(self) -> u64 {
        self.blocks - 1
    }

    /// Returns the size of `blocks` logical blocks in bytes, or `None` if it overflows.
    fn bytes(self, blocks: u64) -> Option<u64> {
        blocks.checked_mul(self.block_size)
    }
}

/// The fields of the primary header that the parser uses, once they have passed its checks.
#[derive(Debug)]
struct Header {
    /// The first usable LBA, which follows the entry array.
    first_usable_lba: u64,
    /// The last usable LBA, which precedes the alternate header.
    last_usable_lba: u64,
    /// The disk GUID, which is not nil.
    disk_guid: Uuid,
    /// The byte offset of the partition entry array.
    entry_array_offset: u64,
    /// The size of the partition entry array in bytes, at most [`MAX_ENTRY_ARRAY_BYTES`].
    entry_array_size: usize,
    /// The CRC-32 that the header records for the partition entry array.
    entry_array_crc: u32,
}

/// Checks the protective MBR in the first 512 bytes of LBA 0: its signature, and one protective
/// record, which starts at LBA 1, beside three zero records.
fn check_protective_mbr<S: ReadAt + ?Sized>(source: &S) -> Result<(), GptError> {
    let mut mbr = [0; MBR_SIZE];
    source.read_exact_at(0, &mut mbr)?;
    if mbr[MBR_SIGNATURE_OFFSET..] != MBR_SIGNATURE {
        return Err(GptError::MbrSignature);
    }
    let records: Vec<&[u8]> = mbr[MBR_RECORDS_OFFSET..MBR_SIGNATURE_OFFSET]
        .chunks_exact(MBR_RECORD_SIZE)
        .collect();
    let protective = records
        .iter()
        .position(|record| record[MBR_RECORD_TYPE_OFFSET] == PROTECTIVE_TYPE)
        .ok_or(GptError::NoProtectiveRecord)?;
    let start = u32_at(records[protective], MBR_RECORD_START_OFFSET);
    if u64::from(start) != PRIMARY_HEADER_LBA {
        return Err(GptError::ProtectiveRecordStart(start));
    }
    let others = records
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != protective);
    if others
        .flat_map(|(_, record)| record.iter())
        .any(|byte| *byte != 0)
    {
        return Err(GptError::MbrRecords);
    }
    Ok(())
}

/// Reads and checks the primary header in LBA 1, and the layout that it describes:
///
/// `2 <= entry LBA`, `entry LBA + entry blocks <= first usable <= last usable < alternate <=
/// last LBA`.
fn read_header<S: ReadAt + ?Sized>(source: &S, geometry: Geometry) -> Result<Header, GptError> {
    // The block size is at most 64 KiB, so it fits a `usize`.
    let mut block = vec![0; geometry.block_size as usize];
    source.read_exact_at(geometry.block_size, &mut block)?;
    if block[header::SIGNATURE..header::REVISION] != SIGNATURE {
        return Err(GptError::Signature);
    }
    let revision = u32_at(&block, header::REVISION);
    if revision != REVISION {
        return Err(GptError::Revision(revision));
    }
    let header_size = u32_at(&block, header::SIZE);
    if !(HEADER_FIELDS_SIZE..=geometry.block_size as u32).contains(&header_size) {
        return Err(GptError::HeaderSize(header_size));
    }
    let stored = u32_at(&block, header::CRC);
    let computed = Crc32::new()
        .update(&block[..header::CRC])
        .update(&[0; 4])
        .update(&block[header::RESERVED..header_size as usize])
        .finish();
    if stored != computed {
        return Err(GptError::HeaderCrc { stored, computed });
    }
    let reserved = u32_at(&block, header::RESERVED);
    if reserved != 0 {
        return Err(GptError::HeaderReserved(reserved));
    }
    let my_lba = u64_at(&block, header::MY_LBA);
    if my_lba != PRIMARY_HEADER_LBA {
        return Err(GptError::MyLba(my_lba));
    }

    let entry_size = u32_at(&block, header::ENTRY_SIZE);
    if entry_size != ENTRY_SIZE {
        return Err(GptError::EntrySize(entry_size));
    }
    let entry_count = u32_at(&block, header::ENTRY_COUNT);
    let entry_array_size = u64::from(entry_count) * u64::from(ENTRY_SIZE);
    if entry_count == 0 || entry_array_size > u64::from(MAX_ENTRY_ARRAY_BYTES) {
        return Err(GptError::EntryCount(entry_count));
    }
    let entry_lba = u64_at(&block, header::ENTRY_LBA);
    let entry_end = entry_lba
        .checked_add(entry_array_size.div_ceil(geometry.block_size))
        .filter(|end| entry_lba > PRIMARY_HEADER_LBA && *end <= geometry.last_lba())
        .ok_or(GptError::EntryArrayLba(entry_lba))?;
    let first_usable_lba = u64_at(&block, header::FIRST_USABLE_LBA);
    if first_usable_lba < entry_end {
        return Err(GptError::FirstUsableLba(first_usable_lba));
    }
    let last_usable_lba = u64_at(&block, header::LAST_USABLE_LBA);
    if last_usable_lba < first_usable_lba || last_usable_lba >= geometry.last_lba() {
        return Err(GptError::LastUsableLba(last_usable_lba));
    }
    let alternate_lba = u64_at(&block, header::ALTERNATE_LBA);
    if alternate_lba <= last_usable_lba || alternate_lba > geometry.last_lba() {
        return Err(GptError::AlternateLba(alternate_lba));
    }
    let disk_guid = guid_at(&block, header::DISK_GUID);
    if disk_guid.is_nil() {
        return Err(GptError::NilDiskGuid);
    }
    Ok(Header {
        first_usable_lba,
        last_usable_lba,
        disk_guid,
        // The array lies before the last LBA, so its offset is less than the disk's size.
        entry_array_offset: geometry
            .bytes(entry_lba)
            .ok_or(GptError::EntryArrayLba(entry_lba))?,
        // At most `MAX_ENTRY_ARRAY_BYTES`, so it fits a `usize`.
        entry_array_size: entry_array_size as usize,
        entry_array_crc: u32_at(&block, header::ENTRY_ARRAY_CRC),
    })
}

/// Reads and checks the primary partition entry array that `header` describes, and returns its
/// partitions.
fn read_partitions<S: ReadAt + ?Sized>(
    source: &S,
    geometry: Geometry,
    header: &Header,
) -> Result<Vec<Partition>, GptError> {
    let mut array = vec![0; header.entry_array_size];
    source.read_exact_at(header.entry_array_offset, &mut array)?;
    let computed = crc32(&array);
    if computed != header.entry_array_crc {
        return Err(GptError::EntryArrayCrc {
            stored: header.entry_array_crc,
            computed,
        });
    }

    let entries = array.chunks_exact(ENTRY_SIZE as usize);
    let mut used = 0;
    let mut unused = None;
    // Entry numbers start at 1, as partition numbers do, and the array has at most 8,192.
    for (number, raw) in (1..).zip(entries.clone()) {
        if guid_at(raw, entry::TYPE_GUID).is_nil() {
            unused.get_or_insert(number);
        } else if let Some(unused) = unused {
            return Err(GptError::Hole {
                unused,
                used: number,
            });
        } else {
            used += 1;
        }
    }
    if !(MIN_PARTITIONS..=MAX_PARTITIONS).contains(&used) {
        return Err(GptError::PartitionCount(used));
    }

    // At most 256 GUIDs, compared without a hasher, whose random keys an early-boot caller might
    // not want to draw.
    let mut guids = Vec::with_capacity(used + 1);
    guids.push(header.disk_guid);
    let mut partitions = Vec::with_capacity(used);
    for (number, raw) in (1..=u8::MAX).zip(entries.take(used)) {
        let first_lba = u64_at(raw, entry::FIRST_LBA);
        let last_lba = u64_at(raw, entry::LAST_LBA);
        let extent = GptError::Extent {
            number,
            first_lba,
            last_lba,
        };
        if first_lba < header.first_usable_lba
            || last_lba > header.last_usable_lba
            || last_lba < first_lba
        {
            return Err(extent);
        }
        let unique_guid = guid_at(raw, entry::UNIQUE_GUID);
        if unique_guid.is_nil() {
            return Err(GptError::NilPartitionGuid { number });
        }
        if guids.contains(&unique_guid) {
            return Err(GptError::DuplicateGuid { number });
        }
        guids.push(unique_guid);
        // The partition lies in the usable blocks, so neither its offset nor its size exceeds
        // the disk's size.
        let (Some(offset), Some(size)) = (
            geometry.bytes(first_lba),
            geometry.bytes(last_lba - first_lba + 1),
        ) else {
            return Err(extent);
        };
        partitions.push(Partition {
            number,
            type_guid: guid_at(raw, entry::TYPE_GUID),
            unique_guid,
            attributes: u64_at(raw, entry::ATTRIBUTES),
            first_lba,
            last_lba,
            offset,
            size,
        });
    }

    let mut ordered: Vec<&Partition> = partitions.iter().collect();
    ordered.sort_unstable_by_key(|partition| partition.first_lba);
    for pair in ordered.windows(2) {
        if pair[1].first_lba <= pair[0].last_lba {
            let (first, second) = (pair[0].number, pair[1].number);
            return Err(GptError::Overlap {
                first: first.min(second),
                second: first.max(second),
            });
        }
    }
    Ok(partitions)
}

/// Returns the little-endian `u32` at `offset` of `bytes`, which holds it.
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    let mut field = [0; 4];
    field.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(field)
}

/// Returns the little-endian `u64` at `offset` of `bytes`, which holds it.
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    let mut field = [0; 8];
    field.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(field)
}

/// Returns the GUID at `offset` of `bytes`, which holds it in the layout of EFI GUIDs.
fn guid_at(bytes: &[u8], offset: usize) -> Uuid {
    let mut field = [0; 16];
    field.copy_from_slice(&bytes[offset..offset + 16]);
    Uuid::from_bytes_le(field)
}
