//! The GPT disks of the VM tests' layered roots, which [`write_disk`] writes as UEFI lays out a
//! disk, and the small layers that they hold, file systems in the ext2 format that
//! [`small_layer`] writes, which the kernel's ext4 driver mounts, since `mkfs.ext4` is not
//! available everywhere that the tests run.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use nvx::gpt::{Disk, Expected, ExpectedPartition};
use nvx::uuid::Uuid;

/// The size of a sector of a disk, its logical block, in bytes.
const SECTOR: u64 = 512;
/// The first sector that a partition may take: after the protective MBR, the primary header, and
/// its array of 128 entries of 128 bytes.
const FIRST_USABLE: u64 = 34;
/// The type GUID of every partition of the disks: Linux's file system data.
const LINUX_DATA: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
/// The size of a block of the layer that [`small_layer`] writes, in bytes.
pub(crate) const LAYER_BLOCK: usize = 1024;
/// The number of blocks of that layer: 128 KiB.
pub(crate) const LAYER_BLOCKS: usize = 128;
/// The number of inodes of that layer, of which the first ten are reserved, as in every ext2 file
/// system, and the second is its top directory.
const LAYER_INODES: usize = 16;

/// The layout of a GPT disk that [`write_disk`] writes.
pub(crate) struct Layout<'a> {
    /// The disk's GUID.
    pub(crate) guid: &'a str,
    /// The partitions, in the order of their entries: the first partition, then the layers.
    pub(crate) partitions: &'a [Partition<'a>],
    /// The number of sectors of which the first sector of each partition is a multiple: at least
    /// [`FIRST_USABLE`].
    pub(crate) alignment: u64,
    /// The disk's length, in bytes, whole sectors, or `None` for the partitions' and then
    /// `alignment` sectors more, in which the backup array and header end the disk.
    pub(crate) length: Option<u64>,
}

/// A partition of a [`Layout`].
pub(crate) struct Partition<'a> {
    /// The partition's unique GUID.
    pub(crate) guid: &'a str,
    /// The partition's size, in bytes: whole sectors, at least what it holds.
    pub(crate) size: u64,
    /// What the partition holds from its start, before NUL bytes.
    pub(crate) contents: Contents<'a>,
}

