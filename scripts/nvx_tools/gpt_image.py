"""Write GPT disk images, and MBRs in place of their protective MBRs, for tests."""

from __future__ import annotations

import struct
import uuid
import zlib
from collections.abc import Sequence
from dataclasses import dataclass

SECTOR_SIZE = 512
ENTRY_SIZE = 128
ENTRY_COUNT = 128
HEADER_SIZE = 92
REVISION = 0x0001_0000
SIGNATURE = b"EFI PART"
MBR_SIGNATURE = b"\x55\xaa"
MBR_RECORDS_OFFSET = 446
MBR_RECORD_SIZE = 16
PROTECTIVE_TYPE = 0xEE
LINUX_TYPE = 0x83
# The Linux filesystem data partition type.
LINUX_DATA = uuid.UUID("0fc63daf-8483-4772-8e79-3d69d8477de4")
# The blocks of the entry array, which start at LBA 2, as UEFI's defaults do.
ENTRY_ARRAY_BLOCKS = ENTRY_COUNT * ENTRY_SIZE // SECTOR_SIZE


@dataclass(frozen=True)
class Partition:
    """A Linux data partition from ``first_lba`` through ``last_lba`` that starts with
    ``contents``."""

    unique_guid: uuid.UUID
    first_lba: int
    last_lba: int
    contents: bytes = b""

    @property
    def offset(self) -> int:
        return self.first_lba * SECTOR_SIZE

    @property
    def size(self) -> int:
        return (self.last_lba - self.first_lba + 1) * SECTOR_SIZE


def _header(
    blocks: int,
    disk_guid: uuid.UUID,
    my_lba: int,
    alternate_lba: int,
    entry_lba: int,
    array_crc: int,
) -> bytes:
    """Return a GPT header at ``my_lba``, with its CRC-32."""
    fields = struct.pack(
        "<8sIIIIQQQQ16sQIII",
        SIGNATURE,
        REVISION,
        HEADER_SIZE,
        0,
        0,
        my_lba,
        alternate_lba,
        2 + ENTRY_ARRAY_BLOCKS,
        blocks - 2 - ENTRY_ARRAY_BLOCKS,
        disk_guid.bytes_le,
        entry_lba,
        ENTRY_COUNT,
        ENTRY_SIZE,
        array_crc,
    )
    return fields[:16] + struct.pack("<I", zlib.crc32(fields)) + fields[20:]


def mbr_record(type_: int, first_lba: int, sectors: int) -> bytes:
    """Return an MBR partition record of ``type_`` from ``first_lba`` for ``sectors``
    sectors, with the CHS addresses that UEFI gives a protective record."""
    return struct.pack(
        "<B3sB3sII", 0, b"\x00\x02\x00", type_, b"\xff\xff\xff", first_lba, sectors
    )


def gpt_image(
    blocks: int, disk_guid: uuid.UUID, partitions: Sequence[Partition]
) -> bytes:
    """Return a disk of ``blocks`` 512-byte blocks with a protective MBR, a primary GPT
    header and entry array of 128 entries, ``partitions`` in that order, and the backup
    array and header at the disk's end, as UEFI lays them out."""
    image = bytearray(blocks * SECTOR_SIZE)
    image[MBR_RECORDS_OFFSET : MBR_RECORDS_OFFSET + MBR_RECORD_SIZE] = mbr_record(
        PROTECTIVE_TYPE, 1, min(blocks - 1, 0xFFFF_FFFF)
    )
    image[510:512] = MBR_SIGNATURE
    array = bytearray(ENTRY_COUNT * ENTRY_SIZE)
    for index, partition in enumerate(partitions):
        struct.pack_into(
            "<16s16sQQQ",
            array,
            index * ENTRY_SIZE,
            LINUX_DATA.bytes_le,
            partition.unique_guid.bytes_le,
            partition.first_lba,
            partition.last_lba,
            0,
        )
        image[partition.offset : partition.offset + len(partition.contents)] = (
            partition.contents
        )
    array_crc = zlib.crc32(array)
    backup_entry_lba = blocks - 1 - ENTRY_ARRAY_BLOCKS
    for my_lba, alternate_lba, entry_lba in (
        (1, blocks - 1, 2),
        (blocks - 1, 1, backup_entry_lba),
    ):
        header = _header(blocks, disk_guid, my_lba, alternate_lba, entry_lba, array_crc)
        image[my_lba * SECTOR_SIZE : my_lba * SECTOR_SIZE + HEADER_SIZE] = header
        image[entry_lba * SECTOR_SIZE : entry_lba * SECTOR_SIZE + len(array)] = array
    return bytes(image)


def with_mbr(image: bytes, records: Sequence[bytes]) -> bytes:
    """Return ``image`` with an MBR of ``records`` in place of its protective MBR."""
    if len(records) > 4:
        raise ValueError("an MBR holds at most four records")
    replaced = bytearray(image)
    table = b"".join(records).ljust(4 * MBR_RECORD_SIZE, b"\x00")
    replaced[MBR_RECORDS_OFFSET : MBR_RECORDS_OFFSET + len(table)] = table
    replaced[510:512] = MBR_SIGNATURE
    return bytes(replaced)
