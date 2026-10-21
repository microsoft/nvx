//! Tests of the GPT reader against disks that the tests write field by field.

use std::cell::RefCell;
use std::io;

use super::crc::crc32;
use super::*;

/// The Linux filesystem data partition type GUID, `0fc63daf-8483-4772-8e79-3d69d8477de4`, as GPT
/// stores it.
const LINUX_DATA: Uuid = Uuid::from_bytes_le([
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
]);

/// The disk GUID of the tests' disks.
const DISK_GUID: Uuid = guid(0xD1);

/// Returns a GUID whose 16 bytes are all `byte`, which is not zero.
const fn guid(byte: u8) -> Uuid {
    Uuid::from_bytes([byte; 16])
}

/// A used partition entry that a test writes.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// The partition type GUID.
    type_guid: Uuid,
    /// The unique partition GUID.
    unique_guid: Uuid,
    /// The first LBA.
    first_lba: u64,
    /// The last LBA, inclusive.
    last_lba: u64,
    /// The attribute bits.
    attributes: u64,
}

impl Entry {
    /// Returns a Linux data partition from `first_lba` through `last_lba` whose unique GUID's
    /// bytes are all `id`.
    fn new(id: u8, first_lba: u64, last_lba: u64) -> Self {
        Self {
            type_guid: LINUX_DATA,
            unique_guid: guid(id),
            first_lba,
            last_lba,
            attributes: 0,
        }
    }

    /// Writes the entry's 128 bytes.
    fn write(&self, bytes: &mut [u8]) {
        bytes[entry::TYPE_GUID..][..16].copy_from_slice(&self.type_guid.to_bytes_le());
        bytes[entry::UNIQUE_GUID..][..16].copy_from_slice(&self.unique_guid.to_bytes_le());
        bytes[entry::FIRST_LBA..][..8].copy_from_slice(&self.first_lba.to_le_bytes());
        bytes[entry::LAST_LBA..][..8].copy_from_slice(&self.last_lba.to_le_bytes());
        bytes[entry::ATTRIBUTES..][..8].copy_from_slice(&self.attributes.to_le_bytes());
    }
}

/// Every field of a test disk. [`Spec::image`] writes it with the CRCs of its contents, unless a
/// test sets them, and with a backup header and array that match the primary ones.
#[derive(Clone, Debug)]
struct Spec {
    /// The logical block size.
    block_size: u32,
    /// The number of blocks.
    blocks: u64,
    /// The MBR's four partition records, 16 raw bytes each.
    mbr_records: [[u8; 16]; 4],
    /// The MBR's signature.
    mbr_signature: [u8; 2],
    /// The header's signature.
    signature: [u8; 8],
    /// The header's revision.
    revision: u32,
    /// The header's size.
    header_size: u32,
    /// The header's CRC, or `None` for the CRC of its contents.
    header_crc: Option<u32>,
    /// The header's reserved field.
    reserved: u32,
    /// The header's own LBA.
    my_lba: u64,
    /// The alternate header's LBA.
    alternate_lba: u64,
    /// The first usable LBA.
    first_usable_lba: u64,
    /// The last usable LBA.
    last_usable_lba: u64,
    /// The disk GUID.
    disk_guid: Uuid,
    /// The LBA of the partition entry array.
    entry_lba: u64,
    /// The number of partition entries.
    entry_count: u32,
    /// The size of a partition entry that the header records; entries are always written 128
    /// bytes apart.
    entry_size: u32,
    /// The entry array's CRC, or `None` for the CRC of its contents.
    entry_array_crc: Option<u32>,
    /// The partition entries by index; `None` leaves an entry unused.
    entries: Vec<Option<Entry>>,
    /// Bytes written at the start of a block, such as p1's contents.
    contents: Vec<(u64, Vec<u8>)>,
}

impl Spec {
    /// Returns a valid disk of `blocks` blocks of `block_size` bytes, whose `entry_count`
    /// entries start at LBA 2, as do UEFI's defaults, and with three partitions: p1, one block,
    /// and two layers of 8 and 16 blocks after it.
    fn new(block_size: u32, blocks: u64, entry_count: u32) -> Self {
        let array_blocks = (u64::from(entry_count) * 128).div_ceil(u64::from(block_size));
        let first_usable_lba = 2 + array_blocks;
        let mut protective = [0; 16];
        protective[MBR_RECORD_TYPE_OFFSET] = PROTECTIVE_TYPE;
        protective[MBR_RECORD_START_OFFSET..][..4].copy_from_slice(&1u32.to_le_bytes());
        let size_in_lba = u32::try_from(blocks - 1).unwrap_or(u32::MAX);
        protective[12..].copy_from_slice(&size_in_lba.to_le_bytes());
        Self {
            block_size,
            blocks,
            mbr_records: [protective, [0; 16], [0; 16], [0; 16]],
            mbr_signature: MBR_SIGNATURE,
            signature: SIGNATURE,
            revision: REVISION,
            header_size: HEADER_FIELDS_SIZE,
            header_crc: None,
            reserved: 0,
            my_lba: 1,
            alternate_lba: blocks - 1,
            first_usable_lba,
            last_usable_lba: blocks - 2 - array_blocks,
            disk_guid: DISK_GUID,
            entry_lba: 2,
            entry_count,
            entry_size: ENTRY_SIZE,
            entry_array_crc: None,
            entries: vec![
                Some(Entry::new(1, first_usable_lba, first_usable_lba)),
                Some(Entry::new(2, first_usable_lba + 1, first_usable_lba + 8)),
                Some(Entry::new(3, first_usable_lba + 9, first_usable_lba + 24)),
            ],
            contents: Vec::new(),
        }
    }

