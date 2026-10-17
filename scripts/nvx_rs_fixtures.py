"""Generate the golden fixtures and Python-parity tables of nvx-rs.

nvx-rs ports Python code that nvx.py runs, and must accept the same inputs and
give the same outputs and messages. This script records what that Python code,
and the parts of Python that it relies on, do with fixed cases and seeded random
corpora, so that the Rust tests replay them without Python:

- nvx-rs/src/host/python/unicode.rs: the Unicode tables of str.isprintable and
  int(), from this interpreter's Unicode database.
- nvx-rs/tests/fixtures/python.json: the Python semantics that the ports keep:
  str methods, int(), uuid.UUID, pathlib and ntpath, json.loads, and the UTF-8
  codec.
- nvx-rs/tests/fixtures/sandbox.json: the launch profile of
  scripts/nvx_tools/sandbox.py, for both pathlib flavors.
- nvx-rs/tests/fixtures/sandbox_options.json: the sandbox helpers of
  scripts/nvx.py.
- nvx-rs/tests/fixtures/egress-policy.json: scripts/nvx_tools/egress_policy.py
  over every policy that scripts/test_egress_policy.py compiles, hand-written
  edge cases, and a seeded random corpus, and how pathlib prints policy paths.
- nvx-rs/tests/fixtures/cli.json: `nvx.py sandbox`, run as a command in fresh
  directories with the fake OpenVMM of nvx-rs, nvx-fake-openvmm, and what the
  Python functions behind `nvx-rs egress compile` and `nvx-rs artifacts` give:
  exit statuses, standard output and error, and the files that each step
  leaves, such as the state directory of a sandbox, with temporary paths,
  process IDs, and random identifiers replaced by placeholders. Its `posix` and
  `windows` sections hold the scenarios of each kind of host, recorded there:
  the script rewrites or checks the section of the host that runs it and keeps
  the other. It builds nvx-rs and nvx-fake-openvmm with cargo, then also plays
  every scenario with nvx-rs, and each interoperability scenario, a sandbox's
  lifecycle, with nvx.py and nvx-rs taking turns both ways, and fails if any
  step differs from nvx.py.

The outputs depend on the interpreter, so run the script with CPython 3.14, from
any directory:

    python3 scripts/nvx_rs_fixtures.py           # rewrite every generated file
    python3 scripts/nvx_rs_fixtures.py --check   # fail if any is out of date

Run it on both Linux and Windows when the CLI changes, so that both sections of
nvx-rs/tests/fixtures/cli.json are current.

With --output, it writes or checks only an egress policy corpus of
--random-cases random policies at that path, such as a larger corpus for the
NVX_EGRESS_POLICY_FIXTURE variable of the Rust tests.

Windows paths are recorded with PureWindowsPath, whose ntpath functions are C
functions that index UTF-16 code units on Windows and Python functions that
index code points elsewhere. The corpora have no character beyond the Basic
Multilingual Plane before a colon, on which the two differ, so the script writes
the same files on Windows and Linux.
"""

# pyright: reportPrivateUsage=false

from __future__ import annotations