/// What a [`Partition`] holds.
pub(crate) enum Contents<'a> {
    /// These bytes.
    Bytes(&'a [u8]),
    /// The bytes of this file.
    File(&'a Path),
}

/// Writes the GPT disk of `layout` at `path`, as UEFI lays out a disk, with a protective MBR, the
/// primary GPT header and its array of 128 entries, and the backup array and header at the disk's
/// end, of its partitions in their order, each aligned to its `alignment`, after the end of the
/// one before. The file is a new one, which no one else may change: of the mode 0644 on Unix,
/// whatever the umask, and on Windows of its directory's access control list.
///
/// Then checks the disk as the guest's assembly does, through `nvx::gpt`, with the GUIDs and
/// sizes that it wrote, and returns the bytes of its first partition, as the guest returns them,
/// without their trailing NUL bytes and whitespace.
pub(crate) fn write_disk(path: &Path, layout: &Layout<'_>) -> Vec<u8> {
    let alignment = layout.alignment;
    assert!(
        alignment >= FIRST_USABLE,
        "a partition starts after the table"
    );
    let mut extents = Vec::new();
    let mut next = alignment;
    for partition in layout.partitions {
        assert_eq!(partition.size % SECTOR, 0, "a partition is whole sectors");
        let last = next + partition.size / SECTOR - 1;
        extents.push((next, last));
        next = (last + 1).next_multiple_of(alignment);
    }
    // The backup array's 32 sectors and the backup header end the disk, after the usable sectors.
    let sectors = match layout.length {
        None => next + alignment,
        Some(length) => {
            assert_eq!(length % SECTOR, 0, "a disk is whole sectors");
            length / SECTOR
        }
    };
    let (first_usable, last_usable) = (FIRST_USABLE, sectors - 34);
    assert!(
        extents.last().is_some_and(|(_, last)| *last <= last_usable),
        "the partitions end before the backup array"
    );
    let (backup_entries, backup_header) = (sectors - 33, sectors - 1);
    let guid = |text: &str| Uuid::parse(text).unwrap().to_bytes_le();

    let mut entries = vec![0; 128 * 128];
    for (entry, ((first, last), partition)) in entries
        .chunks_exact_mut(128)
        .zip(extents.iter().zip(layout.partitions))
    {
        entry[0..16].copy_from_slice(&guid(LINUX_DATA));
        entry[16..32].copy_from_slice(&guid(partition.guid));
        entry[32..40].copy_from_slice(&first.to_le_bytes());
        entry[40..48].copy_from_slice(&last.to_le_bytes());
    }
    let entries_crc = crc32(&entries);
    let header = |own: u64, alternate: u64, array: u64| {
        let mut header = vec![0; 92];
        header[0..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
        header[12..16].copy_from_slice(&92_u32.to_le_bytes());
        header[24..32].copy_from_slice(&own.to_le_bytes());
        header[32..40].copy_from_slice(&alternate.to_le_bytes());
        header[40..48].copy_from_slice(&first_usable.to_le_bytes());
        header[48..56].copy_from_slice(&last_usable.to_le_bytes());
        header[56..72].copy_from_slice(&guid(layout.guid));
        header[72..80].copy_from_slice(&array.to_le_bytes());
        header[80..84].copy_from_slice(&128_u32.to_le_bytes());
        header[84..88].copy_from_slice(&128_u32.to_le_bytes());
        header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&header);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        header
    };
    // The protective MBR: one record of type 0xEE, from LBA 1 over the rest of the disk, with the
    // addresses that UEFI gives it.
    let mut mbr = vec![0; 512];
    let protective = u32::try_from(sectors - 1).unwrap_or(u32::MAX);
    mbr[446..450].copy_from_slice(&[0x00, 0x00, 0x02, 0x00]);
    mbr[450..454].copy_from_slice(&[0xee, 0xff, 0xff, 0xff]);
    mbr[454..458].copy_from_slice(&1_u32.to_le_bytes());
    mbr[458..462].copy_from_slice(&protective.to_le_bytes());
    mbr[510..512].copy_from_slice(&[0x55, 0xaa]);

    let mut disk = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        disk.set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    disk.set_len(sectors * SECTOR).unwrap();
    let table = [
        (0, mbr),
        (1, header(1, backup_header, 2)),
        (2, entries.clone()),
        (backup_entries, entries),
        (backup_header, header(backup_header, 1, backup_entries)),
    ];
    for (sector, bytes) in table {
        disk.seek(SeekFrom::Start(sector * SECTOR)).unwrap();
        disk.write_all(&bytes).unwrap();
    }
    for ((first, _), partition) in extents.iter().zip(layout.partitions) {
        disk.seek(SeekFrom::Start(first * SECTOR)).unwrap();
        let written = match partition.contents {
            Contents::Bytes(bytes) => {
                disk.write_all(bytes).unwrap();
                u64::try_from(bytes.len()).unwrap()
            }
            Contents::File(file) => io::copy(&mut File::open(file).unwrap(), &mut disk).unwrap(),
        };
        assert!(written <= partition.size, "a partition holds what it holds");
    }
    disk.sync_all().unwrap();

    let partitions = layout
        .partitions
        .iter()
        .map(|partition| {
            ExpectedPartition::new()
                .unique_guid(Uuid::parse(partition.guid).unwrap())
                .size(partition.size)
        })
        .collect();
    let expected = Expected::new()
        .disk_guid(Uuid::parse(layout.guid).unwrap())
        .partitions(partitions);
    let read = Disk::read(&disk, 512, sectors * SECTOR, &expected).unwrap();
    let size = layout.partitions[0].size;
    read.read_first_partition(&disk, usize::try_from(size).unwrap())
        .unwrap()
}

/// Returns a small file system in the ext2 format, of [`LAYER_BLOCKS`] blocks of [`LAYER_BLOCK`]
/// bytes, which the kernel's ext4 driver mounts as an ext4 file system without features: its top
/// directory holds the regular `files`, each a name and its bytes, of at most 12 blocks each,
/// then the empty `directories`, each with mode 0755, and at most [`LAYER_INODES`] inodes in all.
///
/// Block 1 holds the superblock, 2 the group descriptor, 3 and 4 the block and inode bitmaps, 5
/// and 6 the inode table, 7 the top directory, and the blocks after them the files' bytes and the
/// directories.
pub(crate) fn small_layer(files: &[(&str, &[u8])], directories: &[&str]) -> Vec<u8> {
    /// Writes `value` at `offset` of `image`, in little-endian order.
    fn put<const N: usize>(image: &mut [u8], offset: usize, value: [u8; N]) {
        image[offset..offset + N].copy_from_slice(&value);
    }
    /// Writes the inode `number` of `image`, of `mode`, `size` bytes, and `links` links, whose data
    /// is in `blocks`, at most the 12 that an inode addresses directly.
    fn inode(
        image: &mut [u8],
        number: usize,
        mode: u16,
        size: usize,
        links: usize,
        blocks: &[usize],
    ) {
        assert!(
            blocks.len() <= 12,
            "an inode of the layer addresses 12 blocks"
        );
        let offset = 5 * LAYER_BLOCK + (number - 1) * 128;
        put(image, offset, mode.to_le_bytes());
        put(
            image,
            offset + 4,
            u32::try_from(size).unwrap().to_le_bytes(),
        );
        put(
            image,
            offset + 26,
            u16::try_from(links).unwrap().to_le_bytes(),
        );
        // The number of 512-byte sectors that the data takes.
        let sectors = u32::try_from(blocks.len() * LAYER_BLOCK / 512).unwrap();
        put(image, offset + 28, sectors.to_le_bytes());
        for (index, block) in blocks.iter().enumerate() {
            let block = u32::try_from(*block).unwrap();
            put(image, offset + 40 + 4 * index, block.to_le_bytes());
        }
    }
    /// Writes the directory `entries` into `block` of `image`, each an inode, a name, and a file
    /// type, the last of which spans the rest of the block.
    fn directory(image: &mut [u8], block: usize, entries: &[(usize, &str, u8)]) {
        let (mut offset, end) = (block * LAYER_BLOCK, (block + 1) * LAYER_BLOCK);
        for (index, (inode, name, kind)) in entries.iter().enumerate() {
            let length = if index + 1 == entries.len() {
                end - offset
            } else {
                (8 + name.len()).next_multiple_of(4)
            };
            put(image, offset, u32::try_from(*inode).unwrap().to_le_bytes());
            put(
                image,
                offset + 4,
                u16::try_from(length).unwrap().to_le_bytes(),
            );
            image[offset + 6] = u8::try_from(name.len()).unwrap();
            image[offset + 7] = *kind;
            image[offset + 8..offset + 8 + name.len()].copy_from_slice(name.as_bytes());
            offset += length;
        }
    }
    /// Sets the bits `bits` of the bitmap in `block` of `image`.
    fn set(image: &mut [u8], block: usize, bits: std::ops::Range<usize>) {
        for bit in bits {
            image[block * LAYER_BLOCK + bit / 8] |= 1 << (bit % 8);
        }
    }

    let (top, regular, directory_type) = (2, 1, 2);
    let mut image = vec![0; LAYER_BLOCK * LAYER_BLOCKS];
    // The first block and the first inode that are free.
    let (mut block, mut number) = (8, 11);
    let mut entries = vec![(top, ".", directory_type), (top, "..", directory_type)];
    for (name, bytes) in files {
        let blocks = (block..block + bytes.len().div_ceil(LAYER_BLOCK)).collect::<Vec<_>>();
        image[block * LAYER_BLOCK..block * LAYER_BLOCK + bytes.len()].copy_from_slice(bytes);
        inode(&mut image, number, 0o100_644, bytes.len(), 1, &blocks);
        entries.push((number, *name, regular));
        block += blocks.len();
        number += 1;
    }
    for name in directories {
        directory(
            &mut image,
            block,
            &[(number, ".", directory_type), (top, "..", directory_type)],
        );
        inode(&mut image, number, 0o040_755, LAYER_BLOCK, 2, &[block]);
        entries.push((number, *name, directory_type));
        block += 1;
        number += 1;
    }
    directory(&mut image, 7, &entries);
    inode(
        &mut image,
        top,
        0o040_755,
        LAYER_BLOCK,
        2 + directories.len(),
        &[7],
    );
    assert!(
        block <= LAYER_BLOCKS && number <= LAYER_INODES + 1,
        "the layer is too small"
    );

    // The bitmaps of the used blocks, from block 1, and inodes, from inode 1, whose bits past the
    // last block and inode are set, as `mke2fs` sets them.
    set(&mut image, 3, 0..block - 1);
    set(&mut image, 3, LAYER_BLOCKS - 1..LAYER_BLOCK * 8);
    set(&mut image, 4, 0..number - 1);
    set(&mut image, 4, LAYER_INODES..LAYER_BLOCK * 8);
    let free_blocks = LAYER_BLOCKS - block;
    let free_inodes = LAYER_INODES + 1 - number;

    // The superblock: one group of blocks, inodes of 128 bytes, the first of them not reserved,
    // 11, and only the `filetype` feature.
    let superblock = LAYER_BLOCK;
    for (offset, value) in [
        (0x00, LAYER_INODES),
        (0x04, LAYER_BLOCKS),
        (0x0c, free_blocks),
        (0x10, free_inodes),
        (0x14, 1),
        (0x20, LAYER_BLOCK * 8),
        (0x24, LAYER_BLOCK * 8),
        (0x28, LAYER_INODES),
        (0x4c, 1),
        (0x54, 11),
        (0x60, 0x2),
    ] {
        let value = u32::try_from(value).unwrap();
        put(&mut image, superblock + offset, value.to_le_bytes());
    }
    for (offset, value) in [
        (0x36, 0xffff),
        (0x38, 0xef53),
        (0x3a, 1),
        (0x3c, 1),
        (0x58, 128),
    ] {
        put(&mut image, superblock + offset, u16::to_le_bytes(value));
    }
    let uuid = Uuid::parse("2d4e6f80-91a2-4b3c-8d5e-6f708192a3b4").unwrap();
    image[superblock + 0x68..superblock + 0x78].copy_from_slice(uuid.as_bytes());

    // The group descriptor: where the bitmaps and the inode table are, and what is free.
    let descriptor = 2 * LAYER_BLOCK;
    for (offset, value) in [(0x00, 3), (0x04, 4), (0x08, 5)] {
        put(&mut image, descriptor + offset, u32::to_le_bytes(value));
    }
    for (offset, value) in [
        (0x0c, free_blocks),
        (0x0e, free_inodes),
        (0x10, 1 + directories.len()),
    ] {
        let value = u16::try_from(value).unwrap();
        put(&mut image, descriptor + offset, value.to_le_bytes());
    }
    image
}

/// Returns the CRC-32 of `bytes`, which GPT's headers and entry arrays hold: of the reflected
/// polynomial 0xEDB88320, from all ones, inverted.
fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0, |crc, byte| {
        (0..8).fold(crc ^ u32::from(*byte), |crc: u32, _| {
            (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg())
        })
    })
}

/// Returns `bytes` in lowercase hexadecimal, as the `first-partition` probe reports them.
pub(crate) fn hexadecimal(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0xf])
        .map(|digit| char::from(DIGITS[usize::from(digit)]))
        .collect()
}