    /// Returns [`Spec::new`]'s disk of 2,048 512-byte blocks with 128 entries: entries in LBAs
    /// 2 through 33, usable blocks 34 through 2,014, and the backup array and header after them.
    fn standard() -> Self {
        Self::new(512, 2048, 128)
    }

    /// Returns the used entry `number`, which a test changes.
    fn entry(&mut self, number: usize) -> &mut Entry {
        self.entries[number - 1].as_mut().unwrap()
    }

    /// Returns the entry array: the entries 128 bytes apart, for at most 8,193 entries, more
    /// than any that the reader reads.
    fn array(&self) -> Vec<u8> {
        let count = self.entry_count.min(MAX_ENTRY_ARRAY_BYTES / ENTRY_SIZE + 1) as usize;
        let mut array = vec![0; count * 128];
        for (index, entry) in self.entries.iter().enumerate().take(count) {
            if let Some(entry) = entry {
                entry.write(&mut array[index * 128..][..128]);
            }
        }
        array
    }

    /// Returns the block of a header at `my_lba` whose alternate is at `alternate_lba` and whose
    /// entries start at `entry_lba`, with `array_crc`.
    fn header(&self, my_lba: u64, alternate_lba: u64, entry_lba: u64, array_crc: u32) -> Vec<u8> {
        let mut block = vec![0; self.block_size as usize];
        block[header::SIGNATURE..][..8].copy_from_slice(&self.signature);
        block[header::REVISION..][..4].copy_from_slice(&self.revision.to_le_bytes());
        block[header::SIZE..][..4].copy_from_slice(&self.header_size.to_le_bytes());
        block[header::RESERVED..][..4].copy_from_slice(&self.reserved.to_le_bytes());
        block[header::MY_LBA..][..8].copy_from_slice(&my_lba.to_le_bytes());
        block[header::ALTERNATE_LBA..][..8].copy_from_slice(&alternate_lba.to_le_bytes());
        block[header::FIRST_USABLE_LBA..][..8]
            .copy_from_slice(&self.first_usable_lba.to_le_bytes());
        block[header::LAST_USABLE_LBA..][..8].copy_from_slice(&self.last_usable_lba.to_le_bytes());
        block[header::DISK_GUID..][..16].copy_from_slice(&self.disk_guid.to_bytes_le());
        block[header::ENTRY_LBA..][..8].copy_from_slice(&entry_lba.to_le_bytes());
        block[header::ENTRY_COUNT..][..4].copy_from_slice(&self.entry_count.to_le_bytes());
        block[header::ENTRY_SIZE..][..4].copy_from_slice(&self.entry_size.to_le_bytes());
        block[header::ENTRY_ARRAY_CRC..][..4].copy_from_slice(&array_crc.to_le_bytes());
        let covered = (self.header_size as usize).clamp(header::RESERVED, block.len());
        let crc = self.header_crc.unwrap_or_else(|| crc32(&block[..covered]));
        block[header::CRC..][..4].copy_from_slice(&crc.to_le_bytes());
        block
    }

    /// Writes the disk.
    fn image(&self) -> Vec<u8> {
        let block_size = self.block_size as usize;
        let mut image = vec![0; block_size * self.blocks as usize];
        for (index, record) in self.mbr_records.iter().enumerate() {
            image[MBR_RECORDS_OFFSET + index * MBR_RECORD_SIZE..][..MBR_RECORD_SIZE]
                .copy_from_slice(record);
        }
        image[MBR_SIGNATURE_OFFSET..MBR_SIZE].copy_from_slice(&self.mbr_signature);
        let array = self.array();
        let array_crc = self.entry_array_crc.unwrap_or_else(|| crc32(&array));
        let primary = self.header(self.my_lba, self.alternate_lba, self.entry_lba, array_crc);
        let array_blocks = array.len().div_ceil(block_size) as u64;
        let backup_entry_lba = self.alternate_lba.wrapping_sub(array_blocks);
        let backup = self.header(self.alternate_lba, self.my_lba, backup_entry_lba, array_crc);
        let mut write = |lba: u64, bytes: &[u8]| {
            let start = lba.checked_mul(block_size as u64);
            let range = start.and_then(|start| {
                let start = usize::try_from(start).ok()?;
                Some(start..start.checked_add(bytes.len())?)
            });
            if let Some(range) = range.filter(|range| range.end <= image.len()) {
                image[range].copy_from_slice(bytes);
            }
        };
        write(1, &primary);
        // An array that a test places at LBA 0 or 1 is not written over the MBR or the header.
        if self.entry_lba > 1 {
            write(self.entry_lba, &array);
        }
        if self.alternate_lba > 1 {
            write(backup_entry_lba, &array);
            write(self.alternate_lba, &backup);
        }
        for (lba, bytes) in &self.contents {
            write(*lba, bytes);
        }
        image
    }

