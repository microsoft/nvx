"""Check that executables are static ELF files that need no program interpreter.

The guest binaries run in initramfs images without a dynamic loader, so each
must be a 64-bit little-endian ELF executable without a ``PT_INTERP`` program
header and without ``DT_NEEDED`` entries. A static position-independent
executable has a ``PT_DYNAMIC`` segment for its own relocations, which is
allowed. Each must also keep its stack non-executable, which a
``PT_GNU_STACK`` program header with ``PF_X`` would override.
"""

from __future__ import annotations

import struct
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

from .common import ScriptError

_ELF_MAGIC = b"\x7fELF"
_ELFCLASS64 = 2
_ELFDATA2LSB = 1
_EXECUTABLE_TYPES = {2: "EXEC", 3: "DYN"}
_PT_DYNAMIC = 2
_PT_INTERP = 3
_PT_GNU_STACK = 0x6474E551
_PF_X = 0x1
_PN_XNUM = 0xFFFF
_DT_NULL = 0
_DT_NEEDED = 1
_HEADER = struct.Struct("<16sHHIQQQIHHHHHH")
_PROGRAM_HEADER = struct.Struct("<IIQQQQQQ")
_DYNAMIC_ENTRY = struct.Struct("<qQ")


@dataclass(frozen=True)
class ElfExecutable:
    """The linkage of an ELF executable."""

    path: Path
    size: int
    elf_type: str
    interpreter: str | None
    needed: int
    dynamic: bool
    executable_stack: bool
    machine: int

    @property
    def static(self) -> bool:
        """Whether the executable runs without a dynamic loader or libraries."""
        return self.interpreter is None and self.needed == 0

    def describe(self) -> str:
        linkage = "static-pie" if self.dynamic else "static"
        if not self.static:
            details: list[str] = []
            if self.interpreter is not None:
                details.append(f"interpreter {self.interpreter}")
            if self.needed:
                details.append(f"{self.needed} needed libraries")
            linkage = "dynamic: " + ", ".join(details)
        stack = ", executable stack" if self.executable_stack else ""
        return f"{self.path}: ELF {self.elf_type}, {self.size} bytes, {linkage}{stack}"


def read_elf_executable(path: Path) -> ElfExecutable:
    """Read the type and dynamic linkage of the ELF executable at ``path``."""
    data = path.read_bytes()
    try:
        return _parse(path, data)
    except struct.error as error:
        raise ValueError(f"{path} is a truncated ELF file") from error


def _parse(path: Path, data: bytes) -> ElfExecutable:
    if data[:4] != _ELF_MAGIC:
        raise ValueError(f"{path} is not an ELF file")
    header = _HEADER.unpack_from(data)
    identity: bytes = header[0]
    if identity[4] != _ELFCLASS64 or identity[5] != _ELFDATA2LSB:
        raise ValueError(f"{path} is not a 64-bit little-endian ELF file")
    elf_type = _EXECUTABLE_TYPES.get(header[1])
    if elf_type is None:
        raise ValueError(f"{path} is not an ELF executable (type {header[1]})")
    program_offset: int = header[5]
    program_entry_size: int = header[9]
    program_count: int = header[10]
    if program_count == _PN_XNUM:
        raise ValueError(f"{path} uses extended program header numbering")
    if program_count and program_entry_size < _PROGRAM_HEADER.size:
        raise ValueError(f"{path} has malformed program headers")
    interpreter = None
    needed = 0
    dynamic = False
    executable_stack = False
    for index in range(program_count):
        segment_type, flags, offset, _, _, file_size, _, _ = (
            _PROGRAM_HEADER.unpack_from(
                data, program_offset + index * program_entry_size
            )
        )
        segment = data[offset : offset + file_size]
        if len(segment) != file_size:
            raise ValueError(f"{path} is a truncated ELF file")
        if segment_type == _PT_INTERP:
            interpreter = segment.split(b"\0", 1)[0].decode("utf-8", "replace")
        elif segment_type == _PT_DYNAMIC:
            dynamic = True
            for tag, _ in _DYNAMIC_ENTRY.iter_unpack(
                segment[: len(segment) - len(segment) % _DYNAMIC_ENTRY.size]
            ):
                if tag == _DT_NULL:
                    break
                if tag == _DT_NEEDED:
                    needed += 1
        elif segment_type == _PT_GNU_STACK:
            executable_stack = bool(flags & _PF_X)
    return ElfExecutable(
        path=path,
        size=len(data),
        elf_type=elf_type,
        interpreter=interpreter,
        needed=needed,
        dynamic=dynamic,
        executable_stack=executable_stack,
        machine=header[2],
    )


def check_static_executables(
    paths: Sequence[Path], *, expected_machine: int | None = None
) -> list[ElfExecutable]:
    """Require static, non-executable-stack files and the selected guest's machine."""
    executables = [read_elf_executable(path) for path in paths]
    for executable in executables:
        print(executable.describe())
    if expected_machine is not None:
        wrong_machine = [
            str(executable.path)
            for executable in executables
            if executable.machine != expected_machine
        ]
        if wrong_machine:
            raise ScriptError(
                f"executables have the wrong ELF machine (expected {expected_machine}): "
                + ", ".join(wrong_machine)
            )
    dynamic = [
        str(executable.path) for executable in executables if not executable.static
    ]
    if dynamic:
        raise ScriptError("not static executables: " + ", ".join(dynamic))
    executable_stacks = [
        str(executable.path)
        for executable in executables
        if executable.executable_stack
    ]
    if executable_stacks:
        raise ScriptError(
            "executables with an executable stack: " + ", ".join(executable_stacks)
        )
    return executables
