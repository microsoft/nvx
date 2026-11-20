"""Shared helpers for the host tooling's unit tests."""

import contextlib
import os
import struct
from collections.abc import Generator
from unittest.mock import patch

from nvx_tools import nvx_cli


@contextlib.contextmanager
def without_nvx_variable() -> Generator[None]:
    with patch.dict(os.environ):
        os.environ.pop(nvx_cli.VARIABLE, None)
        yield


def elf_file(
    segments: tuple[tuple[int, bytes], ...] = ((1, b"code"),),
    *,
    elf_type: int = 2,
    elf_class: int = 2,
    stack_flags: int | None = None,
) -> bytes:
    """Returns a minimal ELF file with one program header for each segment, and
    a ``PT_GNU_STACK`` header with ``stack_flags`` unless it is ``None``."""
    headers = [(segment_type, 4, payload) for segment_type, payload in segments]
    if stack_flags is not None:
        headers.append((0x6474E551, stack_flags, b""))
    header_size, entry_size = 64, 56
    payload_offset = header_size + entry_size * len(headers)
    program_headers = b""
    payloads = b""
    for segment_type, flags, payload in headers:
        program_headers += struct.pack(
            "<IIQQQQQQ",
            segment_type,
            flags,
            payload_offset + len(payloads),
            0,
            0,
            len(payload),
            len(payload),
            8,
        )
        payloads += payload
    identity = b"\x7fELF" + bytes([elf_class, 1, 1]) + bytes(9)
    header = struct.pack(
        "<16sHHIQQQIHHHHHH",
        identity,
        elf_type,
        62,
        1,
        0,
        header_size,
        0,
        0,
        header_size,
        entry_size,
        len(headers),
        64,
        0,
        0,
    )
    return header + program_headers + payloads