    /// Reads the disk with no expectations.
    fn read(&self) -> Result<Disk, GptError> {
        self.read_expecting(&Expected::new())
    }

    /// Reads the disk with `expected`.
    fn read_expecting(&self, expected: &Expected) -> Result<Disk, GptError> {
        let image = self.image();
        Disk::read(
            image.as_slice(),
            self.block_size,
            image.len() as u64,
            expected,
        )
    }
}

/// Returns [`Spec::standard`]'s disk, for the tests of other modules: p1 in LBA 34, and layers in
/// LBAs 35 through 42 and 43 through 58, of 512 bytes each.
#[cfg(all(feature = "guest", target_os = "linux"))]
pub(crate) fn standard_disk() -> Disk {
    Spec::standard().read().unwrap()
}

/// A source that records the byte ranges that the reader reads.
struct Recording<'a> {
    /// The disk.
    bytes: &'a [u8],
    /// The offset and length of each read, in order.
    reads: RefCell<Vec<(u64, usize)>>,
}

impl ReadAt for Recording<'_> {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.reads.borrow_mut().push((offset, buffer.len()));
        self.bytes.read_exact_at(offset, buffer)
    }
}

/// Asserts that `result` failed as `pattern`, with an optional guard, matches.
macro_rules! assert_fails {
    ($result:expr, $pattern:pat $(if $guard:expr)?) => {
        match $result {
            Err($pattern) $(if $guard)? => {}
            other => panic!(
                "expected {}, got {other:?}",
                stringify!($pattern $(if $guard)?)
            ),
        }
    };
}

#[test]
fn a_disk_reads_with_its_partitions_in_number_order() {
    let mut spec = Spec::standard();
    spec.entry(2).attributes = 1 << 60;
    let disk = spec.read().unwrap();
    assert_eq!(disk.logical_block_size(), 512);
    assert_eq!(disk.size(), 2048 * 512);
    assert_eq!(disk.guid(), DISK_GUID);
    let extents: Vec<_> = disk
        .partitions()
        .iter()
        .map(|partition| {
            (
                partition.number(),
                partition.first_lba(),
                partition.last_lba(),
                partition.offset(),
                partition.size(),
            )
        })
        .collect();
    assert_eq!(
        extents,
        [
            (1, 34, 34, 34 * 512, 512),
            (2, 35, 42, 35 * 512, 8 * 512),
            (3, 43, 58, 43 * 512, 16 * 512),
        ]
    );
    assert_eq!(disk.p1().number(), 1);
    assert_eq!(disk.p1().unique_guid(), guid(1));
    assert_eq!(disk.p1().type_guid(), LINUX_DATA);
    // GPT stores the first three groups of a GUID little-endian.
    assert_eq!(
        disk.p1().type_guid().to_string(),
        "0fc63daf-8483-4772-8e79-3d69d8477de4"
    );
    let lowers: Vec<_> = disk.lowers().iter().map(Partition::number).collect();
    assert_eq!(lowers, [2, 3]);
    assert_eq!(disk.lowers()[0].attributes(), 1 << 60);
}

#[test]
fn a_disk_of_4096_byte_blocks_reads_in_its_own_blocks() {
    // 128 entries fill four blocks: LBAs 2 through 5.
    let spec = Spec::new(4096, 512, 128);
    let disk = spec.read().unwrap();
    assert_eq!(disk.logical_block_size(), 4096);
    let extents: Vec<_> = disk
        .partitions()
        .iter()
        .map(|partition| (partition.first_lba(), partition.offset(), partition.size()))
        .collect();
    assert_eq!(
        extents,
        [
            (6, 6 * 4096, 4096),
            (7, 7 * 4096, 8 * 4096),
            (15, 15 * 4096, 16 * 4096)
        ]
    );
}

#[test]
fn the_geometry_is_whole_blocks_of_a_supported_size() {
    let image = Spec::standard().image();
    let read = |block_size, size| Disk::read(image.as_slice(), block_size, size, &Expected::new());
    for block_size in [0, 256, 511, 513, 1000, 128 * 1024, u32::MAX] {
        assert_fails!(read(block_size, 1 << 20), GptError::LogicalBlockSize(size) if size == block_size);
    }
    for size in [0, 511, 512, 1000, (1 << 20) + 1] {
        assert_fails!(read(512, size), GptError::DiskSize { size: found, logical_block_size: 512 } if found == size);
    }
    // A disk of 4096-byte blocks whose size is a whole number of 512-byte sectors only.
    let image = Spec::new(4096, 512, 128).image();
    let size = image.len() as u64 - 512;
    assert_fails!(
        Disk::read(image.as_slice(), 4096, size, &Expected::new()),
        GptError::DiskSize { .. }
    );
}

#[test]
fn a_disk_larger_than_its_table_reads_without_overflow() {
    // A disk that grew after its table was written keeps its alternate header inside it. The
    // largest size of whole blocks leaves every computation in range.
    let spec = Spec::standard();
    let image = spec.image();
    let size = u64::MAX - u64::MAX % 512;
    let disk = Disk::read(image.as_slice(), 512, size, &Expected::new()).unwrap();
    assert_eq!(disk.size(), size);
    assert_eq!(disk.partitions().len(), 3);
}