import argparse
import contextlib
import difflib
import hashlib
import inspect
import io
import ipaddress
import json
import ntpath
import os
import random
import re
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import time
import unicodedata
import unittest
import uuid
from collections.abc import Callable, Generator, Iterable, Iterator, Sequence
from pathlib import Path, PurePath, PurePosixPath, PureWindowsPath
from typing import AnyStr, cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import nvx  # noqa: E402
import test_egress_policy  # noqa: E402
from nvx_tools import (  # noqa: E402
    build_constants,
    common,
    control_session,
    guests,
    sandbox,
    sandbox_lifecycle,
)
from nvx_tools.common import ScriptError  # noqa: E402
from nvx_tools.egress_policy import (  # noqa: E402
    MAX_POLICY_FILE_SIZE,
    MAX_RULES_PER_ACTION,
    CompiledEgressPolicy,
    compile_policy_file,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
UNICODE_TABLES = REPO_ROOT / "nvx-rs" / "src" / "host" / "python" / "unicode.rs"
FIXTURE_DIR = REPO_ROOT / "nvx-rs" / "tests" / "fixtures"
EGRESS_POLICY_FIXTURE = FIXTURE_DIR / "egress-policy.json"
# The interpreter whose behavior the fixtures record.
PYTHON = (3, 14)
MAX_CODE_POINT = 0x10FFFF
SEED = 359
# The fixed current directories of the two flavors.
CWDS = {"posix": "/fixture/cwd", "windows": "C:\\fixture\\cwd"}
# The contents of the distro layer that the systemd policy cases hash.
DISTRO_LAYER = "distro"
RANDOM_CASES = 200
# Messages name the policy file; the fixture writes its path as this placeholder.
PATH_PLACEHOLDER = "{path}"
# Runs of one character or byte at least this long are recorded as a repetition.
MIN_REPEAT = 256
# Tests whose policies are too large to record. Their tests in
# `scripts/test_egress_policy.py`, and the Rust ports of those tests, pin the
# complete outcome of each.
LARGE_VECTORS = frozenset(
    (
        "test_address_only_rule_prunes_redundant_port_events_before_sweep",
        "test_delays_fragment_conversion_until_after_covering_union",
    )
)

Entry = dict[str, object]
Rule = dict[str, object]
Policy = dict[str, list[Rule]]
Network = ipaddress.IPv4Network | ipaddress.IPv6Network

Case = dict[str, object]


class _PosixFixturePath(PurePosixPath):
    """A POSIX path whose current directory is fixed."""

    @classmethod
    def cwd(cls) -> _PosixFixturePath:
        return cls(CWDS["posix"])


class _WindowsFixturePath(PureWindowsPath):
    """A Windows path whose current directory is fixed."""

    @classmethod
    def cwd(cls) -> _WindowsFixturePath:
        return cls(CWDS["windows"])


FLAVOR_PATHS: dict[str, type[PurePath]] = {
    "posix": PurePosixPath,
    "windows": PureWindowsPath,
}
FIXTURE_PATHS: dict[str, type[PurePath]] = {
    "posix": _PosixFixturePath,
    "windows": _WindowsFixturePath,
}


@contextlib.contextmanager
def _flavor(name: str) -> Generator[None]:
    """Make sandbox.py use the paths of a flavor, with its fixed current directory."""
    with patch.object(sandbox, "Path", FIXTURE_PATHS[name]):
        yield


def _error(function: Callable[..., object], *arguments: object) -> str | None:
    """Return the message of the ScriptError that function raises, if any."""
    try:
        function(*arguments)
    except common.ScriptError as error:
        return str(error)
    return None


# Unicode tables.


def _ranges(predicate: Callable[[int], bool], start: int) -> list[tuple[int, int]]:
    """Return the inclusive ranges of code points from start that satisfy predicate."""
    ranges: list[tuple[int, int]] = []
    first: int | None = None
    for code in range(start, MAX_CODE_POINT + 1):
        if predicate(code):
            if first is None:
                first = code
        elif first is not None:
            ranges.append((first, code - 1))
            first = None
    if first is not None:
        ranges.append((first, MAX_CODE_POINT))
    return ranges


def space_ranges() -> list[tuple[int, int]]:
    return _ranges(lambda code: chr(code).isspace(), 0)


def decimal_starts() -> list[int]:
    starts: list[int] = []
    for first, last in _ranges(lambda code: chr(code).isdecimal(), 0):
        if (last - first + 1) % 10:
            raise SystemExit(f"decimal digits {first:#x}-{last:#x} are not runs of ten")
        for start in range(first, last + 1, 10):
            values = [unicodedata.decimal(chr(start + value)) for value in range(10)]
            if values != list(range(10)):
                raise SystemExit(f"decimal digits at {start:#x} are not 0 to 9")
            starts.append(start)
    return starts


def non_printable_ranges() -> list[tuple[int, int]]:
    return _ranges(lambda code: not chr(code).isprintable(), 0x80)


def unicode_tables_source() -> str:
    """Return the Rust source of the Unicode tables."""
    starts = decimal_starts()
    non_printable = non_printable_ranges()
    lines = [
        "//! Unicode properties of Python's `str`, from the Unicode "
        f"{unicodedata.unidata_version} database",
        f"//! of Python {sys.version_info.major}.{sys.version_info.minor}.",
        "//!",
        "//! Generated by `scripts/nvx_rs_fixtures.py`; do not edit.",
        "",
        "/// The first code point of each run of ten decimal digits, of category Nd, "
        "which `int`",
        "/// reads as the digits zero to nine.",
        "#[rustfmt::skip]",
        "pub(crate) const DECIMAL_STARTS: &[u32] = &[",
    ]
    for index in range(0, len(starts), 8):
        row = ", ".join(f"0x{start:05X}" for start in starts[index : index + 8])
        lines.append(f"    {row},")
    lines.extend(
        (
            "];",
            "",
            "/// The inclusive ranges of the code points from U+0080 for which "
            "`str.isprintable` is false,",
            "/// in order: `repr` escapes them.",
            "#[rustfmt::skip]",
            "pub(crate) const NON_PRINTABLE: &[(u32, u32)] = &[",
        )
    )
    for index in range(0, len(non_printable), 4):
        row = ", ".join(
            f"(0x{first:05X}, 0x{last:05X})"
            for first, last in non_printable[index : index + 4]
        )
        lines.append(f"    {row},")
    lines.extend(("];", ""))
    return "\n".join(lines)


# Random inputs.


def _random_text(
    rng: random.Random, tokens: Sequence[str], minimum: int, maximum: int
) -> str:
    return "".join(rng.choice(tokens) for _ in range(rng.randint(minimum, maximum)))


def _mutate(rng: random.Random, text: str, tokens: Sequence[str]) -> str:
    """Return text with a few random insertions, deletions, and replacements."""
    characters = list(text)
    for _ in range(rng.randint(0, 3)):
        operation = rng.randrange(3)
        position = rng.randint(0, len(characters))
        if operation == 0:
            characters.insert(position, rng.choice(tokens))
        elif characters and position < len(characters):
            if operation == 1:
                del characters[position]
            else:
                characters[position] = rng.choice(tokens)
    return "".join(characters)


def _sometimes(
    rng: random.Random, valid: Sequence[str], invalid: Sequence[str], rate: float = 0.1
) -> str:
    """Return a valid value, or at the given rate an invalid one."""
    return rng.choice(invalid if rng.random() < rate else valid)


def _random_number(rng: random.Random) -> str:
    """Return a decimal number, which may have underscores, leading zeros, or Unicode digits."""
    digits = str(rng.choice((rng.randint(0, 99), rng.randint(0, 2**32 + 9), 65534)))
    if rng.random() < 0.2:
        digits = "0" + digits
    if rng.random() < 0.2 and len(digits) > 1:
        position = rng.randint(1, len(digits) - 1)
        digits = f"{digits[:position]}_{digits[position:]}"
    if rng.random() < 0.1:
        digits = "".join(
            chr(0xFF10 + int(digit)) if digit.isdigit() else digit for digit in digits
        )
    return digits


TARGET_COMPONENTS = ("work", "a", "srv", "cache", "é", "data-1", "run", "nvx", "etc")
POLICY_KINDS = ("denied", "allowed", "writable")


def _target(rng: random.Random) -> str:
    """Return a guest target, which one time in seven has a random irregular part."""
    components = [rng.choice(TARGET_COMPONENTS) for _ in range(rng.randint(1, 3))]
    if rng.random() < 1 / 7:
        position = rng.randrange(len(components))
        components[position] = _random_text(rng, TARGET_TOKENS, 0, 3)
    return "/" + "/".join(components)


TEXT_TOKENS = (
    "0",
    "1",
    "7",
    "9",
    "a",
    "F",
    "x",
    "X",
    "_",
    "+",
    "-",
    " ",
    "\t",
    "\n",
    "\x0b",
    "\x1c",
    "\x1f",
    "\x85",
    "\xa0",
    "\u2003",
    "\u3000",
    "\u200b",
    "\u0663",
    "\uff11",
    "\U0001d7ce",
    "\x00",
    "\x7f",
    "\xad",
    "\u0378",
    "\u2028",
    "\U0001f600",
    "\U000e0001",
    "\ufeff",
    "'",
    '"',
    "\\",
    "{",
    "}",
    ":",
    ",",
    "=",
    "é",
    "urn:",
    "uuid:",
    "0x",
)
HEX = "0123456789abcdefABCDEF"
PATH_TOKENS = (
    "a",
    "b",
    "é",
    "C",
    "c",
    ":",
    "/",
    "//",
    "\\",
    "\\\\",
    ".",
    "..",
    "?",
    "UNC",
    "unc",
    " ",
    "x,y",
    ";",
    "\x00",
)
TARGET_TOKENS = (
    "/",
    "a",
    "work",
    "proc",
    "etc",
    "machine-id",
    "run",
    "nvx",
    "shares",
    ".",
    "..",
    " ",
    "\u3000",
    "\x1c",
    "=",
    ",",
    "\\",
    "\x00",
    "é",
)


# Python's string, integer, and UUID operations.


def _int_outcome(text: str) -> Case:
    try:
        return {"int": str(int(text, 10))}
    except ValueError as error:
        return {"int_error": str(error)}


def _uuid_outcome(text: str) -> str | None:
    try:
        return str(uuid.UUID(text))
    except ValueError:
        return None


def string_cases(rng: random.Random) -> list[Case]:
    texts = [
        "",
        " ",
        "007",
        "+5",
        "-5",
        " 5 ",
        "5_000",
        "_5",
        "5_",
        "5__0",
        "0x5",
        "1e3",
        "\u20035\u3000",
        "\uff15",
        "\u0664\u0662",
        "\x1c42\x1f",
        "1\x002",
        "it's",
        'say "hi"',
        "both ' and \"",
        "a\\b",
        "\t\n\r\x00\x7f\x1b",
        "é\xa0\xad",
        "\u2028\u200b\u0378",
        "\U0001f600\U000e0001",
        "1" * 4300,
        "0" * 4300 + "1",
        "x" * 300,
        "12345678-1234-5678-1234-567812345678",
        "{12345678-1234-5678-1234-567812345678}",
        "urn:uuid:12345678-1234-5678-1234-567812345678",
        "12345678123456781234567812345678",
        "12345678-1234-5678-1234-567812345678".upper(),
        "uuuuid:id:12345678123456781234567812345678",
        "{urn:uuid:12345678-1234-5678-1234-567812345678}",
        "0x" + "1" * 30,
        "+" + "1" * 31,
        "-" + "1" * 31,
        " " + "a" * 31,
        "1_" + "1" * 30,
        "\u0661" + "0" * 31,
        "0b" + "1" * 30,
    ]
    for _ in range(300):
        texts.append(_random_text(rng, TEXT_TOKENS, 0, 12))
    canonical = "12345678-1234-5678-1234-567812345678"
    for _ in range(200):
        text = rng.choice(
            (
                canonical,
                canonical.replace("-", ""),
                f"{{{canonical}}}",
                f"urn:uuid:{canonical}",
                canonical.upper(),
            )
        )
        texts.append(_mutate(rng, text, (*HEX, *TEXT_TOKENS)))
    for _ in range(150):
        texts.append(_mutate(rng, str(rng.randint(-(10**12), 10**12)), TEXT_TOKENS))
    cases: list[Case] = []
    for text in texts:
        case: Case = {
            "text": text,
            "repr": repr(text),
            "split": text.split(),
            "strip": text.strip(),
            "uuid": _uuid_outcome(text),
        }
        case.update(_int_outcome(text))
        cases.append(case)
    return cases


# pathlib.


def path_cases(rng: random.Random) -> list[Case]:
    texts = [
        "",
        ".",
        "./a",
        "a/./b/",
        "a//b",
        "a/../b",
        "/a",
        "//a",
        "///a",
        "a\\b",
        "c:x",
        "C:/a/b/",
        "c:",
        "./c:x",
        "//server/share",
        "//server/share/x",
        "\\\\?\\C:\\x",
        "//?/UNC/server/share",
        "//?/UNC/server",
        "\\\\.\\device\\x",
        "\\a",
        "é:x",
        "\\\\\\x",
        "//?/",
        "x:y:z",
    ]
    for _ in range(300):
        texts.append(_random_text(rng, PATH_TOKENS, 0, 8))
    cases: list[Case] = []
    for flavor, path_class in FLAVOR_PATHS.items():
        for text in texts:
            path = path_class(text)
            cases.append(
                {
                    "flavor": flavor,
                    "text": text,
                    "str": str(path),
                    "parts": list(path.parts),
                    "absolute": path.is_absolute(),
                    "name": path.name,
                    "parents": [str(parent) for parent in path.parents],
                }
            )
    return cases


def join_cases(rng: random.Random) -> list[Case]:
    pairs = [
        ("/a/b", "c"),
        ("/a/b", "/c"),
        ("C:\\cwd", "a/b"),
        ("C:\\cwd", "C:x"),
        ("C:\\cwd", "c:x"),
        ("C:\\cwd", "D:x"),
        ("C:\\cwd", "\\x"),
        ("//server/share", "x"),
        ("C:", "x"),
        ("", "x"),
        ("a", ""),
    ]
    for _ in range(200):
        pairs.append(
            (
                _random_text(rng, PATH_TOKENS, 0, 6),
                _random_text(rng, PATH_TOKENS, 0, 6),
            )
        )
    cases: list[Case] = []
    for flavor, path_class in FLAVOR_PATHS.items():
        for left, right in pairs:
            cases.append(
                {
                    "flavor": flavor,
                    "left": left,
                    "right": right,
                    "str": str(path_class(left) / right),
                }
            )
    return cases


def equality_cases(rng: random.Random) -> list[Case]:
    pairs = [("C:\\A", "c:/a"), ("C:\\a", "C:\\b"), ("\\\\S\\x\\Y", "//s/X/y")]
    for _ in range(100):
        left = _random_text(rng, (*PATH_TOKENS, "A", "B"), 0, 5)
        pairs.append((left, rng.choice((left.lower(), left.upper(), left))))
    return [
        {
            "left": left,
            "right": right,
            "equal": PureWindowsPath(left) == PureWindowsPath(right),
        }
        for left, right in pairs
    ]


def normpath_cases(rng: random.Random) -> list[Case]:
    texts = [
        "",
        ".",
        "a",
        "a\\",
        "A//B",
        "A/./B",
        "A/foo/../B",
        "C:\\",
        "C:",
        "C:a\\..\\..",
        "C:\\..\\a",
        "..\\..\\a",
        "a\\..\\..\\b",
        "\\\\server\\share\\",
        "\\\\server\\share\\a\\..",
        ".\\a\\.\\b\\",
        "\\",
        "./..",
        "C:./x",
    ]
    for _ in range(250):
        texts.append(_random_text(rng, PATH_TOKENS, 0, 10))
    return [{"text": text, "normpath": ntpath.normpath(text)} for text in texts]


# The launch profile.


def identity_cases(rng: random.Random) -> list[Case]:
    values = [
        "1000:1001",
        "0:0",
        "1000",
        "1:2:3",
        "a:1",
        "1:",
        " +1_000 :\uff11\uff10",
        "4294967295:1",
        "4294967296:1",
        "-1:1",
        "1:0",
        "1:0x10",
        "1" * 4301 + ":1",
    ]
    tokens = ("0", "1", "5", "9", "_", "+", "-", " ", "\u3000", "\u0661", ":", "x")
    for _ in range(100):
        values.append(_random_text(rng, tokens, 0, 12))
    spaces = ("", " ", "\t", "\u3000")
    for _ in range(100):
        fields = [
            rng.choice(spaces)
            + rng.choice(("", "", "+", "-"))
            + _random_number(rng)
            + rng.choice(spaces)
            for _ in range(2)
        ]
        values.append(":".join(fields))
    cases: list[Case] = []
    for value in values:
        try:
            identity = sandbox.parse_workload_identity(value)
            cases.append({"value": value, "identity": list(identity)})
        except common.ScriptError as error:
            cases.append({"value": value, "error": str(error)})
    return cases


def layer_cases(rng: random.Random) -> list[Case]:
    values = [
        "distro,distro.erofs,11111111-1111-1111-1111-111111111111",
        "custom,layers//custom.erofs,{22222222-2222-2222-2222-22222222222A}",
        "distro,C:/l/d.erofs,urn:uuid:11111111111111111111111111111111",
        "unknown,layer.erofs,11111111-1111-1111-1111-111111111111",
        "distro,layer.erofs,not-a-uuid",
        "distro,,1",
        "distro",
        "distro,a",
        "distro,a,b,c",
        "runtime,a\\b/./c,11111111111111111111111111111111",
    ]
    roles = ("distro", "runtime", "custom")
    uuid_forms: tuple[Callable[[uuid.UUID], str], ...] = (
        str,
        lambda value: str(value).upper(),
        lambda value: value.hex,
        lambda value: f"{{{value}}}",
        lambda value: value.urn,
    )
    for _ in range(150):
        role = _sometimes(rng, roles, ("Distro", "", "other"))
        path = _random_text(rng, PATH_TOKENS, 1, 5) if rng.random() < 0.3 else "l.erofs"
        identifier = rng.choice(uuid_forms)(uuid.UUID(int=rng.getrandbits(128)))
        if rng.random() < 0.3:
            identifier = _mutate(rng, identifier, (*HEX, "-", "{", "}", " "))
        values.append(f"{role},{path},{identifier}")
    cases: list[Case] = []
    for flavor in FIXTURE_PATHS:
        with _flavor(flavor):
            for value in values:
                try:
                    layer = sandbox.SandboxLayer.parse(value)
                    cases.append(
                        {
                            "flavor": flavor,
                            "value": value,
                            "role": layer.role,
                            "path": str(layer.path),
                            "uuid": layer.uuid,
                        }
                    )
                except common.ScriptError as error:
                    cases.append(
                        {"flavor": flavor, "value": value, "error": str(error)}
                    )
    return cases


def mount_value_cases(rng: random.Random) -> list[Case]:
    values = [
        "/workspace",
        "/workspace,",
        "/workspace,,rw",
        "/workspace,host,rx",
        "/workspace,host,rw,extra",
        "/workspace,alpha,beta",
        "/data,/srv/data,rw,ro",
        "/data,/srv/data,,rw",
        "/data,/srv/data,rw",
        "/data,/srv/data",
        ",",
        "",
    ]
    for _ in range(100):
        values.append(_random_text(rng, (*PATH_TOKENS, ",", "ro", "rw"), 0, 8))
    for _ in range(100):
        host_path = _random_text(rng, (*PATH_TOKENS, ","), 1, 4)
        mode = _sometimes(rng, ("", ",ro", ",rw"), (",", ",rx", ",RW"))
        values.append(f"{_target(rng)},{host_path}{mode}")
    cases: list[Case] = []
    for value in values:
        try:
            target, host_path, access = sandbox.parse_mount_value(value)
            cases.append(
                {
                    "value": value,
                    "target": target,
                    "host_path": host_path,
                    "access": access,
                }
            )
        except common.ScriptError as error:
            cases.append({"value": value, "error": str(error)})
    return cases


def target_cases(rng: random.Random) -> list[Case]:
    targets = [
        "",
        "/",
        "workspace",
        "/workspace/",
        "/work//space",
        "/work/./space",
        "/work/../space",
        "/work space",
        "/work=space",
        "/work\\space",
        "/" + "x" * 4096,
        "/" + "x" * 4095,
        "/" + "é" * 2048,
        "/proc",
        "/proc/self",
        "/sys/fs",
        "/dev/shm",
        "/.nvx-agent/tools",
        "/etc",
        "/etc/machine-id",
        "/etc/machine-id/child",
        "/workspace",
        "/procfs",
        "/etc/app",
        "/run",
        "/run/nvx",
        "/run/nvx/time",
        "/run/nvx/shares",
    ]
    for _ in range(100):
        targets.append(_random_text(rng, TARGET_TOKENS, 0, 8))
    for _ in range(100):
        targets.append(_target(rng))
    return [
        {"target": target, "error": _error(sandbox.validate_mount_target, target)}
        for target in targets
    ]


def run_target_cases(rng: random.Random) -> list[Case]:
    lists = [
        ["/workspace", "/opt/hostedtoolcache", "/srv/cache"],
        ["/workspace", "/workspace/cache"],
        ["/run/nvx", "/b"],
        ["/run/nvx/shares/x"],
        ["/a b", "/b"],
        ["relative", "/b"],
        ["/" + "x" * 5000],
        [],
    ]
    for _ in range(40):
        lists.append(
            [_random_text(rng, TARGET_TOKENS, 1, 6) for _ in range(rng.randint(1, 3))]
        )
    for _ in range(80):
        lists.append([_target(rng) for _ in range(rng.randint(1, 4))])
    return [
        {
            "targets": targets,
            "error": _error(nvx._validate_run_share_targets, targets),
        }
        for targets in lists
    ]


def child_name_cases(rng: random.Random) -> list[Case]:
    pairs = [(0, "/workspace"), (1, "/workspace"), (999, "/" + "x" * 1000)]
    for _ in range(50):
        pairs.append((rng.randint(0, 50), _random_text(rng, TARGET_TOKENS, 1, 8)))
    return [
        {
            "index": index,
            "target": target,
            "name": sandbox.aggregate_child_name(index, target),
        }
        for index, target in pairs
    ]


def _mount_spec(rng: random.Random) -> Case:
    """Return a share whose parts are each valid, except one time in ten."""
    target = _sometimes(rng, (_target(rng),), ("/proc", "/a b", "/etc"))
    host_path = _sometimes(
        rng,
        ("work", "tools/cache", "/srv/data", "C:\\data\\x,y", "C:/w", "w//x/."),
        ("a/../b", "a\x00b", _random_text(rng, PATH_TOKENS, 1, 5)),
    )
    access = _sometimes(rng, ("", ",ro", ",rw"), (",rx",))
    kind = rng.choice(("directory", "directory", "directory", "file"))
    denied: list[str] = []
    allowed: list[str] = []
    writable: list[str] = []
    if kind == "directory" or rng.random() < 0.1:
        denied = rng.sample(
            ("logs", "secrets", "/abs/out", "C:\\abs\\out"), rng.randint(0, 2)
        )
        if denied and rng.random() < 0.5:
            allowed = ["logs/payloads"]
        if access == ",rw" and rng.random() < 0.5:
            writable = rng.sample(("out", "cache", "/abs/out"), rng.randint(1, 2))
    if rng.random() < 0.1:
        rng.choice((denied, allowed, writable)).append(
            rng.choice(("", ".", "a\x00", "logs", "out"))
        )
    return {
        "value": f"{target},{host_path}{access}",
        "denied": denied,
        "allowed": allowed,
        "writable": writable,
        "owner": rng.choice(("vmm", "vmm", "caller")),
        "kind": kind,
    }


def _parse_mount(spec: Case) -> sandbox.SandboxMount:
    return sandbox.SandboxMount.parse(
        cast(str, spec["value"]),
        tuple(cast(list[str], spec["denied"])),
        cast(str, spec["owner"]),
        allowed_paths=tuple(cast(list[str], spec["allowed"])),
        writable_paths=tuple(cast(list[str], spec["writable"])),
        kind=cast(str, spec["kind"]),
    )


def _mount_fields(mount: sandbox.SandboxMount) -> Case:
    return {
        "guest_target": mount.guest_target,
        "host_path": str(mount.host_path),
        "access": mount.access,
        "denied": list(mount.denied_paths),
        "allowed": list(mount.allowed_paths),
        "writable": list(mount.writable_paths),
        "owner": mount.owner,
        "kind": mount.kind,
    }


def mount_cases(rng: random.Random) -> list[Case]:
    specs: list[Case] = [
        {
            "value": "/opt/hostedtoolcache,host-dir,rw",
            "denied": ["logs", "secrets"],
            "allowed": [],
            "writable": [],
            "owner": "vmm",
            "kind": "directory",
        },
        {
            "value": "/tmp/mcp-runtime,host-dir,rw",
            "denied": ["mcp-logs"],
            "allowed": ["mcp-logs/payloads"],
            "writable": ["agent", "cache"],
            "owner": "caller",
            "kind": "directory",
        },
        {
            "value": "/w,host,ro",
            "denied": [],
            "allowed": [],
            "writable": ["out"],
            "owner": "vmm",
            "kind": "directory",
        },
        {
            "value": "/w,host,rw",
            "denied": [],
            "allowed": [f"path-{index}" for index in range(129)],
            "writable": [],
            "owner": "vmm",
            "kind": "directory",
        },
        {
            "value": "/etc/app/settings.json,settings.json,ro",
            "denied": [],
            "allowed": [],
            "writable": [],
            "owner": "vmm",
            "kind": "file",
        },
    ]
    for _ in range(100):
        specs.append(_mount_spec(rng))
    cases: list[Case] = []
    for flavor in FIXTURE_PATHS:
        with _flavor(flavor):
            for spec in specs:
                case: Case = {"flavor": flavor, **spec}
                try:
                    mount = _parse_mount(spec)
                except common.ScriptError as error:
                    case["error"] = str(error)
                    cases.append(case)
                    continue
                case["mount"] = _mount_fields(mount)
                case["arguments"] = sandbox.mounts_openvmm_arguments((mount,))
                case["fragment"] = mount.command_line_fragment()
                case["absolute"] = _mount_fields(mount.absolute())
                case["absolute_policy"] = [
                    [kind, list(paths)] for kind, paths in mount.absolute_policy_paths()
                ]
                cases.append(case)
    return cases


def _launch_spec(rng: random.Random) -> Case:
    roles = rng.sample(("distro", "runtime", "custom"), rng.choice((1, 1, 2, 3)))
    if rng.random() < 0.05:
        roles.append(roles[0])
    layers = [
        f"{role},{rng.choice(('layer.erofs', 'C:/layers/x.erofs', 'a//b.erofs'))},"
        f"{uuid.UUID(int=rng.getrandbits(128))}"
        for role in roles
    ]
    argument_tokens = ("a", "--serve", "x" * 40, "é", "=", "-")
    arguments = [
        _random_text(rng, argument_tokens, 1, 3)
        for _ in range(rng.choice((0, 1, 2, 5, 20)))
    ]
    if arguments and rng.random() < 0.05:
        arguments[rng.randrange(len(arguments))] = rng.choice(("", " a", "a\x1cb"))
    mounts: list[Case] = []
    for index in range(rng.choice((0, 0, 1, 2, 3, 6))):
        kind = rng.choice(("directory", "directory", "file"))
        mounts.append(
            {
                "value": f"/share/{index:02}{'x' * rng.randint(0, 120)},"
                f"{rng.choice(('host', 'C:/host', '/srv'))}{index}"
                f"{rng.choice(('', ',ro', ',rw'))}",
                "denied": (
                    rng.choice(([], ["secrets"], ["/abs/secrets"]))
                    if kind == "directory"
                    else []
                ),
                "allowed": [],
                "writable": [],
                "owner": "vmm",
                "kind": kind,
            }
        )
    identity = [rng.choice((1000, 65534)), rng.choice((1001, 65534))]
    if rng.random() < 0.05:
        identity[rng.randrange(2)] = 0
    # Each setting is invalid one time in twenty, so most launches reach the command line.
    rate = 0.05
    return {
        "layers": layers,
        "scratch": rng.choice(("scratch.ext4", "C:/s/scratch.ext4", "s//x.ext4")),
        "entrypoint": _sometimes(
            rng,
            ("/bin/sh", "/bin/workload", "/usr/bin/python3"),
            ("bin/sh", "/bin/s h"),
            rate,
        ),
        "args": arguments,
        "hostname": _sometimes(
            rng,
            ("nvx-sandbox", "example", "a" * 63, "a-1"),
            ("Example", "a" * 64, "-a"),
            rate,
        ),
        "identity": identity,
        "memory_max": _sometimes(
            rng, ("", "", "", "268435456", str(10**30)), ("0", "-1"), rate
        )
        or None,
        "pids_max": _sometimes(
            rng, ("", "", "", "64", str(10**20 - 1)), ("0", "-5"), rate
        )
        or None,
        "mounts": mounts,
        "command_line": rng.choice(
            (
                "",
                "",
                "quiet",
                " quiet  loglevel=0 ",
                "\u3000quiet\x1c",
                "nvx_sandbox=0",
                "virtfs_dir=/x",
                "a\x00b",
                "x" * rng.randint(100, 1000),
                "é" * rng.randint(50, 500),
                "a\x1cnvx_x=1",
            )
        ),
    }


def _launch(flavor: str, spec: Case) -> sandbox.SandboxLaunch:
    """Return the launch of a specification, under the flavor's patched paths.

    The launch's layers and shares are valid, so that each case checks the launch
    itself.
    """
    layers = tuple(
        sandbox.SandboxLayer.parse(value) for value in cast(list[str], spec["layers"])
    )
    mounts = tuple(_parse_mount(mount) for mount in cast(list[Case], spec["mounts"]))
    identity = cast(list[int], spec["identity"])
    memory_max = cast(str | None, spec["memory_max"])
    pids_max = cast(str | None, spec["pids_max"])
    return sandbox.SandboxLaunch(
        layers=layers,
        scratch=cast(Path, FIXTURE_PATHS[flavor](cast(str, spec["scratch"]))),
        entrypoint=cast(str, spec["entrypoint"]),
        args=tuple(cast(list[str], spec["args"])),
        hostname=cast(str, spec["hostname"]),
        workload_identity=(identity[0], identity[1]),
        memory_max=None if memory_max is None else int(memory_max),
        pids_max=None if pids_max is None else int(pids_max),
        mounts=mounts,
    )


def _budget_specs() -> list[Case]:
    """Return launches whose kernel command lines fill the sandbox budget exactly,
    and by one byte more, with each kind of share set."""
    share_sets = (
        (),
        (("/workspace,host,rw", "directory"),),
        (("/workspace,host,rw", "directory"), ("/srv/cache,cache", "directory")),
        (("/etc/app/settings.json,settings.json", "file"),),
    )
    specs: list[Case] = []
    for shares in share_sets:
        spec: Case = {
            "layers": ["distro,layer.erofs,11111111-1111-1111-1111-111111111111"],
            "scratch": "scratch.ext4",
            "entrypoint": "/bin/sh",
            "args": [],
            "hostname": "nvx-sandbox",
            "identity": [65534, 65534],
            "memory_max": None,
            "pids_max": None,
            "mounts": [
                {
                    "value": value,
                    "denied": [],
                    "allowed": [],
                    "writable": [],
                    "owner": "vmm",
                    "kind": kind,
                }
                for value, kind in shares
            ],
            "command_line": "",
        }
        with _flavor("posix"):
            launch = _launch("posix", spec)
            fragment = sandbox.mounts_command_line_fragment(launch.mounts)
            used = len(launch.kernel_command_line("").encode()) + len(fragment.encode())
        # The user's command line comes first, with a space after it, and a NUL ends
        # the whole command line.
        room = sandbox.SANDBOX_COMMAND_LINE_MAX_SIZE - used - 2
        specs.extend({**spec, "command_line": "x" * (room + extra)} for extra in (0, 1))
    return specs


def launch_cases(rng: random.Random) -> list[Case]:
    specs = [_launch_spec(rng) for _ in range(80)]
    specs.extend(_budget_specs())
    cases: list[Case] = []
    for flavor in FIXTURE_PATHS:
        with _flavor(flavor):
            for spec in specs:
                case: Case = {"flavor": flavor, **spec}
                try:
                    launch = _launch(flavor, spec)
                except common.ScriptError as error:
                    case["error"] = str(error)
                    cases.append(case)
                    continue
                case["arguments"] = launch.openvmm_arguments()
                command_line = cast(str, spec["command_line"])
                try:
                    case["kernel_command_line"] = launch.kernel_command_line(
                        command_line
                    )
                except common.ScriptError as error:
                    case["kernel_error"] = str(error)
                cases.append(case)
    return cases


# The helpers of nvx.py.


def sandbox_mounts_cases(rng: random.Random) -> list[Case]:
    specs: list[Case] = [
        {
            "mounts": ["/workspace,work,rw", "/opt/hostedtoolcache,tools,ro"],
            "policy": [
                ["denied", 0, "logs"],
                ["allowed", 0, "logs/payloads"],
                ["writable", 0, "out"],
                ["denied", 1, "credentials"],
            ],
            "owner": None,
            "files": [],
        },
        {
            "mounts": ["/w,work,rw"],
            "policy": [["denied", None, "secrets"]],
            "owner": "caller",
            "files": [],
        },
        {
            "mounts": ["/workspace,work,rw", "/opt/hostedtoolcache,tools"],
            "policy": [["allowed", None, "logs"], ["denied", None, "secrets"]],
            "owner": None,
            "files": [],
        },
        {
            "mounts": ["/etc/app/settings.json,settings.json,rw"],
            "policy": [["writable", 0, "settings.json"]],
            "owner": None,
            "files": ["settings.json"],
        },
        {"mounts": [], "policy": [["denied", None, "x"]], "owner": None, "files": []},
    ]
    for _ in range(120):
        files = ["f.json"] if rng.random() < 0.5 else []
        values: list[str] = []
        policy: list[list[object]] = []
        if rng.random() < 0.1:
            policy.append([rng.choice(POLICY_KINDS), None, "early"])
        for index in range(rng.randint(0, 3)):
            host_path = rng.choice(("work", "tools", "f.json", "C:/x"))
            mode = rng.choice(("", ",ro", ",rw"))
            values.append(f"/m{index},{host_path}{mode}")
            if host_path in files and rng.random() < 0.9:
                continue
            denied = rng.sample(("logs", "secrets", "/d"), rng.randint(0, 2))
            policy.extend(["denied", index, path] for path in denied)
            if denied and rng.random() < 0.5:
                policy.append(["allowed", index, "logs/payloads"])
            if mode == ",rw" and rng.random() < 0.5:
                policy.append(["writable", index, "out"])
            if rng.random() < 0.1:
                policy.append(
                    [rng.choice(POLICY_KINDS), index, rng.choice(("", "logs"))]
                )
        specs.append(
            {
                "mounts": values,
                "policy": policy,
                "owner": rng.choice((None, "vmm", "caller")),
                "files": files,
            }
        )
    destinations = {
        "denied": "mount_deny",
        "allowed": "mount_allow",
        "writable": "mount_write",
    }
    cases: list[Case] = []
    for flavor in FIXTURE_PATHS:
        with _flavor(flavor):
            for spec in specs:
                files = set(cast(list[str], spec["files"]))
                namespace = argparse.Namespace(
                    mount=list(cast(list[str], spec["mounts"])),
                    mount_owner=spec["owner"],
                    mount_deny=[],
                    mount_allow=[],
                    mount_write=[],
                )
                for kind, preceding, path in cast(list[list[object]], spec["policy"]):
                    index = -1 if preceding is None else cast(int, preceding)
                    getattr(namespace, destinations[cast(str, kind)]).append(
                        (index, path)
                    )

                def kind_of(host_path: PurePath, files: set[str] = files) -> str:
                    return "file" if str(host_path) in files else "directory"

                case: Case = {"flavor": flavor, **spec}
                try:
                    with patch.object(nvx, "mount_kind", kind_of):
                        mounts = nvx._sandbox_mounts(namespace)
                    case["result"] = [_mount_fields(mount) for mount in mounts]
                except common.ScriptError as error:
                    case["error"] = str(error)
                cases.append(case)
    return cases


def environment_cases(rng: random.Random) -> list[Case]:
    lists: list[list[str]] = [
        [f"KEY{index}=value" for index in range(257)],
        [f"KEY{index}=value" for index in range(256)],
        ["NO_EQUALS"],
        ["=empty-key"],
        ["KEY=embedded\x00nul"],
        ["KEY=" + "\N{SNOWMAN}" * 1365],
        ["K=" + "v" * 4094],
        ["DUPLICATE=one", "DUPLICATE=two"],
        ["A=1", "B=2"],
        [],
    ]
    tokens = ("A", "B", "=", "", "\x00", "é", "x" * 1000)
    for _ in range(30):
        lists.append(
            [_random_text(rng, tokens, 0, 5) for _ in range(rng.randint(0, 4))]
        )
    values = ("", "1", "a=b", "é", "\N{SNOWMAN}" * 300, "x" * 1000, " spaced ")
    for _ in range(40):
        entries = [
            f"{rng.choice(('A', 'B', 'PATH', 'K_1', 'É'))}={rng.choice(values)}"
            for _ in range(rng.randint(0, 5))
        ]
        if entries and rng.random() < 0.1:
            entries[rng.randrange(len(entries))] = rng.choice(
                ("NO_EQUALS", "=x", "A=\0")
            )
        lists.append(entries)
    cases: list[Case] = []
    for entries in lists:
        try:
            control_session.encode_exec_environment(tuple(entries))
            cases.append({"entries": entries, "error": None})
        except (TypeError, ValueError) as error:
            cases.append({"entries": entries, "error": str(error)})
    return cases


def _encode_json_value(value: object) -> object:
    """Return a JSON value in the tagged form that the Rust test compares."""
    if value is None:
        return ["null"]
    if isinstance(value, bool):
        return ["bool", value]
    if isinstance(value, int):
        return ["int", str(value)]
    if isinstance(value, float):
        return ["float", struct.pack(">d", value).hex()]
    if isinstance(value, str):
        return _encode_string(value)
    if isinstance(value, list):
        return [
            "list",
            [_encode_json_value(item) for item in cast(list[object], value)],
        ]
    members = cast(dict[str, object], value)
    return [
        "dict",
        [
            [_encode_string(key), _encode_json_value(item)]
            for key, item in members.items()
        ],
    ]


def _encode_string(value: str) -> object:
    try:
        value.encode("utf-8")
    except UnicodeEncodeError:
        return ["code_points", [ord(character) for character in value]]
    return ["str", value]


JSON_SEEDS = (
    '{"format": 1, "artifact": "a.erofs", "packages": [{"name": "systemd"}]}',
    '["A=1", "B=\\u00e9", "C=\\ud83d\\ude00"]',
    "[NaN, Infinity, -Infinity, 1e400, -0.0, 1E-5, 10, -3]",
    '{"a": [true, false, null], "a": {"b": "\\n\\t\\\\\\"\\/"}}',
    '"\\ud800\\udc00\\udc00\\ud800x"',
    "[]",
    "{}",
    " \t\r\n[1 , 2 ] ",
)
JSON_TOKENS = (
    "[",
    "]",
    "{",
    "}",
    ",",
    ":",
    '"',
    "\\",
    "u",
    "d800",
    "0",
    "1",
    "-",
    ".",
    "e",
    "+",
    " ",
    "\n",
    "\r",
    "\t",
    "\x00",
    "\x1f",
    "null",
    "true",
    "NaN",
    "Infinity",
    "x",
    "é",
    "\ufeff",
)


def json_loads_cases(rng: random.Random) -> list[Case]:
    texts: list[str] = list(JSON_SEEDS)
    texts.extend(
        (
            "",
            "[",
            "[1",
            "[1,]",
            '{"a":1,}',
            "{1:2}",
            '{"a" 1}',
            '"abc',
            '"a\tb"',
            '"\\x"',
            '"\\u12"',
            '"\\u12x4"',
            '"\\ud800\\u12x4"',
            "[]\n x",
            "\ufeff[]",
            "-",
            "01",
            "1.",
            "1e",
            "1e+",
            "-Infinityx",
            "nul",
            "[" * 50 + "]" * 50,
            "1" * 4301,
            "[" + "1" * 4300 + "]",
            "1." + "1" * 5000,
        )
    )
    for _ in range(400):
        texts.append(_mutate(rng, rng.choice(JSON_SEEDS), JSON_TOKENS))
    cases: list[Case] = []
    for text in texts:
        try:
            value = json.loads(text)
            cases.append({"text": text, "value": _encode_json_value(value)})
        except json.JSONDecodeError as error:
            cases.append({"text": text, "decode_error": str(error)})
        except ValueError as error:
            cases.append({"text": text, "value_error": str(error)})
    return cases


def utf8_cases(rng: random.Random) -> list[Case]:
    samples = [
        b"ok",
        b"\xff",
        b"a\xe2\x82",
        b"\xe2(",
        b"\xf0\x90(",
        b"\xed\xa0\x80",
        b"\xc0\x80",
        b"\xf4\x90\x80\x80",
        b"\xe0\x80\x80",
        b"\xf0\x80\x80\x80",
        b"\xf0\x9f\x98",
    ]
    pieces = (b"a", b"\xc3\xa9", b"\xe2\x82\xac", b"\xf0\x9f\x98\x80", b"\x80", b"\xff")
    for _ in range(150):
        data = b"".join(rng.choice(pieces) for _ in range(rng.randint(0, 6)))
        cut = rng.randint(0, len(data))
        samples.append(data[:cut] + data[cut + rng.randint(0, 2) :])
    cases: list[Case] = []
    for data in samples:
        try:
            data.decode("utf-8")
            cases.append({"bytes": data.hex(), "error": None})
        except UnicodeDecodeError as error:
            cases.append({"bytes": data.hex(), "error": str(error)})
    return cases


class _Executed(Exception):
    """Raised in place of a managed exec, with the environment that it received."""

    def __init__(self, environment: tuple[str, ...] | None) -> None:
        super().__init__("executed")
        self.environment = environment


def _document(data: bytes) -> Case:
    """Return a document as its text when it is UTF-8, or else as its bytes in hex."""
    try:
        return {"text": data.decode("utf-8")}
    except UnicodeDecodeError:
        return {"bytes": data.hex()}


def environment_file_cases(rng: random.Random) -> list[Case]:
    # A document is its bytes, or a number of spaces.
    documents: list[bytes | int] = [
        b"[]",
        b'["A=1", "B=2"]',
        b'["DUPLICATE=one", "DUPLICATE=two"]',
        1024 * 1024 + 1,
        1024 * 1024,
        b"{}",
        b'"A=1"',
        b"[1]",
        b"[NaN]",
        b'[["A=1"]]',
        b"",
        b"[",
        b'["A=1",]',
        "\ufeff[]".encode(),
        b'["\xff"]',
        b'["A=1", "B=\\ud800"]',
        b'["B=\\udc00\\ud800x"]',
        b"[" + b"1" * 4301 + b"]",
        b'["A=\\u00e9"]\r\n',
    ]
    for _ in range(60):
        text = _mutate(rng, rng.choice(JSON_SEEDS[1:2] + JSON_SEEDS[5:]), JSON_TOKENS)
        documents.append(text.encode("utf-8", "surrogatepass"))
    for _ in range(30):
        entries = [
            f"{rng.choice(('A', 'B', 'É'))}={rng.choice(('1', 'é', '😀', 'x' * 100))}"
            for _ in range(rng.randint(0, 4))
        ]
        documents.append(json.dumps(entries, ensure_ascii=rng.random() < 0.5).encode())
    cases: list[Case] = []
    with tempfile.TemporaryDirectory(prefix="nvx-rs-fixtures-") as directory:
        path = Path(directory) / "environment.json"
        for document in documents:
            if isinstance(document, int):
                path.write_bytes(b" " * document)
                case: Case = {"spaces": document}
            else:
                path.write_bytes(document)
                case = _document(document)
            arguments = nvx.parse_args(
                [
                    "sandbox",
                    "exec",
                    "--state-dir",
                    "absent-state",
                    "--environment-file",
                    str(path),
                ]
            )

            def execute(*_: object, **keywords: object) -> object:
                raise _Executed(cast(tuple[str, ...] | None, keywords["environment"]))

            try:
                with patch.object(sandbox_lifecycle, "exec_workload", execute):
                    nvx.command_sandbox(arguments)
                raise SystemExit("the managed exec did not run")
            except _Executed as executed:
                case["environment"] = (
                    None if executed.environment is None else list(executed.environment)
                )
            except (common.ScriptError, ValueError) as error:
                case["error"] = str(error).replace(str(path), "{path}")
            cases.append(case)
    return cases


def distro_manifest_cases(rng: random.Random) -> list[Case]:
    manifests: list[bytes] = []
    with tempfile.TemporaryDirectory(prefix="nvx-rs-fixtures-") as directory:
        root = Path(directory)
        layer_path = root / "distro.erofs"
        layer_path.write_text(DISTRO_LAYER, encoding="utf-8")
        digest = common.sha256_file(layer_path)
        template = (
            '{{"format": {format}, "artifact": {artifact}, '
            '"artifact_sha256": {sha256}, "packages": {packages}}}'
        )
        for format_value in ("1", "1.0", "true", '"1"', "2", "NaN"):
            for artifact in ('"distro.erofs"', '"other.erofs"', "1"):
                for sha256 in (f'"{digest}"', '"0"'):
                    for packages in (
                        '[{"name": "systemd"}]',
                        '[{"name": "bash"}]',
                        "[]",
                        "{}",
                        "[1]",
                        '[{"name": 1}]',
                        '[{"name": "systemd"}, 1]',
                    ):
                        manifests.append(
                            template.format(
                                format=format_value,
                                artifact=artifact,
                                sha256=sha256,
                                packages=packages,
                            ).encode()
                        )
        manifests.extend(
            (
                b"[]",
                b'{\r\n"format": }',
                b"{\xff}",
                b"\xef\xbb\xbf{}",
                b'{"format": 1, "artifact": "distro.erofs", "format": 2}',
                b"1" * 4301,
            )
        )
        valid = template.format(
            format="1",
            artifact='"distro.erofs"',
            sha256=f'"{digest}"',
            packages='[{"name": "bash"}]',
        )
        for _ in range(60):
            manifests.append(
                _mutate(rng, valid, JSON_TOKENS).encode("utf-8", "surrogatepass")
            )
        launch = sandbox.SandboxLaunch(
            layers=(
                sandbox.SandboxLayer(
                    role="distro",
                    path=layer_path,
                    uuid="11111111-1111-1111-1111-111111111111",
                ),
            ),
            scratch=root / "scratch.ext4",
            entrypoint="/sbin/init",
        )
        manifest_path = root / "distro.erofs.manifest.json"
        cases: list[Case] = []
        for manifest in manifests:
            manifest_path.write_bytes(manifest)
            try:
                nvx._validate_sandbox_systemd_policy(launch)
                error = None
            except (common.ScriptError, ValueError) as raised:
                error = str(raised)
            cases.append({"manifest": _document(manifest), "error": error})
    return cases


def python_fixture() -> dict[str, object]:
    """Return the cases of the Python semantics that the port reproduces."""
    rng = random.Random(SEED)
    return {
        **_header(),
        "unicode": unicodedata.unidata_version,
        "space": space_ranges(),
        "decimal_starts": decimal_starts(),
        "non_printable": non_printable_ranges(),
        "strings": string_cases(rng),
        "paths": path_cases(rng),
        "joins": join_cases(rng),
        "equality": equality_cases(rng),
        "normpath": normpath_cases(rng),
        "json": json_loads_cases(rng),
        "utf8": utf8_cases(rng),
    }


def sandbox_fixture() -> dict[str, object]:
    """Return the cases of the launch profile of scripts/nvx_tools/sandbox.py."""
    rng = random.Random(SEED + 1)
    return {
        **_header(),
        "cwds": CWDS,
        "identities": identity_cases(rng),
        "layers": layer_cases(rng),
        "mount_values": mount_value_cases(rng),
        "mount_targets": target_cases(rng),
        "child_names": child_name_cases(rng),
        "mounts": mount_cases(rng),
        "launches": launch_cases(rng),
    }


def sandbox_options_fixture() -> dict[str, object]:
    """Return the cases of the sandbox helpers of scripts/nvx.py."""
    rng = random.Random(SEED + 2)
    return {
        **_header(),
        "cwds": CWDS,
        "run_share_targets": run_target_cases(rng),
        "sandbox_mounts": sandbox_mounts_cases(rng),
        "environments": environment_cases(rng),
        "environment_files": environment_file_cases(rng),
        "distro_layer": DISTRO_LAYER,
        "distro_manifests": distro_manifest_cases(rng),
    }


def _header() -> dict[str, object]:
    return {
        "generator": "scripts/nvx_rs_fixtures.py",
        "python": f"{sys.version_info.major}.{sys.version_info.minor}",
    }


FIXTURES: dict[str, Callable[[], dict[str, object]]] = {
    "python.json": python_fixture,
    "sandbox.json": sandbox_fixture,
    "sandbox_options.json": sandbox_options_fixture,
}


def fixture_text(document: dict[str, object]) -> str:
    """Return the fixture with one case per line, in ASCII."""
    lines = ["{"]
    items = list(document.items())
    for position, (key, value) in enumerate(items):
        comma = "," if position < len(items) - 1 else ""
        if isinstance(value, list) and value and isinstance(value[0], dict):
            cases = cast(list[object], value)
            lines.append(f"{json.dumps(key)}: [")
            for index, case in enumerate(cases):
                separator = "," if index < len(cases) - 1 else ""
                lines.append(json.dumps(case, separators=(",", ":")) + separator)
            lines.append(f"]{comma}")
        else:
            lines.append(f"{json.dumps(key)}: {json.dumps(value)}{comma}")
    lines.append("}")
    return "\n".join(lines) + "\n"


# The egress policy compiler.


def printable(text: str) -> str:
    """Return `text` as Python prints it to UTF-8, with surrogates as `\\udXXX`."""
    return text.encode("utf-8", "backslashreplace").decode("utf-8")


def segments(data: bytes) -> list[Entry]:
    """Describe `data` as text, or else as hexadecimal, with long runs repeated."""
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return [
            {"hex": run.hex(), **({"repeat": count} if count > 1 else {})}
            for run, count in runs(data)
        ]
    return [
        {"text": run, **({"repeat": count} if count > 1 else {})}
        for run, count in runs(text)
    ]


def runs(items: AnyStr) -> list[tuple[AnyStr, int]]:
    """Split a string or bytes into literal stretches and long runs of one item."""
    result: list[tuple[AnyStr, int]] = []
    start = 0
    index = 0
    while index < len(items):
        end = index
        while end < len(items) and items[end] == items[index]:
            end += 1
        if end - index >= MIN_REPEAT:
            if start < index:
                result.append((items[start:index], 1))
            result.append((items[index : index + 1], end - index))
            start = end
        index = end
    if start < len(items) or not result:
        result.append((items[start:], 1))
    return result


def outcome(path: Path) -> Entry:
    """Compile the policy file at `path` and describe the result."""
    try:
        compiled = compile_policy_file(path)
    except ScriptError as error:
        message = str(error).replace(str(path), PATH_PLACEHOLDER)
        return {"error": printable(message)}
    return {"allow": list(compiled.allow), "deny": list(compiled.deny)}


def test_vectors() -> list[tuple[str, bytes]]:
    """Return every policy file that `scripts/test_egress_policy.py` compiles."""
    captured: list[tuple[str, bytes]] = []
    original = test_egress_policy.compile_policy_file

    def capture(path: Path) -> CompiledEgressPolicy:
        test = next(
            frame.function
            for frame in inspect.stack()
            if frame.function.startswith("test_")
        )
        captured.append((test, path.read_bytes()))
        return original(path)

    test_egress_policy.compile_policy_file = capture
    try:
        suite = unittest.defaultTestLoader.loadTestsFromModule(test_egress_policy)
        result = unittest.TextTestRunner(stream=io.StringIO()).run(suite)
    finally:
        test_egress_policy.compile_policy_file = original
    if not result.wasSuccessful():
        raise SystemExit("scripts/test_egress_policy.py failed")
    seen: set[bytes] = set()
    counts: dict[str, int] = {}
    vectors: list[tuple[str, bytes]] = []
    for test, data in captured:
        if data not in seen and test not in LARGE_VECTORS:
            seen.add(data)
            counts[test] = counts.get(test, 0) + 1
            vectors.append((f"{test} #{counts[test]}", data))
    return vectors


def policy(value: object) -> bytes:
    return json.dumps(value).encode()


def rules(category: str, items: Iterable[object]) -> bytes:
    return policy({category: list(items)})


def json_cases() -> Iterator[tuple[str, bytes]]:
    """Yield cases of the JSON reader: syntax, numbers, hooks, and strings."""
    cidr = "192.0.2.0/24"
    port = f'{{"allow": [{{"cidr": "{cidr}", "protocol": "tcp", "port": %s}}]}}'
    for name, text in (
        ("empty file", ""),
        ("whitespace only", " \t\r\n"),
        ("null", "null"),
        ("array root", "[]"),
        ("number root", "1"),
        ("string root", '"allow"'),
        ("empty object", "{}"),
        ("surrounding whitespace", ' \t\n\r{"allow": []}\r\n '),
        ("form feed after", '{"allow": []}\f'),
        ("vertical tab before", '\v{"allow": []}'),
        ("no-break space after", '{"allow": []}\u00a0'),
        ("trailing comma in object", '{"allow": [],}'),
        ("trailing comma in array", f'{{"allow": [{{"cidr": "{cidr}"}},]}}'),
        ("comment", '{"allow": []} // comment'),
        ("single quotes", "{'allow': []}"),
        ("unquoted name", "{allow: []}"),
        ("missing colon", '{"allow" []}'),
        ("missing comma", '{"allow": [] "deny": []}'),
        ("unclosed object", '{"allow": []'),
        ("unclosed string", '{"allow'),
        ("extra value", '{"allow": []} {}'),
        ("two roots", "{}{}"),
        ("constant prefix", '{"allow": nullx}'),
        ("raw tab in string", f'{{"allow": [{{"cidr": "{cidr}\t"}}]}}'),
        ("escaped tab in string", f'{{"allow": [{{"cidr": "{cidr}\\t"}}]}}'),
        ("raw newline in name", '{"al\nlow": []}'),
        ("invalid escape", '{"allow": [{"cidr": "\\x41"}]}'),
        ("short unicode escape", '{"allow": [{"cidr": "\\u41"}]}'),
        ("solidus escape", '{"allow": [{"cidr": "192.0.2.0\\/24"}]}'),
        ("escaped names", '{"\\u0061llow": [{"\\u0063idr": "192.0.2.7"}]}'),
        ("escaped duplicate", '{"allow": [], "\\u0061llow": []}'),
        ("deep arrays in a rule list", '{"allow": ' + "[" * 100 + "]" * 100 + "}"),
        ("deep arrays", "[" * 500 + "]" * 500),
        ("deep objects", '{"allow": [' + '{"a": ' * 200 + "1" + "}" * 200 + "]}"),
    ):
        yield f"json: {name}", text.encode()
    for value in (
        "01",
        "1.",
        ".5",
        "+80",
        "0x50",
        "8e",
        "8e+",
        "nan",
        "-NaN",
        "Inf",
        "NaN",
        "Infinity",
        "-Infinity",
        "1e400",
        "-1e400",
        "80.0",
        "8e1",
        "1E2",
        "1e-400",
        "-0",
        "0",
        "true",
        "false",
        "null",
        '"80"',
        "[80]",
        "{}",
        "65535",
        "65536",
        "-1",
        "9" * 64,
        "-" + "9" * 64,
        "9" * 65,
        "-" + "9" * 65,
        "1" + "0" * 4300,
    ):
        yield f"json: port {value[:16]}", (port % value).encode()
        yield (
            f"json: endPort {value[:16]}",
            f'{{"deny": [{{"ports": [{{"port": 1, "endPort": {value}}}]}}]}}'.encode(),
        )
    long_digits = "9" * 65
    for name, text in (
        ("duplicate root field", '{"deny": [{"cidr": "0.0.0.0/0"}], "deny": []}'),
        ("duplicate rule field", '{"allow": [{"cidr": "10.0.0.0/8", "cidr": "::/0"}]}'),
        ("duplicate port field", '{"allow": [{"ports": [{"port": 1, "port": 2}]}]}'),
        ("duplicate peer field", '{"allow": [{"to": [{"except": [], "except": []}]}]}'),
        ("first duplicate in order", '{"b": 1, "a": 2, "a": 3, "b": 4}'),
        ("duplicate before long integer", f'{{"a": 1, "a": 2, "b": {long_digits}}}'),
        ("inner duplicate first", f'{{"a": {{"x": 1, "x": 2}}, "b": {long_digits}}}'),
        ("long integer first", f'{{"b": {long_digits}, "a": {{"x": 1, "x": 2}}}}'),
        ("duplicate before syntax error", '{"a": 1, "a": 2,}'),
        ("duplicate before extra data", '{"a": 1, "a": 2} x'),
        ("long integer before syntax error", f"[{long_digits}x]"),
        ("long integer in unknown field", f'{{"unknown": [{long_digits}]}}'),
        ("duplicate in unknown field", '{"unknown": {"x": 1, "x": 2}}'),
        ("duplicate surrogate names", '{"\\ud800": 1, "\\ud800": 2}'),
        ("distinct surrogate names", '{"\\ud800": 1, "\\udc00": 2}'),
        ("surrogate root name", '{"\\ud800": []}'),
        ("surrogate rule name", '{"allow": [{"\\udfff": 1}]}'),
        ("surrogate cidr", '{"allow": [{"cidr": "\\ud800"}]}'),
        ("surrogate scope", '{"allow": [{"cidr": "fe80::1%\\ud800/64"}]}'),
        (
            "surrogate protocol",
            f'{{"allow": [{{"cidr": "{cidr}", "protocol": "\\udc00"}}]}}',
        ),
        ("paired surrogates", '{"allow": [{"cidr": "fe80::1%\\ud83d\\ude00/64"}]}'),
        ("reversed surrogates", '{"allow": [{"cidr": "fe80::1%\\ude00\\ud83d/64"}]}'),
        ("high surrogate before letter", '{"allow": [{"cidr": "\\ud800A"}]}'),
        ("high surrogate before bad escape", '{"allow": [{"cidr": "\\ud800\\u12g4"}]}'),
        ("surrogate exception", '{"allow": [{"cidr": "::/0", "except": ["\\udbff"]}]}'),
    ):
        yield f"json: {name}", text.encode()
    for name, data in (
        ("byte-order mark", b'\xef\xbb\xbf{"allow": []}'),
        ("invalid byte", b'{"allow": [\xff]}'),
        ("invalid byte in string", b'{"allow": [{"cidr": "192.0.2.0/24\xfe"}]}'),
        ("overlong encoding", b'{"allow": [{"cidr": "\xc0\xaf"}]}'),
        ("encoded surrogate", b'{"allow": [{"cidr": "\xed\xa0\x80"}]}'),
        ("truncated sequence", b'{"\xe2\x82": []}'),
        ("utf-16", '{"allow": []}'.encode("utf-16")),
        ("non-ascii text", '{"allow": [{"cidr": "192.0.2.0/24\u00e9"}]}'.encode()),
    ):
        yield f"utf-8: {name}", data
    # Unknown fields are named in code-point order.
    for name, fields in (
        ("ascii", ("b", "a", "B")),
        ("non-ascii", ("\u00e9", "z", "\u00ff")),
        ("astral and surrogate", ("\\ud83d\\ude00", "\\ud800", "\\uffff")),
    ):
        members = ", ".join(f'"{field}": 1' for field in fields)
        yield f"unknown fields: {name}", ("{" + members + "}").encode()
        yield (
            f"unknown rule fields: {name}",
            ('{"allow": [{"cidr": "192.0.2.0/24", ' + members + "}]}").encode(),
        )


def format_items(cidr: str) -> Iterator[tuple[str, list[object]]]:
    """Yield rule lists that reach each message of the format, in check order."""
    tcp: Rule = {"cidr": cidr, "protocol": "tcp"}
    items: tuple[tuple[str, object], ...] = (
        ("rule string", cidr),
        ("rule number", 7),
        ("rule null", None),
        ("rule array", [{"cidr": cidr}]),
        ("empty rule", Rule()),
        ("mixed forms", {"to": [{"cidr": cidr}], "cidr": cidr}),
        ("mixed ports", {"ports": list[Rule](), "port": 80}),
        ("legacy unknown field", {"cidr": cidr, "unknown": 1}),
        ("legacy unknown before missing cidr", {"port": 80, "unknown": 1}),
        ("legacy missing cidr", {"protocol": "tcp", "port": 80}),
        ("legacy except only", {"except": list[str]()}),
        ("legacy cidr number", {"cidr": 7}),
        ("legacy cidr null", {"cidr": None}),
        ("legacy cidr invalid", {"cidr": "192.0.2.0/33"}),
        ("legacy cidr scoped", {"cidr": "fe80::1%eth0/64"}),
        ("legacy except string", {"cidr": cidr, "except": "192.0.2.1"}),
        ("legacy except null", {"cidr": cidr, "except": None}),
        ("legacy except number", {"cidr": cidr, "except": [7]}),
        ("legacy except outside", {"cidr": cidr, "except": ["198.51.100.0/24"]}),
        ("legacy except family", {"cidr": cidr, "except": ["2001:db8::/32"]}),
        ("legacy except wider", {"cidr": cidr, "except": ["192.0.0.0/16"]}),
        ("legacy except whole", {"cidr": cidr, "except": [cidr]}),
        ("legacy except invalid", {"cidr": cidr, "except": ["192.0.2.0/24", "x"]}),
        ("legacy endPort without port", {"cidr": cidr, "endPort": 80}),
        ("legacy endPort with protocol", {**tcp, "endPort": 80}),
        ("legacy port without protocol", {"cidr": cidr, "port": 80}),
        ("legacy protocol upper", {"cidr": cidr, "protocol": "TCP"}),
        ("legacy protocol unknown", {"cidr": cidr, "protocol": "sctp"}),
        ("legacy protocol empty", {"cidr": cidr, "protocol": ""}),
        ("legacy protocol null", {"cidr": cidr, "protocol": None}),
        ("legacy protocol number", {"cidr": cidr, "protocol": 6}),
        ("legacy icmp port", {"cidr": cidr, "protocol": "icmp", "port": 8}),
        ("legacy icmp endPort", {"cidr": cidr, "protocol": "icmp", "endPort": 8}),
        ("legacy port zero", {**tcp, "port": 0}),
        ("legacy port bool", {**tcp, "port": True}),
        ("legacy port string", {**tcp, "port": "80"}),
        ("legacy endPort zero", {**tcp, "port": 1, "endPort": 0}),
        ("legacy endPort bool", {**tcp, "port": 1, "endPort": False}),
        ("legacy endPort below", {**tcp, "port": 10, "endPort": 5}),
        (
            "legacy any range",
            {"cidr": cidr, "protocol": "any", "port": 1, "endPort": 9},
        ),
        ("legacy any", {"cidr": cidr, "protocol": "any"}),
        ("legacy icmp", {"cidr": cidr, "protocol": "icmp"}),
        ("mxc unknown field", {"to": [{"cidr": cidr}], "unknown": 1}),
        ("mxc unknown only", {"unknown": 1}),
        ("mxc to object", {"to": {"cidr": cidr}}),
        ("mxc to null", {"to": None}),
        ("mxc to empty", {"to": list[Rule]()}),
        ("mxc peer string", {"to": [cidr]}),
        ("mxc peer unknown", {"to": [{"cidr": cidr, "unknown": 1}]}),
        ("mxc peer empty", {"to": [Rule()]}),
        ("mxc peer except only", {"to": [{"except": list[str]()}]}),
        ("mxc peer cidr number", {"to": [{"cidr": 7}]}),
        ("mxc peer second invalid", {"to": [{"cidr": cidr}, {"cidr": "x"}]}),
        ("mxc peer except null", {"to": [{"cidr": cidr, "except": None}]}),
        ("mxc peer except outside", {"to": [{"cidr": cidr, "except": ["10.0.0.0/8"]}]}),
        ("mxc ports object", {"ports": {"port": 80}}),
        ("mxc ports empty", {"ports": list[Rule]()}),
        ("mxc port string", {"ports": ["tcp"]}),
        ("mxc port unknown", {"ports": [{"protocol": "tcp", "unknown": 1}]}),
        ("mxc port empty", {"ports": [Rule()]}),
        ("mxc port protocol upper", {"ports": [{"protocol": "Udp"}]}),
        ("mxc port protocol null", {"ports": [{"protocol": None}]}),
        ("mxc port endPort only", {"ports": [{"endPort": 80}]}),
        ("mxc port icmp endPort", {"ports": [{"protocol": "icmp", "endPort": 80}]}),
        ("mxc port icmp port", {"ports": [{"protocol": "icmp", "port": 8}]}),
        ("mxc port null", {"ports": [{"port": None}]}),
        ("mxc port float", {"ports": [{"port": 80.0}]}),
        ("mxc port large", {"ports": [{"port": 70000}]}),
        ("mxc endPort large", {"ports": [{"port": 80, "endPort": 70000}]}),
        (
            "mxc endPort below",
            {"ports": [{"protocol": "udp", "port": 9, "endPort": 8}]},
        ),
        ("mxc second port invalid", {"ports": [{"port": 1}, {"port": 0}]}),
        ("mxc to before ports", {"to": [{"cidr": "x"}], "ports": [{"port": 0}]}),
        ("mxc every destination", {"ports": [{"protocol": "udp", "port": 53}]}),
        ("mxc every port", {"to": [{"cidr": cidr}]}),
    )
    for name, item in items:
        yield name, [item]
    yield "second rule invalid", [{"cidr": cidr}, {"cidr": cidr, "protocol": "TCP"}]


CIDR_FORMS = (
    "192.0.2.7",
    "192.0.2.7/32",
    "192.0.2.7/24",
    "10.0.0.5/255.255.255.0",
    "10.0.0.5/0.0.0.255",
    "10.0.0.5/255.255.255.255",
    "10.0.0.5/0.0.0.0",
    "10.0.0.5/0",
    "10.0.0.5/08",
    "10.0.0.5/0008",
    "10.0.0.5/" + "0" * 4299 + "8",
    "10.0.0.5/" + "0" * 4300 + "8",
    "10.0.0.5/255.0.255.0",
    "10.0.0.5/255.255.255.000",
    "10.0.0.5/+8",
    "10.0.0.5/-8",
    "10.0.0.5/8.0",
    "10.0.0.5/0x8",
    "10.0.0.5/",
    "10.0.0.5//8",
    "10.0.0.5/8/8",
    "10.0.0.5/33",
    " 10.0.0.5",
    "10.0.0.5 ",
    "010.0.0.5",
    "10.0.0.05",
    "256.0.0.0",
    "10.0.0",
    "10.0.0.0.5",
    "10.0.0.\u0665",
    "",
    "/8",
    "::",
    "::/0",
    "::/000128",
    "::/129",
    "2001:DB8::/32",
    "2001:0db8:0000:0000::/32",
    "2001:db8::7/64",
    "2001:db8:0:0:1:0:0:1",
    "1:0:0:2:0:0:0:3",
    "fe80::1",
    "fe80::1%eth0",
    "fe80::1%eth0/64",
    "fe80::%1/10",
    "fe80::1%",
    "fe80::1%a%b",
    "fe80::1%eth0/129",
    "::ffff:192.0.2.1",
    "::ffff:192.0.2.1/120",
    "::ffff:c000:201/120",
    "::FFFF:192.0.2.1/96",
    "::ffff:0:0/95",
    "::192.0.2.1",
    "::c000:200/120",
    "64:ff9b::192.0.2.1/120",
    "1:2:3:4:5:6:1.2.3.4",
    "1:2:3:4:5::1.2.3.4",
    "1:2:3:4:5:6:7:1.2.3.4",
    "1:2:3:4:5:6::1.2.3.4",
    "::1.2.3",
    "::ffff:1.2.3.04",
    "1:2:3:4:5:6:7:8",
    "1:2:3:4:5:6:7::",
    "::2:3:4:5:6:7:8",
    "1:2:3:4:5:6:7:8:9",
    "::1:2:3:4:5:6:7:8",
    "1::2::3",
    ":1::2",
    "1::2:",
    ":::",
    "00000::",
    "0000::",
    "[::1]",
    "::1/0x8",
    "2001:db8::g",
    "2001:db8::\uff11",
)

EXCEPTION_FORMS = (
    "10.0.0.0/8",
    "10.1.2.3/16",
    "10.255.255.255",
    "10.0.0.0/255.255.0.0",
    "10.0.0.0/7",
    "11.0.0.0/8",
    "0.0.0.0/0",
    "::/96",
    "::ffff:10.0.0.0/104",
    "fe80::1%eth0",
    "10.0.0.0/+16",
)


def format_cases() -> Iterator[tuple[str, bytes]]:
    """Yield cases of the policy format: every message, networks, and limits."""
    for category in ("allow", "deny"):
        for name, items in format_items("192.0.2.0/24"):
            yield f"{category}: {name}", rules(category, items)
    hosts: list[Rule] = [{"cidr": f"10.0.{i}.1"} for i in range(300)]
    policies: tuple[tuple[str, object], ...] = (
        ("allow object", {"allow": {}}),
        ("allow null", {"allow": None}),
        ("deny string", {"deny": "192.0.2.0/24"}),
        ("deny number", {"deny": 1}),
        ("unknown root field", {"allow": [], "denied": []}),
        ("allow before deny errors", {"allow": [{"cidr": 7}], "deny": [{"cidr": 8}]}),
        ("later rule error before lowering", {"allow": [*hosts, {"cidr": 7}]}),
        ("allow limit before deny errors", {"allow": hosts[:257], "deny": [7]}),
    )
    for name, value in policies:
        yield f"policy: {name}", policy(value)
    for value in CIDR_FORMS:
        yield f"cidr: {value[:40]}", rules("allow", [{"cidr": value}])
    for value in EXCEPTION_FORMS:
        peer = {"cidr": "10.0.0.0/8", "except": [value]}
        yield f"exception: {value}", rules("deny", [{"to": [peer]}])
    limit = MAX_RULES_PER_ACTION
    many: list[Rule] = [
        {"cidr": f"10.{i // 256}.{i % 256}.1"} for i in range(limit + 1)
    ]
    yield "limit: address-only at the limit", rules("allow", many[:limit])
    yield "limit: address-only over the limit", rules("deny", many)
    for protocol in ("tcp", "udp", "icmp"):
        wide: list[Rule] = [
            {"cidr": "192.0.2.0/24", "protocol": protocol},
            {"cidr": "198.51.100.0/24", "protocol": protocol},
        ]
        yield (
            f"limit: {protocol} for every port over the limit",
            rules("allow", [*many[: limit - 1], *wide]),
        )
    for count in (limit, limit + 1):
        selectors = [{"port": 2 * i + 1, "protocol": "udp"} for i in range(count)]
        yield (
            f"limit: {count} separate ports",
            rules("allow", [{"to": [{"cidr": "192.0.2.1"}], "ports": selectors}]),
        )
    for ipv4, ipv6 in ((128, 128), (129, 128), (128, 129), (200, 56), (0, 257)):
        yield (
            f"limit: {ipv4} IPv4 and {ipv6} IPv6 ports",
            rules(
                "deny",
                (
                    {
                        "to": [{"cidr": peer}],
                        "ports": [{"port": 2 * i + 1} for i in range(n)],
                    }
                    for peer, n in (("192.0.2.1", ipv4), ("2001:db8::1", ipv6))
                    if n
                ),
            ),
        )
    yield (
        "limit: ranges of any protocol over the limit",
        rules(
            "allow",
            (
                {"cidr": "192.0.2.1", "protocol": "any", "port": p, "endPort": p + 1}
                for p in range(1, 3 * 129, 3)
            ),
        ),
    )
    yield (
        "limit: fragmented exclusions",
        rules(
            "allow",
            [{"cidr": "0.0.0.0/0", "except": [f"{2 * i}.0.0.1" for i in range(64)]}],
        ),
    )
    yield (
        "limit: fragments covered by a wider rule",
        rules(
            "allow",
            [
                {"cidr": "0.0.0.0/0", "except": [f"{i}.0.0.1" for i in range(256)]},
                {"cidr": "0.0.0.0/0"},
            ],
        ),
    )
    # The size limit applies to the bytes read.
    valid = b'{"allow": [{"cidr": "192.0.2.0/24"}]}'
    padding = MAX_POLICY_FILE_SIZE - len(valid)
    yield "size: exactly the limit", valid + b" " * padding
    yield "size: one byte over the limit", valid + b" " * (padding + 1)
    yield "size: invalid and over the limit", b"\xff" * (MAX_POLICY_FILE_SIZE + 1)
    yield "size: spaces up to the limit", b" " * MAX_POLICY_FILE_SIZE


class Corpus:
    """A seeded generator of random policies that exercise the lowering."""

    def __init__(self, seed: int) -> None:
        self.random = random.Random(seed)

    def chance(self, probability: float) -> bool:
        return self.random.random() < probability

    def network(self) -> Network:
        pick = self.random.random()
        if pick < 0.45:
            # A dense IPv4 neighborhood, so that rules overlap and merge.
            address = (10 << 24) + self.random.randrange(1024)
            return ipaddress.IPv4Network((address, self.random.randint(22, 32)), False)
        if pick < 0.75:
            address = (0x20010DB8 << 96) + self.random.randrange(1024)
            prefix = self.random.randint(118, 128)
            return ipaddress.IPv6Network((address, prefix), False)
        if pick < 0.88:
            prefix = self.random.choice((0, 1, 2, 4, 8, 12, 16, 24, 31, 32))
            address = self.random.getrandbits(32)
            return ipaddress.IPv4Network((address, prefix), False)
        prefix = self.random.choice((0, 1, 3, 32, 64, 96, 104, 120, 127, 128))
        address = self.random.getrandbits(128)
        if self.chance(0.3):
            # An IPv4-mapped address.
            address = (0xFFFF << 32) | (address & 0xFFFFFFFF)
        return ipaddress.IPv6Network((address, prefix), False)

    def spell(self, network: Network) -> str:
        """Write a network as a policy may: with host bits, masks, or other forms."""
        address = int(network.network_address)
        if network.num_addresses > 1 and self.chance(0.3):
            address += self.random.randrange(network.num_addresses)
        prefix = str(network.prefixlen)
        if self.chance(0.1):
            prefix = "0" * self.random.randint(1, 3) + prefix
        if isinstance(network, ipaddress.IPv4Network):
            text = str(ipaddress.IPv4Address(address))
            if self.chance(0.1):
                prefix = str(network.netmask)
            elif self.chance(0.05):
                prefix = str(network.hostmask)
            if network.prefixlen == 32 and self.chance(0.5):
                return text
            return f"{text}/{prefix}"
        ipv6 = ipaddress.IPv6Address(address)
        pick = self.random.random()
        if pick < 0.15:
            text = ipv6.exploded
        elif pick < 0.25:
            text = str(ipv6).upper()
        elif pick < 0.3:
            embedded = ipaddress.IPv4Address(address & 0xFFFFFFFF)
            text = f"{ipv6.exploded[:30]}{embedded}"
        else:
            text = str(ipv6)
        if network.prefixlen == 128 and self.chance(0.5):
            return text
        return f"{text}/{prefix}"

    def exception(self, parent: Network) -> str:
        if self.chance(0.01):
            return self.random.choice(("x", "10.0.0.0/33", "fe80::1%eth0", "::/0"))
        if self.chance(0.01):
            return self.spell(self.network())
        maximum = parent.max_prefixlen
        prefix = self.random.randint(
            parent.prefixlen, min(maximum, parent.prefixlen + 8)
        )
        size = 1 << (maximum - prefix)
        offset = self.random.randrange(parent.num_addresses // size) * size
        address = int(parent.network_address) + offset
        if isinstance(parent, ipaddress.IPv4Network):
            return self.spell(ipaddress.IPv4Network((address, prefix)))
        return self.spell(ipaddress.IPv6Network((address, prefix)))

    def destination(self, rule: Rule) -> None:
        network = self.network()
        rule["cidr"] = self.spell(network)
        if self.chance(0.3):
            count = self.random.randint(0, 3)
            rule["except"] = [self.exception(network) for _ in range(count)]

    def selector(self, rule: Rule, *, mxc: bool) -> None:
        pick = self.random.random()
        protocol = (
            "tcp"
            if pick < 0.35
            else "udp"
            if pick < 0.6
            else "any"
            if pick < 0.8
            else "icmp"
            if pick < 0.9
            else None
        )
        if protocol is None and not mxc:
            return
        if protocol is not None:
            rule["protocol"] = protocol
        if self.chance(0.3) or (protocol == "icmp" and self.chance(0.98)):
            return
        start = self.random.choice(
            (
                self.random.randint(1, 12),
                self.random.randint(78, 82),
                self.random.randint(440, 445),
                self.random.randint(65530, 65535),
            )
        )
        rule["port"] = start
        if self.chance(0.5):
            if self.chance(0.1):
                rule["endPort"] = self.random.randint(start, 65535)
            else:
                rule["endPort"] = min(65535, start + self.random.randint(0, 6))

    def rule(self) -> Rule:
        rule: Rule = {}
        if self.chance(0.35):
            self.destination(rule)
            self.selector(rule, mxc=False)
            return rule
        if self.chance(0.85):
            peers: list[Rule] = []
            for _ in range(self.random.randint(1, 4)):
                peer: Rule = {}
                self.destination(peer)
                peers.append(peer)
            rule["to"] = peers
        if self.chance(0.75):
            selectors: list[Rule] = []
            for _ in range(self.random.randint(1, 4)):
                selector: Rule = {}
                self.selector(selector, mxc=True)
                selectors.append(selector)
            rule["ports"] = selectors
        return rule

    def policy(self) -> Policy:
        value: Policy = {}
        for category in ("allow", "deny"):
            if self.chance(0.85):
                count = self.random.choice((0, 1, 2, 3, 4, 6, 8))
                if self.chance(0.03):
                    count = self.random.randint(20, 60)
                value[category] = [self.rule() for _ in range(count)]
                if self.chance(0.03):
                    value[category].extend(self.crowd())
        return value

    def crowd(self) -> list[Rule]:
        """Return many rules on scattered ports, which approach the rule limit."""
        crowd: list[Rule] = []
        for _ in range(self.random.randint(80, 300)):
            if self.chance(0.7):
                address = (10 << 24) + self.random.randrange(1024)
                cidr = str(
                    ipaddress.IPv4Network(
                        (address, 32 - self.random.randint(0, 2)), False
                    )
                )
            else:
                address = (0x20010DB8 << 96) + self.random.randrange(1024)
                cidr = str(ipaddress.IPv6Network((address, 128), False))
            port = 2 * self.random.randrange(200) + 1
            rule: Rule = {
                "cidr": cidr,
                "protocol": self.random.choice(("tcp", "udp", "any")),
                "port": port,
            }
            if self.chance(0.3):
                rule["endPort"] = port + self.random.randint(0, 4)
            crowd.append(rule)
        return crowd

    def mutate(self, value: Policy) -> object:
        """Break one part of a policy, as an invalid file would."""
        candidates = [rule for rules in value.values() for rule in rules]
        if not candidates:
            return self.random.choice(([value], {**value, "extra": []}, None))
        rule = self.random.choice(candidates)
        broken: tuple[tuple[str, object], ...] = (
            ("unknown", 1),
            ("cidr", "192.0.2.0/24"),
            ("to", []),
            ("ports", []),
            ("port", True),
            ("protocol", "TCP"),
            ("endPort", 0),
            ("port", 99999),
            ("to", "192.0.2.0/24"),
            ("ports", [{"protocol": "icmp", "port": 1}]),
            ("ports", [{"port": 9, "endPort": 8}]),
            ("to", [{"cidr": "192.0.2.0/24", "except": ["10.0.0.0/8"]}]),
        )
        index = self.random.randrange(len(broken) + 1)
        if index == len(broken):
            rule.pop("cidr", None)
        else:
            field, replacement = broken[index]
            rule[field] = replacement
        return value

    def text(self, value: object) -> bytes:
        """Write a policy as JSON with random formatting, and sometimes break it."""
        indent = self.random.choice((None, None, None, 0, 1, "\t"))
        separators = self.random.choice((None, (",", ":"), (", ", ": ")))
        text = json.dumps(
            value,
            indent=indent,
            separators=separators,
            ensure_ascii=self.chance(0.8),
        )
        if self.chance(0.04):
            replacement = self.random.choice(
                ("NaN", "Infinity", "1e400", "80.0", "9" * 65, "9" * 64, "true")
            )
            text = re.sub(
                r'("(?:port|endPort)"\s*:\s*)\d+', rf"\g<1>{replacement}", text, count=1
            )
        if self.chance(0.03):
            text = text.replace("{", '{"allow": [], ', 1)
        data = text.encode()
        if self.chance(0.03):
            data = data[: self.random.randrange(len(data) + 1)]
        return data

    def cases(self, count: int) -> Iterator[tuple[str, bytes]]:
        for index in range(count):
            value = self.policy()
            item = self.mutate(value) if self.chance(0.12) else value
            yield f"random #{index}", self.text(item)


PATHS = (
    "",
    ".",
    "./",
    "a",
    "a/",
    "a//b",
    "a/./b/.",
    "./a",
    "../a",
    "a/../b",
    "/",
    "//",
    "///",
    "////a",
    "//a/b",
    "///a/b",
    "a\\b",
    "c:",
    "c:a",
    "c:/a",
    "c:\\a\\",
    "C:/",
    "/a",
    "\\a",
    "\\\\server",
    "\\\\server\\share",
    "\\\\server\\share\\x",
    "//server/share/x/",
    "\\\\?\\c:\\x",
    "\\\\?\\UNC\\server\\share\\x",
    "\\\\?\\unc\\server\\share",
    "\\\\.\\device\\x",
    "\\\\\\share",
    "\\\\?.\\x\\y",
    "./c:foo",
    "c:foo/./bar",
    "1:x",
    "::x",
    "a:b:c",
    "\\\\server\\share\\",
    "//?/UNC/a",
    "//?/UNC/a/b",
    "//?/UNC/a/b/c",
    "\\\\?\\",
    "\\\\.\\",
    "\\\\?\\x",
    "a/.",
    ".\\.\\",
    "x\\.\\y",
    "\\\\a\\b\\c\\d\\e",
    " a ",
    "a /b",
    "\u00e9:x",
    "dir/\u00e9.json",
    "policy.json",
)


def egress_policy_fixture(random_cases: int) -> dict[str, list[Entry]]:
    """Return the fixture: compiled policies, unreadable files, and paths."""
    if str(ipaddress.IPv6Address("::ffff:192.0.2.1")) != "::ffff:192.0.2.1":
        raise SystemExit(
            "generating the fixture needs Python 3.13 or later, whose ipaddress "
            "module prints IPv4-mapped IPv6 addresses in dotted form"
        )
    named = [
        *((f"vector: {name}", data) for name, data in test_vectors()),
        *json_cases(),
        *format_cases(),
        *Corpus(SEED).cases(random_cases),
    ]
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        path = directory / "policy.json"
        cases: list[Entry] = []
        for name, data in named:
            path.write_bytes(data)
            cases.append(
                {"name": name, "input": segments(data), "expected": outcome(path)}
            )
        files: list[Entry] = [
            {"name": "missing", "expected": outcome(directory / "missing.json")},
            {"name": "directory", "expected": outcome(directory)},
        ]
    paths: list[Entry] = [
        {
            "path": value,
            "posix": str(PurePosixPath(value)),
            "windows": str(PureWindowsPath(value)),
        }
        for value in PATHS
    ]
    return {"cases": cases, "files": files, "paths": paths}


def egress_policy_text(fixture: dict[str, list[Entry]]) -> str:
    """Write the fixture as ASCII JSON with one entry per line."""
    lines = [
        "{",
        '  "comment": "Generated by scripts/nvx_rs_fixtures.py; do not edit.",',
    ]
    sections = list(fixture.items())
    for position, (name, entries) in enumerate(sections):
        lines.append(f'  "{name}": [')
        for index, entry in enumerate(entries):
            comma = "," if index + 1 < len(entries) else ""
            lines.append(f"    {json.dumps(entry, separators=(',', ':'))}{comma}")
        lines.append("  ]," if position + 1 < len(sections) else "  ]")
    lines.append("}")
    return "\n".join(lines) + "\n"


# The nvx-rs CLI.

CLI_FIXTURE = FIXTURE_DIR / "cli.json"
CLI_PLATFORM = "windows" if os.name == "nt" else "posix"
CLI_PLATFORMS = ("posix", "windows")
NVX_RS_DIR = REPO_ROOT / "nvx-rs"
EXE = ".exe" if os.name == "nt" else ""
# The top-level usage of nvx-rs, which replaces that of nvx.py in the usage
# errors that the top-level parser reports, such as unrecognized arguments.
NVX_RS_USAGE = "usage: nvx-rs [-h] [--root PATH] {artifacts,egress,sandbox} ..."
CLI_STEP_TIMEOUT = 120
LAYER_UUIDS = {
    "distro": "11111111-1111-1111-1111-111111111111",
    "runtime": "22222222-2222-2222-2222-222222222222",
    "custom": "33333333-3333-3333-3333-333333333333",
}
# The tree of every scenario, below its root: files with their text, and
# directories with None. The OpenVMM executable of the root is a copy of
# nvx-fake-openvmm.
CLI_TREE: tuple[tuple[str, str | None], ...] = (
    ("build/vmlinux", "kernel\n"),
    ("build/initramfs.cpio.gz", "initramfs\n"),
    ("distro.erofs", "distro\n"),
    ("runtime.erofs", "runtime\n"),
    ("custom.erofs", "custom\n"),
    ("scratch.ext4", "scratch\n"),
    ("share/file.txt", "file\n"),
    ("share/sub/data.txt", "data\n"),
    ("share/hidden/shown/data.txt", "data\n"),
    ("share/writable/data.txt", "data\n"),
    ("share2/data.txt", "data\n"),
    ("single.txt", "single\n"),
    ("reports", None),
)
CLI_OPENVMM = "openvmm/target/release/openvmm{exe}"
# The files whose permissions the CLI sets, rather than the umask or OpenVMM.
CLI_MODE_NAMES = frozenset(
    (
        "state",
        sandbox_lifecycle.CONFIG_NAME,
        sandbox_lifecycle.RUNTIME_NAME,
        sandbox_lifecycle.CAPABILITY_NAME,
        sandbox_lifecycle.LOCK_NAME,
        "exec.json",
    )
)
VOLATILE = (
    (re.compile(r'("(?:pid|start_time)": )\d+'), r"\1{int}"),
    (re.compile(r"openvmm-microvm-[0-9a-f]{32}"), "openvmm-microvm-{hex32}"),
    (re.compile(r'("(?:operation_id|instance_id)": ")[0-9a-f]{32}"'), r'\1{hex32}"'),
)
# Runs `nvx.py sandbox` with the artifacts of NVX_FIXTURE_ROOT and the OpenVMM
# executable NVX_FIXTURE_OPENVMM.
PYTHON_SANDBOX = """\
import os
import sys
from pathlib import Path

sys.path.insert(0, os.environ["NVX_FIXTURE_SCRIPTS"])
import nvx
from nvx_tools import sandbox_lifecycle

root = Path(os.environ["NVX_FIXTURE_ROOT"])
openvmm = Path(os.environ["NVX_FIXTURE_OPENVMM"])
for module in (nvx, sandbox_lifecycle):
    module.artifact_path = lambda name: root / "build" / name
    module.openvmm_binary_path = lambda: openvmm
sys.argv[0] = "nvx.py"
raise SystemExit(nvx.main(sys.argv[1:]))
"""

Step = dict[str, object]
Scenario = dict[str, object]


def _r(path: str = "") -> str:
    """Return the native path of `path` below the scenario root."""
    return "{root}" + (os.sep + path.replace("/", os.sep) if path else "")


def _layer(role: str = "distro", path: str | None = None) -> list[str]:
    image = _r(path if path is not None else f"{role}.erofs")
    return ["--layer", f"{role},{image},{LAYER_UUIDS[role]}"]


BASE = [*_layer(), "--scratch", _r("scratch.ext4")]
STATE = ["--state-dir", _r("state")]
# What a lifecycle step shows of the state directory.
STATE_FILES = (
    "state",
    "state/config.json",
    "state/runtime.json",
    "state/control.capability",
    "state/outcome.json",
    "state/openvmm.log",
)
START_FILES = (*STATE_FILES, "build/fake-openvmm-state.json")


def _step(
    *argv: str,
    write: dict[str, object] | None = None,
    remove: Sequence[str] = (),
    check: Sequence[str] = (),
    env: dict[str, str] | None = None,
) -> Step:
    """Return a step that runs `nvx-rs ARGV` after writing and removing files.

    A written file takes text, or a dict with `hex` bytes, or with `text`, if
    any, then `fill` repeated to `size` bytes. After the step, the files of
    `check` are recorded."""
    step: Step = {"argv": list(argv)}
    if write:
        step["write"] = write
    if remove:
        step["remove"] = list(remove)
    if check:
        step["check"] = list(check)
    if env:
        step["env"] = env
    return step


def _sandbox(
    *argv: str,
    write: dict[str, object] | None = None,
    remove: Sequence[str] = (),
    check: Sequence[str] = (),
) -> Step:
    return _step("sandbox", *argv, write=write, remove=remove, check=check)


def _scenario(
    name: str,
    *steps: Step,
    platforms: Sequence[str] = CLI_PLATFORMS,
    interop: bool = False,
) -> Scenario | None:
    """Return a scenario of `steps` in a fresh root, if it runs on this platform.

    An interoperability scenario also runs with nvx.py and nvx-rs taking turns,
    as the checks of this script do, so that a state directory that either
    leaves is valid for the other."""
    if CLI_PLATFORM not in platforms:
        return None
    scenario: Scenario = {"name": name, "steps": list(steps)}
    if interop:
        scenario["interop"] = True
    return scenario


def _distro_manifest(packages: list[str] | None, digest: str | None = None) -> str:
    """Return a manifest of the distro layer of the tree, `distro.erofs`."""
    value: dict[str, object] = {
        "format": 1,
        "artifact": "distro.erofs",
        "artifact_sha256": digest or hashlib.sha256(b"distro\n").hexdigest(),
    }
    if packages is not None:
        value["packages"] = [{"name": name, "version": "1"} for name in packages]
    return json.dumps(value)


def _validation_scenarios() -> list[Scenario | None]:
    """Return the option checks of `command_sandbox`, each in its own root."""
    environment = {"env.json": '["A=1"]'}
    cases: list[tuple[str, list[str], dict[str, object] | None]] = [
        ("dry run outside run", ["provision", "--dry-run"], None),
        (
            "environment and environment file",
            ["exec", "--environment", "A=1", "--environment-file", _r("env.json")],
            environment,
        ),
        ("cwd outside exec", ["run", "--cwd", "/"], None),
        ("environment outside exec", ["start", "--environment", "A=1"], None),
        (
            "environment file outside exec",
            ["provision", "--environment-file", _r("env.json")],
            environment,
        ),
        (
            "inherited environment outside exec",
            ["stop", "--inherit-default-environment"],
            None,
        ),
        (
            "environment file too large",
            ["exec", "--environment-file", _r("env.json")],
            {"env.json": {"fill": " ", "size": nvx.MAX_ENVIRONMENT_FILE_BYTES + 1}},
        ),
        (
            "environment file at the limit",
            ["exec", "--environment-file", _r("env.json")],
            {
                "env.json": {
                    "text": '["A=1"]',
                    "fill": " ",
                    "size": nvx.MAX_ENVIRONMENT_FILE_BYTES,
                }
            },
        ),
        (
            "environment file missing",
            ["exec", "--environment-file", _r("missing.json")],
            None,
        ),
        (
            "environment file directory",
            ["exec", "--environment-file", _r("share")],
            None,
        ),
        (
            "environment file not UTF-8",
            ["exec", "--environment-file", _r("env.json")],
            {"env.json": {"hex": "5b22ff225d"}},
        ),
        (
            "environment file not JSON",
            ["exec", "--environment-file", _r("env.json")],
            {"env.json": '["A=1"'},
        ),
        (
            "environment file object",
            ["exec", "--environment-file", _r("env.json")],
            {"env.json": '{"A": "1"}'},
        ),
        (
            "environment file non-string",
            ["exec", "--environment-file", _r("env.json")],
            {"env.json": '["A=1", 2]'},
        ),
        (
            "environment file entry without equals",
            ["exec", "--environment-file", _r("env.json")],
            {"env.json": '["A"]'},
        ),
        (
            "environment file entry with NUL",
            ["exec", "--environment-file", _r("env.json")],
            {"env.json": '["A=\\u0000"]'},
        ),
        (
            "environment file duplicate entry",
            ["exec", "--environment-file", _r("env.json")],
            {"env.json": '["A=1", "A=2"]'},
        ),
        ("environment entry without name", ["exec", "--environment", "=x"], None),
        ("environment entry with space", ["exec", "--environment", "A B=x"], None),
        ("network outside run", ["start", "--net", "10.0.0.2/24"], None),
        (
            "egress rule outside run",
            ["exec", "--network-egress-allow", "10.0.0.0/8"],
            None,
        ),
        ("forward outside run", ["stop", "--host-loopback-forward", "80"], None),
        (
            "policy file outside run",
            ["deprovision", "--network-egress-policy-file", _r("policy.json")],
            None,
        ),
        (
            "systemd entrypoint of run",
            ["run", *BASE, "--entrypoint", "/usr/lib/systemd/systemd"],
            None,
        ),
        (
            "systemd entrypoint of exec",
            ["exec", "--entrypoint", "/lib/systemd/systemd"],
            None,
        ),
        (
            "outcome report outside run",
            ["provision", "--outcome-report", _r("reports/exec.json")],
            None,
        ),
        ("deny without mount", ["run", "--mount-deny", _r("share/hidden")], None),
        ("allow without mount", ["run", "--mount-allow", _r("share/hidden")], None),
        ("write without mount", ["run", "--mount-write", _r("share/writable")], None),
        ("owner without mount", ["run", "--mount-owner", "vmm"], None),
        ("mount outside run", ["start", "--mount", "/data," + _r("share")], None),
        ("net without profile", ["run", *BASE, "--net", "10.0.0.2/24"], None),
        ("profile without net", ["run", *BASE, "--network-profile", "portable"], None),
        ("run without layers", ["run"], None),
        ("provision without scratch", ["provision", *_layer()], None),
        ("run without layer", ["run", "--scratch", _r("scratch.ext4")], None),
        (
            "policy file with rules",
            [
                "run",
                *BASE,
                "--network-egress",
                "allow",
                "--network-egress-policy-file",
                _r("policy.json"),
                "--network-egress-deny",
                "10.0.0.0/8",
            ],
            None,
        ),
        (
            "policy file without egress",
            ["run", *BASE, "--network-egress-policy-file", _r("policy.json")],
            None,
        ),
        (
            "policy file missing",
            [
                "run",
                *BASE,
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                _r("missing.json"),
            ],
            None,
        ),
        (
            "policy file invalid",
            [
                "run",
                *BASE,
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                _r("policy.json"),
            ],
            {"policy.json": '{"allow": [{"cidr": "10.0.0.0/8", "port": 0}]}'},
        ),
        ("provision without state directory", ["provision", *BASE], None),
        ("start without state directory", ["start"], None),
        ("exec without state directory", ["exec"], None),
        ("stop without state directory", ["stop"], None),
        ("deprovision without state directory", ["deprovision"], None),
        ("run with state directory", ["run", *BASE, *STATE], None),
        (
            "invalid layer UUID",
            [
                "run",
                "--layer",
                f"distro,{_r('distro.erofs')},nope",
                "--scratch",
                _r("scratch.ext4"),
            ],
            None,
        ),
        (
            "invalid layer role",
            ["run", "--layer", f"base,{_r('distro.erofs')},{LAYER_UUIDS['distro']}"],
            None,
        ),
        (
            "missing layer",
            [
                "run",
                *_layer("distro", "missing.erofs"),
                "--scratch",
                _r("scratch.ext4"),
            ],
            None,
        ),
        (
            "duplicate layer",
            ["run", *_layer(), *_layer(), "--scratch", _r("scratch.ext4")],
            None,
        ),
        ("missing scratch", ["run", *_layer(), "--scratch", _r("missing.ext4")], None),
        ("relative entrypoint", ["run", *BASE, "--entrypoint", "bin/sh"], None),
        ("invalid hostname", ["run", *BASE, "--hostname", "bad host"], None),
        ("argument with space", ["run", *BASE, "--arg", "a b"], None),
        ("root workload", ["run", *BASE, "--workload-user", "0:0"], None),
        ("invalid workload user", ["run", *BASE, "--workload-user", "x"], None),
        ("invalid memory limit", ["run", *BASE, "--memory-max", "x"], None),
        ("invalid process limit", ["run", *BASE, "--pids-max", "0"], None),
        (
            "relative mount target",
            ["run", *BASE, "--mount", "data," + _r("share")],
            None,
        ),
        (
            "missing mount host",
            ["run", *BASE, "--mount", "/data," + _r("missing")],
            None,
        ),
        (
            "invalid mount mode",
            ["run", *BASE, "--mount", "/data," + _r("share") + ",rx"],
            None,
        ),
        (
            "deny outside share",
            [
                "run",
                *BASE,
                "--mount",
                "/data," + _r("share"),
                "--mount-deny",
                _r("share2"),
            ],
            None,
        ),
        (
            "allow outside denied path",
            [
                "run",
                *BASE,
                "--mount",
                "/data," + _r("share"),
                "--mount-allow",
                _r("share/hidden/shown"),
            ],
            None,
        ),
        (
            "write in read-only share",
            [
                "run",
                *BASE,
                "--mount",
                "/data," + _r("share") + ",ro",
                "--mount-write",
                _r("share/writable"),
            ],
            None,
        ),
        (
            "reserved command-line token",
            ["run", *BASE, "--cmdline", "nvx_sandbox=0"],
            None,
        ),
        (
            "systemd distro image",
            ["run", *BASE],
            {"distro.erofs.manifest.json": _distro_manifest(["busybox", "systemd"])},
        ),
        (
            "mismatched distro manifest",
            ["run", *BASE],
            {"distro.erofs.manifest.json": _distro_manifest([], "0" * 64)},
        ),
        (
            "invalid distro manifest",
            ["run", *BASE],
            {"distro.erofs.manifest.json": "{"},
        ),
        (
            "distro manifest without packages",
            ["run", *BASE],
            {"distro.erofs.manifest.json": _distro_manifest(None)},
        ),
    ]
    scenarios: list[Scenario | None] = []
    for name, argv, write in cases:
        scenarios.append(_scenario(f"validation: {name}", _sandbox(*argv, write=write)))
    scenarios.append(
        _scenario(
            "validation: caller-owned share on Windows",
            _sandbox(
                "run",
                *BASE,
                "--mount",
                "/data," + _r("share"),
                "--mount-owner",
                "caller",
            ),
            platforms=("windows",),
        )
    )
    return scenarios


def _usage_scenarios() -> list[Scenario | None]:
    """Return the usage errors of the sandbox parser, and its help."""
    cases: list[tuple[str, list[str]]] = [
        ("help", ["-h"]),
        ("help after operation", ["exec", "--help"]),
        ("timeout not a number", ["--timeout", "x"]),
        ("zero timeout", ["--timeout", "0"]),
        ("negative timeout", ["--timeout", "-1"]),
        ("infinite timeout", ["--timeout", "inf"]),
        ("timeout not finite", ["--timeout=1e400"]),
        ("timeout not a number either", ["--timeout", "nan"]),
        ("exec timeout not an integer", ["--exec-timeout-ms", "1.5"]),
        ("memory not an integer", ["--memory-mib", "x"]),
        ("invalid hypervisor", ["--hypervisor", "hyperv"]),
        ("invalid operation", ["restart"]),
        ("second operation", ["run", "start"]),
        ("invalid mount owner", ["--mount-owner", "root"]),
        ("invalid network profile", ["--network-profile", "open"]),
        ("invalid egress default", ["--network-egress", "block"]),
        ("invalid host loopback", ["--host-loopback", "yes"]),
        ("missing value", ["--layer"]),
        ("option as value", ["--layer", "--scratch"]),
        ("dash value", ["--arg", "-x"]),
        ("flag with value", ["--dry-run=1"]),
        ("unrecognized option", ["--bogus"]),
        ("unrecognized short option", ["-x"]),
        ("unrecognized after separator", ["run", "--", "extra"]),
    ]
    return [_scenario(f"usage: {name}", _sandbox(*argv)) for name, argv in cases]


def _dry_run_scenarios() -> list[Scenario | None]:
    """Return one-shot commands, which --dry-run prints without running."""
    policy: dict[str, object] = {
        "policy.json": json.dumps(
            {
                "allow": [
                    {"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]},
                    {"cidr": "2001:db8::/32", "protocol": "tcp", "port": 443},
                ],
                "deny": [{"cidr": "198.51.100.0/24", "protocol": "udp"}],
            }
        )
    }
    cases: list[tuple[str, list[str], dict[str, object] | None]] = [
        ("minimal", [*BASE], None),
        (
            "every layer and option",
            [
                *_layer("custom"),
                *_layer("runtime"),
                *_layer("distro"),
                "--scratch",
                _r("scratch.ext4"),
                "--entrypoint",
                "/bin/server",
                "--arg=--port",
                "--arg",
                "8080",
                "--hostname",
                "worker",
                "--workload-user",
                "1000:1000",
                "--memory-max",
                "268435456",
                "--pids-max",
                "64",
                "--memory-mib",
                "512",
                "--hypervisor",
                "mshv",
                "--cmdline",
                "quiet loglevel=3",
                "--outcome-report",
                _r("reports/openvmm-run.json"),
            ],
            None,
        ),
        ("explicit hypervisor", [*BASE, "--hypervisor", "kvm"], None),
        ("one share", [*BASE, "--mount", "/data," + _r("share")], None),
        ("writable share", [*BASE, "--mount", "/data," + _r("share") + ",rw"], None),
        (
            "file share",
            [*BASE, "--mount", "/etc/single.txt," + _r("single.txt") + ",ro"],
            None,
        ),
        (
            "several shares with policies",
            [
                *BASE,
                "--mount",
                "/data," + _r("share") + ",rw",
                "--mount-deny",
                _r("share/hidden"),
                "--mount-allow",
                _r("share/hidden/shown"),
                "--mount-write",
                _r("share/writable"),
                "--mount",
                "/other," + _r("share2"),
            ],
            None,
        ),
        (
            "network",
            [
                *BASE,
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-ingress",
                "allow",
                "--network-egress-allow",
                "10.0.0.0/8:tcp:443",
                "--network-egress-deny",
                "10.1.0.0/16",
                "--host-loopback",
                "allow",
                "--network-proxy",
                "10.0.0.1:3128",
                "--host-loopback-forward",
                "8080",
            ],
            None,
        ),
        (
            "egress policy file",
            [
                *BASE,
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                _r("policy.json"),
            ],
            policy,
        ),
        ("quoted arguments", [*BASE, "--cmdline", 'a="b c" d'], None),
    ]
    scenarios = [
        _scenario(f"dry run: {name}", _sandbox("run", *argv, "--dry-run", write=write))
        for name, argv, write in cases
    ]
    scenarios.append(
        _scenario(
            "dry run: caller-owned share",
            _sandbox(
                "run",
                *BASE,
                "--mount",
                "/data," + _r("share"),
                "--mount-owner",
                "caller",
                "--dry-run",
            ),
            platforms=("posix",),
        )
    )
    return scenarios


def _run_scenarios() -> list[Scenario | None]:
    """Return one-shot runs of the fake OpenVMM."""
    echo = ["--entrypoint", "/bin/echo", "--arg", "hello", "--arg", "world"]
    return [
        _scenario("run: echo", _sandbox("run", *BASE, *echo)),
        _scenario("run: default operation", _sandbox(*BASE, *echo)),
        _scenario("run: failure", _sandbox("run", *BASE, "--entrypoint", "/bin/false")),
        _scenario(
            "run: missing program",
            _sandbox("run", *BASE, "--entrypoint", "/bin/missing"),
        ),
        _scenario(
            "run: exit status",
            _sandbox(
                "run",
                *BASE,
                "--entrypoint",
                "/bin/true",
                "--cmdline",
                "fake_exit_status=7",
            ),
        ),
        _scenario(
            "run: outcome report",
            _sandbox(
                "run",
                *BASE,
                *echo,
                "--outcome-report",
                _r("reports/openvmm-run.json"),
                check=("reports/openvmm-run.json", "build/fake-openvmm-reports.json"),
            ),
        ),
        _scenario(
            "run: network outcome report",
            _sandbox(
                "run",
                *BASE,
                "--entrypoint",
                "/bin/true",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "10.0.0.0/8:tcp:443",
                "--outcome-report",
                _r("reports/openvmm-run.json"),
                check=("reports/openvmm-run.json",),
            ),
        ),
        _scenario(
            "run: OpenVMM refuses its options",
            _sandbox("run", *BASE, "--host-loopback-forward", "8080"),
        ),
        _scenario(
            "run: missing OpenVMM",
            _sandbox("run", *BASE, remove=(CLI_OPENVMM,)),
        ),
        _scenario(
            "run: missing kernel",
            _sandbox("run", *BASE, remove=("build/vmlinux",)),
        ),
        _scenario(
            "run: missing initramfs",
            _sandbox("run", *BASE, remove=("build/initramfs.cpio.gz",)),
        ),
    ]


def _exec(
    *argv: str,
    write: dict[str, object] | None = None,
    check: Sequence[str] = (),
) -> Step:
    return _sandbox("exec", *STATE, *argv, write=write, check=check)


def _lifecycle(
    name: str,
    provision: Sequence[str],
    *execs: Step,
    write: dict[str, object] | None = None,
    platforms: Sequence[str] = CLI_PLATFORMS,
) -> Scenario | None:
    """Return a sandbox's lifecycle: provision, start, `execs`, stop, start, stop,
    and deprovision."""
    return _scenario(
        f"lifecycle: {name}",
        _sandbox(
            "provision", *STATE, *BASE, *provision, write=write, check=STATE_FILES
        ),
        _sandbox("start", *STATE, check=START_FILES),
        *execs,
        _sandbox("stop", *STATE, check=STATE_FILES),
        _sandbox("start", *STATE, check=STATE_FILES),
        _sandbox("stop", *STATE, check=STATE_FILES),
        _sandbox("deprovision", *STATE, check=("state",)),
        platforms=platforms,
        interop=True,
    )


def _lifecycle_scenarios() -> list[Scenario | None]:
    """Return managed sandboxes from provisioning to deprovisioning."""
    report = _r("reports/exec.json")
    return [
        _lifecycle(
            "minimal",
            (),
            _exec("--entrypoint", "/bin/echo", "--arg", "hello", "--arg", "world"),
            _exec("--entrypoint", "/bin/pwd", "--cwd", "/tmp"),
            _exec(
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg",
                "echo out; echoerr err; exit 3",
                "--outcome-report",
                report,
                check=("reports/exec.json",),
            ),
            _exec(
                "--entrypoint",
                "/usr/bin/env",
                "--environment",
                "A=1",
                "--environment",
                "B=two",
            ),
            _exec(
                "--entrypoint",
                "/usr/bin/printenv",
                "--arg",
                "PATH",
                "--inherit-default-environment",
                "--environment",
                "A=1",
            ),
            _exec(
                "--entrypoint",
                "/usr/bin/env",
                "--environment-file",
                _r("env.json"),
                write={"env.json": '["X=1", "Y=two words"]'},
            ),
            _exec(
                "--entrypoint",
                "/usr/bin/env",
                "--environment-file",
                _r("empty.json"),
                write={"empty.json": "[]"},
            ),
            _exec(
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg",
                "sleep 5000",
                "--exec-timeout-ms",
                "50",
                "--outcome-report",
                _r("reports/exec-timeout.json"),
                check=("reports/exec-timeout.json",),
            ),
            _exec("--entrypoint", "/bin/sh", "--arg=-c", "--arg", "signal 9"),
            _exec("--entrypoint", "/bin/missing"),
            _exec("--entrypoint", "/bin/pwd", "--cwd", "/missing"),
            _exec("--entrypoint", "/bin/pwd", "--cwd", "relative"),
            _exec(
                "--entrypoint",
                "/bin/echo",
                "--outcome-report",
                report,
            ),
            _exec(
                "--entrypoint", "/bin/echo", "--outcome-report", _r("missing/exec.json")
            ),
        ),
        _lifecycle(
            "one share",
            ("--mount", "/data," + _r("share")),
            _exec("--entrypoint", "/bin/pwd", "--cwd", "/data"),
        ),
        _lifecycle(
            "caller-owned share",
            ("--mount", "/data," + _r("share") + ",rw", "--mount-owner", "caller"),
            platforms=("posix",),
        ),
        _lifecycle(
            "several shares",
            (
                "--mount",
                "/data," + _r("share"),
                "--mount",
                "/other," + _r("share2") + ",rw",
            ),
            _exec("--entrypoint", "/bin/pwd", "--cwd", "/other"),
        ),
        _lifecycle(
            "share policies",
            (
                "--mount",
                "/data," + _r("share") + ",rw",
                "--mount-deny",
                _r("share/hidden"),
                "--mount-allow",
                _r("share/hidden/shown"),
                "--mount-write",
                _r("share/writable"),
            ),
        ),
        _lifecycle(
            "file share",
            ("--mount", "/etc/single.txt," + _r("single.txt") + ",ro"),
        ),
        _lifecycle(
            "network and launch options",
            (
                *_layer("runtime"),
                "--entrypoint",
                "/bin/server",
                "--arg=--serve",
                "--hostname",
                "worker",
                "--workload-user",
                "1000:1000",
                "--memory-max",
                "268435456",
                "--pids-max",
                "64",
                "--memory-mib",
                "384",
                "--hypervisor",
                "auto",
                "--cmdline",
                "quiet",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-ingress",
                "deny",
                "--network-egress-policy-file",
                _r("policy.json"),
                "--host-loopback",
                "allow",
                "--network-proxy",
                "10.0.0.1:3128",
                "--host-loopback-forward",
                "8080",
                "--host-loopback-forward",
                "9090",
            ),
            write={"policy.json": LIFECYCLE_POLICY},
        ),
    ]


LIFECYCLE_POLICY = json.dumps(
    {
        "allow": [{"cidr": "192.0.2.0/24", "protocol": "tcp", "port": 443}],
        "deny": [{"cidr": "192.0.2.128/25"}],
    }
)


def _state_scenarios() -> list[Scenario | None]:
    """Return the refusals and failures of the lifecycle operations."""
    provision = _sandbox("provision", *STATE, *BASE)
    return [
        _scenario(
            "state: operations without a sandbox",
            _sandbox("start", *STATE),
            _sandbox("exec", *STATE),
            _sandbox("stop", *STATE),
            _sandbox("deprovision", *STATE),
        ),
        _scenario(
            "state: refusals of a running sandbox",
            provision,
            _sandbox("provision", *STATE, *BASE),
            _sandbox("start", *STATE),
            _sandbox("start", *STATE),
            _sandbox("deprovision", *STATE),
            _sandbox("stop", *STATE),
            _sandbox("stop", *STATE),
            _sandbox("exec", *STATE),
            _sandbox("deprovision", *STATE, check=("state",)),
            interop=True,
        ),
        _scenario(
            "state: relative and nested state directory",
            _sandbox(
                "provision",
                "--state-dir",
                "nested/state",
                *BASE,
                check=("nested/state", "nested/state/config.json"),
            ),
            _sandbox(
                "deprovision", "--state-dir", "nested/state", check=("nested/state",)
            ),
        ),
        _scenario(
            "state: failed start",
            _sandbox("provision", *STATE, *BASE, "--cmdline", "fake_exit_on_start=5"),
            # Like nvx.py, nvx-rs waits until --timeout for the control endpoint
            # of an OpenVMM that exits at once.
            _sandbox("start", *STATE, "--timeout", "0.5", check=STATE_FILES),
            _sandbox("start", *STATE, "--timeout", "0.5", check=STATE_FILES),
            _sandbox("deprovision", *STATE, check=("state",)),
            interop=True,
        ),
        _scenario(
            "state: start without OpenVMM",
            provision,
            _sandbox("start", *STATE, remove=(CLI_OPENVMM,), check=STATE_FILES),
            _sandbox("deprovision", *STATE),
        ),
        _scenario(
            "state: malformed configuration",
            provision,
            _sandbox("start", *STATE, write={"state/config.json": '{"format": 7}'}),
            _sandbox("start", *STATE, write={"state/config.json": "[]"}),
            _sandbox("start", *STATE, write={"state/config.json": '{"format": 1}'}),
            _sandbox("deprovision", *STATE, check=("state",)),
        ),
        _scenario(
            "state: not a directory",
            _sandbox("start", "--state-dir", _r("single.txt")),
            _sandbox("provision", "--state-dir", _r("single.txt"), *BASE),
            _sandbox("deprovision", "--state-dir", _r("single.txt")),
        ),
        _scenario(
            "state: foreign file",
            provision,
            _sandbox(
                "deprovision",
                *STATE,
                write={"state/foreign.txt": "x"},
                check=("state",),
            ),
        ),
    ]


def _artifact_expectation(root: str, openvmm: str | None) -> str:
    """Return what `nvx-rs artifacts` prints for the checkout at `root`, from the
    paths of scripts/nvx_tools/common.py and guests.py."""

    def path(original: Path) -> str:
        return str(Path(root) / original.relative_to(REPO_ROOT))

    constants = {
        "alpine": build_constants.AlpineBuildConstants,
        "ubuntu": build_constants.UbuntuBuildConstants,
        "azurelinux": build_constants.AzureLinuxBuildConstants,
    }
    entries: list[dict[str, object]] = []
    for name, descriptor in guests.GUEST_DESCRIPTORS.items():
        distro = cast(str | None, getattr(constants[name], "DISTRO_NAME", None))
        manifest = cast(
            str | None, getattr(constants[name], "DISTRO_MANIFEST_NAME", None)
        )
        entries.append(
            {
                "name": name,
                "distribution": descriptor.distribution,
                "initramfs": path(common.artifact_path(descriptor.initramfs_name)),
                "package_manifest": path(
                    common.artifact_path(descriptor.package_manifest_name)
                ),
                "distro_layer": None
                if distro is None
                else path(common.artifact_path(distro)),
                "distro_manifest": None
                if manifest is None
                else path(common.artifact_path(manifest)),
            }
        )
    value = {
        "layout": "repository",
        "root": root,
        "openvmm": openvmm
        if openvmm is not None
        else path(common.openvmm_binary_path()),
        "kernel": path(
            common.artifact_path(build_constants.KernelBuildConstants.BINARY_NAME)
        ),
        "guests": entries,
    }
    return json.dumps(value, indent=2, sort_keys=True)


def _command_scenarios() -> list[Scenario | None]:
    """Return the other commands: artifacts and egress compile."""
    policies: list[tuple[str, str]] = [
        ("empty", "{}"),
        ("allow", '{"allow": [{"cidr": "192.0.2.0/24"}]}'),
        (
            "both",
            json.dumps(
                {
                    "allow": [
                        {"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]},
                        {
                            "to": [{"cidr": "2001:db8::/32"}],
                            "ports": [{"protocol": "tcp", "port": 80, "endPort": 81}],
                        },
                    ],
                    "deny": [{"cidr": "198.51.100.0/24", "protocol": "icmp"}],
                }
            ),
        ),
        ("invalid JSON", "{"),
        ("invalid rule", '{"allow": [{"cidr": "192.0.2.0/24", "port": 70000}]}'),
        ("unknown field", '{"allow": [], "other": []}'),
    ]
    scenarios: list[Scenario | None] = [
        _scenario(
            f"egress compile: {name}",
            _step("egress", "compile", _r("policy.json"), write={"policy.json": text}),
        )
        for name, text in policies
    ]
    scenarios += [
        _scenario(
            "egress compile: missing file",
            _step("egress", "compile", _r("missing.json")),
        ),
        _scenario("egress compile: directory", _step("egress", "compile", _r("share"))),
        _scenario("artifacts: checkout", _step("artifacts")),
        _scenario(
            "artifacts: OpenVMM override",
            _step("artifacts", env={"NVX_OPENVMM": _r("alt/openvmm{exe}")}),
        ),
    ]
    return scenarios


def cli_scenarios() -> list[Scenario]:
    """Return the scenarios of this platform."""
    scenarios = [
        *_command_scenarios(),
        *_usage_scenarios(),
        *_validation_scenarios(),
        *_dry_run_scenarios(),
        *_run_scenarios(),
        *_lifecycle_scenarios(),
        *_state_scenarios(),
    ]
    return [scenario for scenario in scenarios if scenario is not None]


def _expand(text: str, root: Path) -> str:
    return text.replace("{root}", str(root)).replace("{exe}", EXE)


def _below(root: Path, relative: str) -> Path:
    return root.joinpath(*_expand(relative, root).split("/"))


def _make_tree(root: Path, fake: Path) -> None:
    for relative, text in CLI_TREE:
        path = _below(root, relative)
        if text is None:
            path.mkdir(parents=True, exist_ok=True)
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(text.encode())
    openvmm = _below(root, CLI_OPENVMM)
    openvmm.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(fake, openvmm)


def _write_file(path: Path, content: object) -> None:
    if isinstance(content, str):
        data = content.encode()
    else:
        spec = cast(dict[str, object], content)
        if "hex" in spec:
            data = bytes.fromhex(cast(str, spec["hex"]))
        else:
            data = cast(str, spec.get("text", "")).encode()
            data += cast(str, spec["fill"]).encode() * (
                cast(int, spec["size"]) - len(data)
            )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def _child_environment() -> dict[str, str]:
    """Return the environment of a recorded command: no terminal size, color,
    or OpenVMM override, and UTF-8 standard streams."""
    variables = ("COLUMNS", "LINES", "FORCE_COLOR", "PYTHON_COLORS", "NVX_OPENVMM")
    environment = {
        name: value for name, value in os.environ.items() if name not in variables
    }
    environment["NO_COLOR"] = "1"
    environment["PYTHONIOENCODING"] = "utf-8"
    return environment


def _lines(text: str) -> str:
    """Return `text` as `print` writes it to a standard stream."""
    return text.replace("\n", os.linesep) + os.linesep


def _nvx_rs_prog(stdout: str, stderr: str) -> tuple[str, str]:
    """Return the output of nvx.py with the program names of nvx-rs.

    The sandbox parser's name, `nvx.py sandbox`, has the length of
    `nvx-rs sandbox`, so its usage wraps the same way. Errors that the
    top-level parser reports show its usage, which nvx-rs replaces with its
    own."""
    stdout = stdout.replace("usage: nvx.py sandbox", "usage: nvx-rs sandbox", 1)
    stderr = stderr.replace("usage: nvx.py sandbox", "usage: nvx-rs sandbox", 1)
    stderr = stderr.replace("nvx.py sandbox: error: ", "nvx-rs sandbox: error: ")
    lines = stderr.splitlines(keepends=True)
    if (
        lines
        and lines[-1].startswith("nvx.py: error: ")
        and lines[0].startswith("usage: nvx.py ")
    ):
        last = lines[-1].replace("nvx.py: error: ", "nvx-rs: error: ", 1)
        stderr = _lines(NVX_RS_USAGE) + last
    return stdout, stderr


def _normalize(text: str, root: Path) -> str:
    """Replace the root, as printed and as JSON writes it, and volatile values,
    such as process IDs and random identifiers, with placeholders."""
    raw = str(root)
    escaped = json.dumps(raw)[1:-1]
    if escaped != raw:
        text = text.replace(escaped, "{root_json}")
    text = text.replace(raw, "{root}")
    for pattern, replacement in VOLATILE:
        text = pattern.sub(replacement, text)
    return text


def _decode(data: bytes) -> str:
    return data.decode("utf-8", "backslashreplace")


def _snapshot(root: Path, relative: str) -> object:
    """Return what a step shows of a file or directory below the root."""
    path = _below(root, relative)
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    entry: Entry = {}
    if stat.S_ISDIR(info.st_mode):
        entry["entries"] = sorted(os.listdir(path))
    elif path.name == sandbox_lifecycle.CAPABILITY_NAME:
        entry["size"] = info.st_size
    else:
        entry["text"] = _normalize(_decode(path.read_bytes()), root)
    if os.name != "nt" and (
        path.name in CLI_MODE_NAMES or path.name.startswith("exec")
    ):
        entry["mode"] = stat.S_IMODE(info.st_mode)
    return entry


def _python_command(argv: list[str], root: Path, step: Step) -> tuple[int, str, str]:
    """Return what nvx.py, or the Python functions that nvx-rs ports for the
    commands that nvx.py lacks, give for `nvx-rs ARGV`."""
    environment = {
        name: _expand(value, root)
        for name, value in cast(dict[str, str], step.get("env", {})).items()
    }
    if argv[0] == "egress":
        try:
            compiled = compile_policy_file(Path(argv[2]))
        except (ScriptError, OSError, ValueError) as error:
            return 1, "", _lines(f"error: {error}")
        value = {"allow": list(compiled.allow), "deny": list(compiled.deny)}
        return 0, _lines(json.dumps(value, indent=2, sort_keys=True)), ""
    if argv[0] == "artifacts":
        text = _artifact_expectation(str(root), environment.get("NVX_OPENVMM"))
        return 0, _lines(text), ""
    child = _child_environment()
    child.update(
        NVX_FIXTURE_SCRIPTS=str(REPO_ROOT / "scripts"),
        NVX_FIXTURE_ROOT=str(root),
        NVX_FIXTURE_OPENVMM=str(_below(root, CLI_OPENVMM)),
    )
    result = subprocess.run(
        [sys.executable, "-u", "-c", PYTHON_SANDBOX, *argv],
        cwd=root,
        env=child,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        timeout=CLI_STEP_TIMEOUT,
        check=False,
    )
    stdout, stderr = _nvx_rs_prog(_decode(result.stdout), _decode(result.stderr))
    return result.returncode, stdout, stderr


def _rust_command(
    nvx_rs: Path, argv: list[str], root: Path, step: Step
) -> tuple[int, str, str]:
    """Return what `nvx-rs --root ROOT ARGV` gives."""
    child = _child_environment()
    child.update(
        {
            name: _expand(value, root)
            for name, value in cast(dict[str, str], step.get("env", {})).items()
        }
    )
    result = subprocess.run(
        [str(nvx_rs), "--root", str(root), *argv],
        cwd=root,
        env=child,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        timeout=CLI_STEP_TIMEOUT,
        check=False,
    )
    return result.returncode, _decode(result.stdout), _decode(result.stderr)


def _end_leftovers(root: Path) -> None:
    """Kill the fake OpenVMM processes that a failed scenario leaves running."""
    for record in root.rglob(sandbox_lifecycle.RUNTIME_NAME):
        with contextlib.suppress(OSError, ValueError, KeyError, TypeError):
            pid = int(json.loads(record.read_text(encoding="utf-8"))["pid"])
            os.kill(pid, signal.SIGTERM if os.name == "nt" else signal.SIGKILL)
            time.sleep(0.2)


def play(
    scenario: Scenario, binaries: tuple[Path, Path], implementations: Sequence[str]
) -> list[Entry]:
    """Play a scenario in a fresh root, with its steps taking turns among
    `implementations`, `python` or `rust`, and return what each step gave."""
    nvx_rs, fake = binaries
    with tempfile.TemporaryDirectory(
        prefix="nvx-cli-", ignore_cleanup_errors=True
    ) as temporary:
        root = Path(temporary).resolve()
        _make_tree(root, fake)
        results: list[Entry] = []
        try:
            for index, step in enumerate(cast(list[Step], scenario["steps"])):
                for relative in cast(list[str], step.get("remove", [])):
                    _below(root, relative).unlink()
                for relative, content in cast(
                    dict[str, object], step.get("write", {})
                ).items():
                    _write_file(_below(root, relative), content)
                argv = [_expand(value, root) for value in cast(list[str], step["argv"])]
                implementation = implementations[index % len(implementations)]
                if implementation == "python":
                    status, stdout, stderr = _python_command(argv, root, step)
                else:
                    status, stdout, stderr = _rust_command(nvx_rs, argv, root, step)
                result: Entry = {
                    "status": status,
                    "stdout": _normalize(stdout, root),
                    "stderr": _normalize(stderr, root),
                }
                checks = cast(list[str], step.get("check", []))
                if checks:
                    result["files"] = {
                        relative: _snapshot(root, relative) for relative in checks
                    }
                results.append(result)
        finally:
            _end_leftovers(root)
        return results


def cli_binaries() -> tuple[Path, Path]:
    """Build nvx-rs and nvx-fake-openvmm, and return their paths."""
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--quiet",
            "--features",
            "cli,testing",
            "--bin",
            "nvx-rs",
            "--bin",
            "nvx-fake-openvmm",
        ],
        cwd=NVX_RS_DIR,
        env=dict(os.environ, NVX_BUNDLE="skip"),
        check=True,
    )
    target = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    directory = (target if target.is_absolute() else NVX_RS_DIR / target) / "debug"
    return directory / f"nvx-rs{EXE}", directory / f"nvx-fake-openvmm{EXE}"


def cli_section(binaries: tuple[Path, Path]) -> list[Entry]:
    """Record the scenarios of this platform with nvx.py."""
    if sys.version_info < (3, 14, 5):
        raise SystemExit(
            "recording nvx-rs/tests/fixtures/cli.json needs CPython 3.14.5 or later, "
            "whose argparse quotes the choices of an invalid choice, as nvx-rs does"
        )
    section: list[Entry] = []
    for scenario in cli_scenarios():
        results = play(scenario, binaries, ("python",))
        steps = [
            {**step, **result}
            for step, result in zip(
                cast(list[Step], scenario["steps"]), results, strict=True
            )
        ]
        section.append({**scenario, "steps": steps})
    return section


def cli_fixture(section: list[Entry]) -> dict[str, object]:
    """Return the fixture with this platform's section and the other's."""
    sections: dict[str, object] = {}
    if CLI_FIXTURE.is_file():
        current = cast(dict[str, object], json.loads(CLI_FIXTURE.read_text("utf-8")))
        sections = {name: current.get(name, []) for name in CLI_PLATFORMS}
    sections[CLI_PLATFORM] = section
    tree: list[Entry] = [
        {"path": relative, "text": text} if text is not None else {"path": relative}
        for relative, text in CLI_TREE
    ]
    tree.append({"path": CLI_OPENVMM, "openvmm": True})
    return {
        **_header(),
        "tree": tree,
        **{name: sections.get(name, []) for name in CLI_PLATFORMS},
    }


def _differences(
    name: str, mode: str, expected: list[Entry], actual: list[Entry]
) -> list[str]:
    problems: list[str] = []
    for index, (want, got) in enumerate(zip(expected, actual, strict=True)):
        for field in ("status", "stdout", "stderr", "files"):
            if want.get(field) != got.get(field):
                diff = "\n".join(
                    difflib.unified_diff(
                        json.dumps(want.get(field), indent=1).splitlines(),
                        json.dumps(got.get(field), indent=1).splitlines(),
                        "nvx.py",
                        mode,
                        lineterm="",
                    )
                )
                problems.append(f"{name}, step {index + 1}, {field}:\n{diff}")
    return problems


def check_cli_parity(section: list[Entry], binaries: tuple[Path, Path]) -> list[str]:
    """Play every scenario with nvx-rs, and interoperability scenarios with
    nvx.py and nvx-rs taking turns both ways, and return how any differs from
    nvx.py."""
    problems: list[str] = []
    for scenario in section:
        name = cast(str, scenario["name"])
        expected = cast(list[Entry], scenario["steps"])
        modes = [("rust",)]
        if scenario.get("interop"):
            modes += [("python", "rust"), ("rust", "python")]
        for implementations in modes:
            actual = play(scenario, binaries, implementations)
            problems += _differences(name, "-".join(implementations), expected, actual)
    return problems


def _write(path: Path, contents: str, check: bool) -> bool:
    """Write contents to path, or with check, report whether it would change."""
    current = path.read_text(encoding="utf-8") if path.is_file() else None
    if current == contents:
        return False
    if check:
        print(f"{path} is out of date; regenerate it", file=sys.stderr)
        return True
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents, encoding="utf-8", newline="\n")
    print(f"wrote {path}")
    return False


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Generate the golden fixtures and Python-parity tables of nvx-rs."
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail instead of writing when a generated file would change",
    )
    parser.add_argument(
        "--random-cases",
        type=int,
        help="with --output, the number of random egress policies "
        f"(default: {RANDOM_CASES})",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="write or check only an egress policy fixture at this path",
    )
    args = parser.parse_args(argv)
    if args.random_cases is not None and args.output is None:
        parser.error("--random-cases needs --output")
    if (sys.implementation.name, sys.version_info[:2]) != ("cpython", PYTHON):
        version = f"{sys.version_info.major}.{sys.version_info.minor}"
        raise SystemExit(
            f"nvx_rs_fixtures.py records CPython {PYTHON[0]}.{PYTHON[1]}, "
            f"not {sys.implementation.name} {version}"
        )
    if args.output is not None:
        random_cases = RANDOM_CASES if args.random_cases is None else args.random_cases
        fixture = egress_policy_text(egress_policy_fixture(random_cases))
        return 1 if _write(args.output, fixture, args.check) else 0
    stale = _write(UNICODE_TABLES, unicode_tables_source(), args.check)
    for name, document in FIXTURES.items():
        stale |= _write(FIXTURE_DIR / name, fixture_text(document()), args.check)
    stale |= _write(
        EGRESS_POLICY_FIXTURE,
        egress_policy_text(egress_policy_fixture(RANDOM_CASES)),
        args.check,
    )
    binaries = cli_binaries()
    section = cli_section(binaries)
    stale |= _write(CLI_FIXTURE, fixture_text(cli_fixture(section)), args.check)
    problems = check_cli_parity(section, binaries)
    for problem in problems:
        print(f"nvx-rs differs from nvx.py: {problem}", file=sys.stderr)
    return 1 if stale or problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