#[test]
fn the_protective_mbr_has_its_signature_and_one_protective_record() {
    let mut spec = Spec::standard();
    spec.mbr_signature = [0x55, 0x00];
    assert_fails!(spec.read(), GptError::MbrSignature);

    let mut spec = Spec::standard();
    spec.mbr_records[0] = [0; 16];
    assert_fails!(spec.read(), GptError::NoProtectiveRecord);

    // A classic MBR, whose partitions Linux would read instead of the GPT.
    let mut spec = Spec::standard();
    spec.mbr_records[0][MBR_RECORD_TYPE_OFFSET] = 0x83;
    spec.mbr_records[0][MBR_RECORD_START_OFFSET..][..4].copy_from_slice(&34u32.to_le_bytes());
    assert_fails!(spec.read(), GptError::NoProtectiveRecord);

    let mut spec = Spec::standard();
    spec.mbr_records[0][MBR_RECORD_START_OFFSET..][..4].copy_from_slice(&2u32.to_le_bytes());
    assert_fails!(spec.read(), GptError::ProtectiveRecordStart(2));

    // A hybrid MBR, and a second protective record.
    for other_type in [0x83, PROTECTIVE_TYPE] {
        let mut spec = Spec::standard();
        spec.mbr_records[1] = spec.mbr_records[0];
        spec.mbr_records[1][MBR_RECORD_TYPE_OFFSET] = other_type;
        assert_fails!(spec.read(), GptError::MbrRecords);
    }
    let mut spec = Spec::standard();
    spec.mbr_records[3][0] = 0x80;
    assert_fails!(spec.read(), GptError::MbrRecords);

    // The protective record may be any of the four, and its other fields, such as its size and
    // boot indicator, are not checked, as UEFI tells readers to ignore them.
    let mut spec = Spec::standard();
    spec.mbr_records.swap(0, 2);
    spec.mbr_records[2][0] = 0x80;
    spec.mbr_records[2][12..].copy_from_slice(&u32::MAX.to_le_bytes());
    spec.read().unwrap();
}

#[test]
fn the_mbr_of_a_disk_of_4096_byte_blocks_is_its_first_512_bytes() {
    let spec = Spec::new(4096, 512, 128);
    let mut image = spec.image();
    // The rest of LBA 0 is not part of the MBR.
    image[512..4096].fill(0xFF);
    let size = image.len() as u64;
    Disk::read(image.as_slice(), 4096, size, &Expected::new()).unwrap();
    image[510] = 0;
    assert_fails!(
        Disk::read(image.as_slice(), 4096, size, &Expected::new()),
        GptError::MbrSignature
    );
}

#[test]
fn the_header_has_its_signature_revision_and_size() {
    let mut spec = Spec::standard();
    spec.signature = *b"EFI PARS";
    assert_fails!(spec.read(), GptError::Signature);

    for revision in [0, 0x0001_0001, 0x0002_0000] {
        let mut spec = Spec::standard();
        spec.revision = revision;
        assert_fails!(spec.read(), GptError::Revision(found) if found == revision);
    }

    for size in [0, 91, 513, u32::MAX] {
        let mut spec = Spec::standard();
        spec.header_size = size;
        assert_fails!(spec.read(), GptError::HeaderSize(found) if found == size);
    }
    // A header may span its whole block, which its CRC then covers.
    for size in [92, 93, 512] {
        let mut spec = Spec::standard();
        spec.header_size = size;
        spec.read().unwrap();
    }
    let mut spec = Spec::new(4096, 512, 128);
    spec.header_size = 4096;
    spec.read().unwrap();
    spec.header_size = 4097;
    assert_fails!(spec.read(), GptError::HeaderSize(4097));
}

#[test]
fn the_header_crc_covers_the_header_with_the_crc_field_zero() {
    let spec = Spec::standard();
    let image = spec.image();
    let stored = u32_at(&image[512..], header::CRC);
    let mut spec = Spec::standard();
    spec.header_crc = Some(stored ^ 1);
    assert_fails!(spec.read(), GptError::HeaderCrc { stored: found, computed } if found == stored ^ 1 && computed == stored);

    // A change to any byte that the header's size covers, and to no other.
    let read = |image: &[u8]| Disk::read(image, 512, image.len() as u64, &Expected::new());
    for offset in [0, 15, 20, 56, 91] {
        let mut image = spec_image_with_header_size(92);
        image[512 + offset] ^= 0x01;
        assert!(read(&image).is_err(), "byte {offset}");
    }
    let mut image = spec_image_with_header_size(92);
    image[512 + 92] = 0xFF;
    read(&image).unwrap();
    let mut image = spec_image_with_header_size(100);
    image[512 + 99] = 0xFF;
    assert_fails!(read(&image), GptError::HeaderCrc { .. });
}

/// Returns the standard disk with a header of `size` bytes.
fn spec_image_with_header_size(size: u32) -> Vec<u8> {
    let mut spec = Spec::standard();
    spec.header_size = size;
    spec.image()
}

#[test]
fn the_header_has_a_zero_reserved_field_and_its_own_lba() {
    let mut spec = Spec::standard();
    spec.reserved = 1;
    assert_fails!(spec.read(), GptError::HeaderReserved(1));

    for lba in [0, 2, 2047, u64::MAX] {
        let mut spec = Spec::standard();
        spec.my_lba = lba;
        assert_fails!(spec.read(), GptError::MyLba(found) if found == lba);
    }
}

#[test]
fn the_entries_are_128_bytes_and_bounded_in_number() {
    for size in [0, 64, 127, 256, u32::MAX] {
        let mut spec = Spec::standard();
        spec.entry_size = size;
        assert_fails!(spec.read(), GptError::EntrySize(found) if found == size);
    }
    for count in [0, 8193, u32::MAX] {
        let mut spec = Spec::standard();
        spec.entry_count = count;
        assert_fails!(spec.read(), GptError::EntryCount(found) if found == count);
    }
    // The largest array, 1 MiB in 256 4096-byte blocks, and the smallest, one entry with the
    // others of its block outside the array, which its CRC does not cover.
    let spec = Spec::new(4096, 600, 8192);
    assert_eq!(spec.read().unwrap().p1().first_lba(), 258);
    let mut spec = Spec::new(512, 2048, 3);
    spec.contents
        .push((2, [spec.array(), vec![0xFF; 128]].concat()));
    let disk = spec.read().unwrap();
    assert_eq!(disk.p1().first_lba(), 3);
}

#[test]
fn the_entry_array_lies_after_the_header_and_before_the_usable_blocks() {
    // The standard disk's array fills LBAs 2 through 33 and its last LBA is 2,047.
    for lba in [0, 1, 2016, 2047, u64::MAX - 1, u64::MAX] {
        let mut spec = Spec::standard();
        spec.entry_lba = lba;
        assert_fails!(spec.read(), GptError::EntryArrayLba(found) if found == lba);
    }
    for lba in [0, 2, 33] {
        let mut spec = Spec::standard();
        spec.first_usable_lba = lba;
        assert_fails!(spec.read(), GptError::FirstUsableLba(found) if found == lba);
    }
    // The array may sit later, and the usable blocks need not start right after it, nor the
    // partitions in the order of their numbers.
    let mut spec = Spec::standard();
    spec.entry_lba = 3;
    spec.first_usable_lba = 35;
    spec.entry(1).first_lba = 59;
    spec.entry(1).last_lba = 59;
    assert_eq!(spec.read().unwrap().p1().offset(), 59 * 512);
}

#[test]
fn the_usable_blocks_end_before_the_alternate_header_inside_the_disk() {
    for lba in [0, 33, 2047, 2048, u64::MAX] {
        let mut spec = Spec::standard();
        spec.last_usable_lba = lba;
        assert_fails!(spec.read(), GptError::LastUsableLba(found) if found == lba);
    }
    let mut spec = Spec::standard();
    spec.first_usable_lba = u64::MAX;
    assert_fails!(spec.read(), GptError::LastUsableLba(2014));

    for lba in [0, 1, 34, 2014, 2048, u64::MAX] {
        let mut spec = Spec::standard();
        spec.alternate_lba = lba;
        assert_fails!(spec.read(), GptError::AlternateLba(found) if found == lba);
    }
    // An alternate header short of the disk's end, as after the disk grew, and a last usable
    // LBA just before it.
    let mut spec = Spec::standard();
    spec.alternate_lba = 2015;
    spec.read().unwrap();
    let mut spec = Spec::standard();
    spec.last_usable_lba = 2046;
    spec.read().unwrap();
}

#[test]
fn the_disk_guid_is_not_nil() {
    let mut spec = Spec::standard();
    spec.disk_guid = Uuid::NIL;
    assert_fails!(spec.read(), GptError::NilDiskGuid);
}
#[test]
fn the_entry_array_crc_covers_every_entry() {
    let spec = Spec::standard();
    let stored = crc32(&spec.array());
    let mut wrong = Spec::standard();
    wrong.entry_array_crc = Some(!stored);
    assert_fails!(wrong.read(), GptError::EntryArrayCrc { stored: found, computed } if found == !stored && computed == stored);

    // A change to a used entry, and to the last unused one.
    for offset in [128 + entry::LAST_LBA, 127 * 128 + 127] {
        let mut image = spec.image();
        image[2 * 512 + offset] ^= 0x01;
        assert_fails!(
            Disk::read(image.as_slice(), 512, image.len() as u64, &Expected::new()),
            GptError::EntryArrayCrc { .. }
        );
    }
}

#[test]
fn an_invalid_primary_table_fails_although_its_backup_is_intact() {
    let spec = Spec::standard();
    let intact = spec.image();
    let read = |image: &[u8]| Disk::read(image, 512, image.len() as u64, &Expected::new());
    let expected = read(&intact).unwrap();

    // The backup's header and array, at LBAs 2,047 and 2,015, hold the same table.
    let backup = &intact[2015 * 512..];
    assert_eq!(&backup[32 * 512..][..8], SIGNATURE);
    assert_eq!(backup[..32 * 512], intact[2 * 512..34 * 512]);

    let mut image = intact.clone();
    image[512 + header::CRC] ^= 0x01;
    assert_fails!(read(&image), GptError::HeaderCrc { .. });
    let mut image = intact.clone();
    image[512..1024].fill(0);
    assert_fails!(read(&image), GptError::Signature);
    let mut image = intact.clone();
    image[2 * 512 + entry::FIRST_LBA] ^= 0x01;
    assert_fails!(read(&image), GptError::EntryArrayCrc { .. });
    let mut image = intact.clone();
    image[2 * 512..34 * 512].fill(0);
    assert_fails!(read(&image), GptError::EntryArrayCrc { .. });

    // A destroyed backup changes nothing.
    let mut image = intact;
    image[2015 * 512..].fill(0);
    assert_eq!(read(&image).unwrap(), expected);
}

#[test]
fn the_reader_never_reads_past_the_usable_blocks() {
    let mut spec = Spec::standard();
    spec.contents.push((34, b"configuration".to_vec()));
    let image = spec.image();
    let source = Recording {
        bytes: &image,
        reads: RefCell::new(Vec::new()),
    };
    let disk = Disk::read(&source, 512, image.len() as u64, &Expected::new()).unwrap();
    assert_eq!(disk.read_p1(&source, 512).unwrap(), b"configuration");
    let reads = source.reads.into_inner();
    // The MBR, the header, the array, and p1, each read once.
    assert_eq!(
        reads,
        [(0, 512), (512, 512), (1024, 128 * 128), (34 * 512, 512)]
    );
}

#[test]
fn partitions_lie_in_the_usable_blocks_in_ascending_ranges() {
    for (number, first, last) in [
        (1, 33, 34),
        (3, 43, 2015),
        (2, 42, 35),
        (2, 35, u64::MAX),
        (3, u64::MAX, u64::MAX),
        (1, 0, 0),
    ] {
        let mut spec = Spec::standard();
        spec.entry(number).first_lba = first;
        spec.entry(number).last_lba = last;
        assert_fails!(
            spec.read(),
            GptError::Extent { number: found, first_lba, last_lba }
                if usize::from(found) == number && first_lba == first && last_lba == last
        );
    }
    // The first and the last usable blocks themselves.
    let mut spec = Spec::standard();
    spec.entry(3).last_lba = 2014;
    spec.read().unwrap();
}

#[test]
fn partitions_share_no_block() {
    for (number, start, end, overlap) in [
        (3, 42, 58, (2, 3)),
        (3, 35, 42, (2, 3)),
        (3, 34, 34, (1, 3)),
        (1, 40, 40, (1, 2)),
        (3, 34, 100, (1, 3)),
    ] {
        let mut spec = Spec::standard();
        spec.entry(number).first_lba = start;
        spec.entry(number).last_lba = end;
        assert_fails!(spec.read(), GptError::Overlap { first, second } if (first, second) == overlap);
    }
    // Partitions may lie in any order on the disk.
    let mut spec = Spec::standard();
    spec.entry(1).first_lba = 100;
    spec.entry(1).last_lba = 100;
    spec.entry(3).first_lba = 34;
    spec.entry(3).last_lba = 34;
    let disk = spec.read().unwrap();
    assert_eq!(disk.p1().first_lba(), 100);
    assert_eq!(disk.lowers()[1].first_lba(), 34);
}

#[test]
fn used_entries_come_first_without_holes() {
    let mut spec = Spec::standard();
    spec.entries.insert(2, None);
    assert_fails!(spec.read(), GptError::Hole { unused: 3, used: 4 });
    let mut spec = Spec::standard();
    spec.entries.insert(0, None);
    assert_fails!(spec.read(), GptError::Hole { unused: 1, used: 2 });

    // A hole past the 255 entries that Linux reads still fails.
    let mut spec = Spec::new(512, 1024, 512);
    spec.entries.resize(511, None);
    spec.entries.push(Some(Entry::new(9, 300, 300)));
    assert_fails!(
        spec.read(),
        GptError::Hole {
            unused: 4,
            used: 512
        }
    );

    // An unused entry is not read beyond its type GUID.
    let mut spec = Spec::standard();
    let mut unused = Entry::new(4, 0, u64::MAX);
    unused.type_guid = Uuid::NIL;
    spec.entries.push(Some(unused));
    assert_eq!(spec.read().unwrap().partitions().len(), 3);
}

#[test]
fn a_disk_has_p1_and_one_to_254_layers() {
    let mut spec = Spec::standard();
    spec.entries.clear();
    assert_fails!(spec.read(), GptError::PartitionCount(0));
    let mut spec = Spec::standard();
    spec.entries.truncate(1);
    assert_fails!(spec.read(), GptError::PartitionCount(1));
    let mut spec = Spec::standard();
    spec.entries.truncate(2);
    assert_eq!(spec.read().unwrap().lowers().len(), 1);

    // 256 entries fill LBAs 2 through 65, and one-block partitions follow from LBA 66.
    let mut spec = Spec::new(512, 1024, 256);
    spec.entries = (0..=254u8)
        .map(|index| {
            let lba = 66 + u64::from(index);
            let mut entry = Entry::new(1, lba, lba);
            let mut unique = [0; 16];
            unique[0] = index;
            unique[15] = 0xAB;
            entry.unique_guid = Uuid::from_bytes(unique);
            Some(entry)
        })
        .collect();
    let disk = spec.read().unwrap();
    assert_eq!(disk.partitions().len(), MAX_PARTITIONS);
    assert_eq!(disk.partitions()[254].number(), 255);
    spec.entries.push(Some(Entry::new(0xEF, 321, 321)));
    assert_fails!(spec.read(), GptError::PartitionCount(256));
}

#[test]
fn partition_guids_are_unique_and_not_nil() {
    let mut spec = Spec::standard();
    spec.entry(2).unique_guid = Uuid::NIL;
    assert_fails!(spec.read(), GptError::NilPartitionGuid { number: 2 });
    let mut spec = Spec::standard();
    spec.entry(3).unique_guid = guid(2);
    assert_fails!(spec.read(), GptError::DuplicateGuid { number: 3 });
    let mut spec = Spec::standard();
    spec.entry(1).unique_guid = DISK_GUID;
    assert_fails!(spec.read(), GptError::DuplicateGuid { number: 1 });
    // Type GUIDs may repeat.
    let mut spec = Spec::standard();
    spec.entry(1).type_guid = guid(0x77);
    let disk = spec.read().unwrap();
    assert_eq!(disk.p1().type_guid(), guid(0x77));
    assert_eq!(disk.lowers()[0].type_guid(), LINUX_DATA);
}

#[test]
fn expectations_follow_the_format_checks() {
    let spec = Spec::standard();
    let sizes = [512, 8 * 512, 16 * 512];
    let all = Expected::new().disk_guid(DISK_GUID).partitions(
        (1..=3)
            .zip(sizes)
            .map(|(id, size)| ExpectedPartition::new().unique_guid(guid(id)).size(size))
            .collect(),
    );
    spec.read_expecting(&all).unwrap();
    // Unset expectations check nothing.
    let partial = Expected::new().partitions(vec![
        ExpectedPartition::new(),
        ExpectedPartition::new().size(8 * 512),
        ExpectedPartition::new().unique_guid(guid(3)),
    ]);
    spec.read_expecting(&partial).unwrap();

    assert_fails!(
        spec.read_expecting(&Expected::new().disk_guid(guid(0xD2))),
        GptError::DiskGuidMismatch { expected, found } if expected == guid(0xD2) && found == DISK_GUID
    );
    assert_fails!(
        spec.read_expecting(&Expected::new().partitions(vec![ExpectedPartition::new(); 2])),
        GptError::PartitionCountMismatch {
            expected: 2,
            found: 3
        }
    );
    assert_fails!(
        spec.read_expecting(&Expected::new().partitions(vec![ExpectedPartition::new(); 4])),
        GptError::PartitionCountMismatch {
            expected: 4,
            found: 3
        }
    );
    let wrong_guid = Expected::new().partitions(vec![
        ExpectedPartition::new(),
        ExpectedPartition::new().unique_guid(guid(3)),
        ExpectedPartition::new(),
    ]);
    assert_fails!(
        spec.read_expecting(&wrong_guid),
        GptError::PartitionGuidMismatch { number: 2, expected, found } if expected == guid(3) && found == guid(2)
    );
    let wrong_size = Expected::new().partitions(vec![
        ExpectedPartition::new(),
        ExpectedPartition::new(),
        ExpectedPartition::new().size(15 * 512),
    ]);
    assert_fails!(
        spec.read_expecting(&wrong_size),
        GptError::PartitionSizeMismatch {
            number: 3,
            expected: 7680,
            found: 8192
        }
    );

    // A disk that breaks the format fails as such, whatever it was expected to be.
    let mut corrupt = Spec::standard();
    corrupt.header_crc = Some(0);
    assert_fails!(
        corrupt.read_expecting(&Expected::new().disk_guid(guid(0xD2))),
        GptError::HeaderCrc { .. }
    );
}

/// Returns the standard disk whose p1, of `blocks` blocks of `block_size` bytes, starts with
/// `contents`.
fn disk_with_p1(block_size: u32, blocks: u64, contents: &[u8]) -> (Disk, Vec<u8>) {
    let mut spec = if block_size == 512 {
        Spec::standard()
    } else {
        Spec::new(block_size, 512, 128)
    };
    let first = spec.first_usable_lba;
    spec.entry(1).last_lba = first + blocks - 1;
    spec.entry(2).first_lba = first + blocks;
    spec.entry(2).last_lba = first + blocks;
    spec.entry(3).first_lba = first + blocks + 1;
    spec.entry(3).last_lba = first + blocks + 1;
    spec.contents.push((first, contents.to_vec()));
    let image = spec.image();
    let disk = Disk::read(
        image.as_slice(),
        block_size,
        image.len() as u64,
        &Expected::new(),
    )
    .unwrap();
    (disk, image)
}

#[test]
fn p1_is_its_opaque_bytes_without_trailing_padding() {
    // One sector of configuration, padded with NUL bytes.
    let (disk, image) = disk_with_p1(512, 1, b"{\"layers\":2}\n");
    assert_eq!(disk.p1().size(), 512);
    assert_eq!(
        disk.read_p1(image.as_slice(), 512).unwrap(),
        b"{\"layers\":2}"
    );

    // Bytes that are not UTF-8, with NUL bytes and whitespace inside, are kept.
    let bytes = [0xFF, 0x00, 0xC3, 0x28, b' ', 0x80, b'\t', 0x01];
    let (disk, image) = disk_with_p1(512, 1, &bytes);
    assert_eq!(disk.read_p1(image.as_slice(), 512).unwrap(), bytes);

    // Trailing NUL bytes and whitespace go, in any order; leading ones stay, as does a
    // vertical tab, which is not ASCII whitespace.
    let (disk, image) = disk_with_p1(512, 1, b" \tx \t\r\n\x0c\0 \0\n");
    assert_eq!(disk.read_p1(image.as_slice(), 512).unwrap(), b" \tx");
    let (disk, image) = disk_with_p1(512, 1, b"x\x0b\0");
    assert_eq!(disk.read_p1(image.as_slice(), 512).unwrap(), b"x\x0b");
    let (disk, image) = disk_with_p1(512, 1, b"\0\n \0");
    assert_eq!(disk.read_p1(image.as_slice(), 512).unwrap(), b"");

    // A full sector, and p1 across several sectors.
    let full = vec![b'a'; 512];
    let (disk, image) = disk_with_p1(512, 1, &full);
    assert_eq!(disk.read_p1(image.as_slice(), 512).unwrap(), full);
    let long: Vec<u8> = (0..1500).map(|index| (index % 251) as u8 + 1).collect();
    let (disk, image) = disk_with_p1(512, 3, &long);
    assert_eq!(disk.read_p1(image.as_slice(), 1536).unwrap(), long);

    // One sector of a disk of 4096-byte blocks.
    let (disk, image) = disk_with_p1(4096, 1, b"config\n");
    assert_eq!(disk.p1().size(), 4096);
    assert_eq!(disk.read_p1(image.as_slice(), 4096).unwrap(), b"config");
}

#[test]
fn p1_over_the_bound_fails_before_it_is_read() {
    let (disk, image) = disk_with_p1(512, 2, b"x");
    for bound in [0, 1, 1023] {
        assert_fails!(
            disk.read_p1(image.as_slice(), bound),
            GptError::P1TooLarge { size: 1024, bound: found } if found == bound
        );
    }
    let source = Recording {
        bytes: &image,
        reads: RefCell::new(Vec::new()),
    };
    assert!(disk.read_p1(&source, 1023).is_err());
    assert!(source.reads.borrow().is_empty());
    assert_eq!(disk.read_p1(&source, 1024).unwrap(), b"x");
    assert_eq!(disk.read_p1(&source, usize::MAX).unwrap(), b"x");
}

#[test]
fn a_short_source_fails_its_read() {
    let spec = Spec::standard();
    let image = spec.image();
    let size = image.len() as u64;
    // The source ends before the MBR or the header, and inside the entry array.
    for length in [0, 511, 1024, 2048, 34 * 512 - 1] {
        let short = &image[..length];
        assert_fails!(
            Disk::read(short, 512, size, &Expected::new()),
            GptError::Io(error) if error.kind() == io::ErrorKind::UnexpectedEof
        );
    }
    let disk = Disk::read(image.as_slice(), 512, size, &Expected::new()).unwrap();
    assert_fails!(
        disk.read_p1(&image[..34 * 512 + 100], 512),
        GptError::Io(error) if error.kind() == io::ErrorKind::UnexpectedEof
    );
}

#[test]
fn byte_sources_read_whole_ranges_only() {
    let bytes: &[u8] = b"0123456789";
    let mut buffer = [0; 4];
    bytes.read_exact_at(6, &mut buffer).unwrap();
    assert_eq!(&buffer, b"6789");
    bytes.read_exact_at(10, &mut []).unwrap();
    for offset in [7, 11, u64::MAX] {
        let error = bytes.read_exact_at(offset, &mut buffer).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}

#[test]
fn files_read_at_offsets() {
    use std::io::Write;

    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"0123456789").unwrap();
    let mut buffer = [0; 4];
    file.read_exact_at(2, &mut buffer).unwrap();
    assert_eq!(&buffer, b"2345");
    let error = file.read_exact_at(8, &mut buffer).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);

    // A disk image in a file.
    let mut spec = Spec::standard();
    spec.contents.push((34, b"from a file".to_vec()));
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&spec.image()).unwrap();
    let disk = Disk::read(&file, 512, 2048 * 512, &Expected::new()).unwrap();
    assert_eq!(disk.read_p1(&file, 512).unwrap(), b"from a file");
}

#[test]
fn errors_describe_the_check_and_keep_their_cause() {
    let error = GptError::Hole { unused: 3, used: 4 };
    assert_eq!(
        error.to_string(),
        "partition entry 4 is used, but entry 3 before it is not"
    );
    assert_eq!(
        GptError::PartitionCount(256).to_string(),
        "the disk has 256 partitions, not 2 through 255: p1 and at least one layer"
    );
    assert_eq!(
        GptError::EntryCount(0).to_string(),
        "the GPT header declares 0 partition entries, not 1 through 8192"
    );
    let error = GptError::from(io::Error::from(io::ErrorKind::UnexpectedEof));
    assert!(std::error::Error::source(&error).is_some());
    assert!(std::error::Error::source(&GptError::Signature).is_none());
}
