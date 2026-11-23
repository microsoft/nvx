"""Find, build, and run nvx, the Rust command-line interface that runs
NVX's microVMs and sandboxes.

The microVM tests run their microVMs and sandboxes with the command lines
that command_line() gives, and compile their egress policies with nvx
(nvx/README.md). They run, in this order:

1. the executable that the NVX environment variable names;
2. nvx/target/release/nvx[.exe] in this checkout, where `cargo build
   --release` puts it; or else
3. that executable, after nvx.py builds it with Cargo, whose output goes to
   standard error so that it never mixes with the output of the command.

Without Cargo, nvx.py fails and names these choices.

The provenance of an nvx, build/nvx.provenance.json, binds the digest
of its executable to source_sha256() of the sources that built it, and to the
Rust release, target, and features of the build, as OpenVMM's provenance
binds it to its source revision. nvx.py records it for the nvx that it
builds, and `nvx.py record-nvx-provenance` for one built otherwise.
Packaging and the adversarial executor reject an nvx that it does not
describe as built from this checkout's sources. Releases ship nvx as
bin/nvx[.exe], built for RELEASE_TARGETS with FEATURES, with its
provenance. `nvx download` installs the other artifacts of a release into
a checkout, and leaves its nvx alone.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from .build_constants import BuildConstants, OpenVMMBuildConstants
from .common import (
    ScriptError,
    artifact_path,
    normalized_sha256_file,
    require_file,
    sha256_file,
)
from .elf import read_elf_executable
from .guest_binaries import pinned_guest_rust_toolchain

# The environment variable that names the nvx executable to run.
VARIABLE = "NVX"
DIRECTORY = BuildConstants.REPO_ROOT / "nvx"
# The artifact root that nvx runs with: this checkout, whose build/ and
# openvmm/target/release/ hold the artifacts that nvx.py runs.
ROOT = BuildConstants.REPO_ROOT
BINARY_NAME = "nvx"
# The command that builds the executable that binary_path() names, from the
# repository root, so that rust-toolchain.toml selects the Rust release. The
# target directory is explicit, so that CARGO_TARGET_DIR cannot move it.
BUILD_COMMAND = (
    "cargo",
    "build",
    "--release",
    "--locked",
    "--features",
    "cli",
    "--manifest-path",
    "nvx/Cargo.toml",
    "--target-dir",
    "nvx/target",
)
# The features of the nvx that BUILD_COMMAND builds and that releases ship.
FEATURES = ("cli",)
# The Rust targets of the nvx that releases ship, by operating system, as
# CI's nvx build jobs build it: a static musl executable, which runs on every
# x86-64 Linux host, and an MSVC executable that links the C runtime
# statically, which runs without the Visual C++ redistributable.
RELEASE_TARGETS = {
    "linux": OpenVMMBuildConstants.MUSL_RUST_TARGET,
    "windows": OpenVMMBuildConstants.WINDOWS_RUST_TARGET,
}
ARM_WINDOWS_RELEASE_TARGET = "aarch64-pc-windows-msvc"
# The domain and format of the inventory that source_sha256() hashes.
SOURCE_DIGEST_DOMAIN = "nvx-cli-sources"
SOURCE_DIGEST_FORMAT = 1
# The provenance of the nvx that nvx.py runs, in build/, beside that of
# OpenVMM, and the format of its record.
PROVENANCE_NAME = "nvx.provenance.json"
PROVENANCE_FORMAT = 1


def binary_path() -> Path:
    """Return the nvx of this checkout, nvx/target/release/nvx[.exe],
    where BUILD_COMMAND puts it."""
    name = f"{BINARY_NAME}.exe" if os.name == "nt" else BINARY_NAME
    return DIRECTORY / "target" / "release" / name


def release_target(*, architecture: str = "x86_64") -> str:
    """Return the Rust target of the nvx that releases for this host's
    operating system ship."""
    if architecture == "aarch64":
        if os.name != "nt":
            raise ScriptError(
                "ARM release packaging currently supports only Windows/WHP"
            )
        return ARM_WINDOWS_RELEASE_TARGET
    if architecture != "x86_64":
        raise ScriptError(f"unsupported release architecture: {architecture}")
    return RELEASE_TARGETS["windows" if os.name == "nt" else "linux"]


def source_files(root: Path | None = None) -> tuple[Path, ...]:
    """Return the files that determine the nvx executable of the checkout at
    `root`, this one by default, in path order: the crate's manifest, lock
    file, and sources, and rust-toolchain.toml, which pins the Rust release
    that builds it."""
    root = DIRECTORY.parent if root is None else root
    crate = root / DIRECTORY.name
    files = [
        crate / "Cargo.toml",
        crate / "Cargo.lock",
        root / "rust-toolchain.toml",
    ]
    sources = crate / "src"
    if not sources.is_dir():
        raise ScriptError(f"nvx source directory not found: {sources}")
    files.extend(path for path in sources.rglob("*") if path.is_file())
    return tuple(
        sorted(
            (require_file(path, "nvx source") for path in files),
            key=lambda path: path.relative_to(root).as_posix(),
        )
    )


def source_sha256(root: Path | None = None) -> str:
    """Hash the paths and contents of source_files(root), with line endings
    normalized, so that every checkout of the same sources has one digest."""
    root = DIRECTORY.parent if root is None else root
    inventory = {
        "domain": SOURCE_DIGEST_DOMAIN,
        "format": SOURCE_DIGEST_FORMAT,
        "files": [
            {
                "path": path.relative_to(root).as_posix(),
                "sha256": normalized_sha256_file(path),
            }
            for path in source_files(root)
        ],
    }
    encoded = json.dumps(inventory, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def provenance_path() -> Path:
    """Return build/nvx.provenance.json, the provenance of the nvx that
    nvx.py runs."""
    return artifact_path(PROVENANCE_NAME)


def host_target() -> str:
    """Return the target that Cargo builds for by default here: the host of
    the Rust release that rust-toolchain.toml selects."""
    try:
        completed = subprocess.run(
            ("rustc", "-vV"),
            cwd=BuildConstants.REPO_ROOT,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:
        raise ScriptError(f"failed to run rustc for its host target: {error}") from (
            error
        )
    if completed.returncode == 0:
        for line in completed.stdout.splitlines():
            if line.startswith("host: "):
                return line.removeprefix("host: ").strip()
    raise ScriptError(f"rustc -vV did not report a host target: {completed.stderr}")


def check_target(executable: Path, target: str) -> None:
    """Fail unless `executable` is what a build for `target` gives, where the
    target makes that checkable: for the Linux release target, a static
    executable, which a build for the host's own target is not."""
    if target == ARM_WINDOWS_RELEASE_TARGET:
        try:
            with executable.open("rb") as stream:
                dos = stream.read(64)
                if len(dos) != 64 or dos[:2] != b"MZ":
                    raise ScriptError(f"invalid ARM64 PE executable: {executable}")
                offset = int.from_bytes(dos[60:64], "little")
                if offset < len(dos):
                    raise ScriptError(
                        f"ARM64 PE header overlaps the DOS header: {executable}"
                    )
                stream.seek(offset)
                pe = stream.read(6)
        except OSError as error:
            raise ScriptError(
                f"cannot inspect ARM64 PE executable {executable}: {error}"
            ) from error
        if pe[:4] != b"PE\0\0" or int.from_bytes(pe[4:6], "little") != 0xAA64:
            raise ScriptError(f"executable is not ARM64 PE: {executable}")
        return
    if target != RELEASE_TARGETS["linux"]:
        return
    try:
        static = read_elf_executable(executable).static
    except (OSError, ValueError) as error:
        raise ScriptError(f"invalid {target} nvx executable: {error}") from error
    if not static:
        raise ScriptError(
            f"nvx executable {executable} is not static, but a {target} build "
            f"is: build it with `cargo build --release --locked --features cli "
            f"--manifest-path nvx/Cargo.toml --target {target}`, and copy "
            f"nvx/target/{target}/release/{BINARY_NAME} to {binary_path()}"
        )


def _format_command(command: Sequence[str]) -> str:
    return subprocess.list2cmdline(command) if os.name == "nt" else shlex.join(command)


def _missing_cargo_message() -> str:
    return (
        f"nvx is not built at {binary_path()}, and Cargo is not on PATH to "
        f"build it: set {VARIABLE} to an nvx executable, or install Rust "
        "(https://rustup.rs) and build it from the repository root with "
        f"`{_format_command(BUILD_COMMAND)}`"
    )


def configured_binary() -> Path | None:
    """Return the executable that NVX names, or None if it is unset."""
    configured = os.environ.get(VARIABLE)
    if not configured:
        return None
    path = Path(configured).absolute()
    if not path.is_file():
        raise ScriptError(f"{VARIABLE} does not name an nvx executable: {path}")
    return path


def find_binary() -> Path | None:
    """Return the nvx that nvx.py runs without building it, or None."""
    configured = configured_binary()
    if configured is not None:
        return configured
    path = binary_path()
    return path if path.is_file() else None


def provenance_record(executable: Path, target: str) -> dict[str, object]:
    """Return the provenance of the nvx executable at `executable`, built
    for `target` with FEATURES from this checkout's sources."""
    return {
        "format": PROVENANCE_FORMAT,
        "source_sha256": source_sha256(),
        "toolchain": pinned_guest_rust_toolchain(DIRECTORY.parent),
        "target": target,
        "features": list(FEATURES),
        "executable_sha256": sha256_file(executable),
    }


def record_provenance(target: str | None = None) -> Path:
    """Record the provenance of the nvx that nvx.py runs without building
    it, built from this checkout for `target`, by default the host's, and
    return where.

    The caller vouches for the build; the record binds its executable to this
    checkout's sources, which is what packaging and the adversarial executor
    check.
    """
    executable = find_binary()
    if executable is None:
        raise ScriptError(
            f"nvx is not built at {binary_path()}: build it with "
            f"`{_format_command(BUILD_COMMAND)}`, or set {VARIABLE} to an "
            "nvx executable"
        )
    target = target or host_target()
    check_target(executable, target)
    path = provenance_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(provenance_record(executable, target), indent=2) + "\n",
        encoding="utf-8",
    )
    return path


def _stderr_target() -> int:
    """Return the descriptor of standard error, to which a build's output goes,
    or DEVNULL if standard error has none, as when a test replaced it."""
    try:
        sys.stderr.flush()
        return sys.stderr.fileno()
    except (AttributeError, OSError, ValueError):
        return subprocess.DEVNULL


def build_binary() -> Path:
    """Build the nvx of this checkout with Cargo and return it."""
    if shutil.which("cargo") is None:
        raise ScriptError(_missing_cargo_message())
    print(f">> {_format_command(BUILD_COMMAND)}", file=sys.stderr, flush=True)
    try:
        completed = subprocess.run(
            BUILD_COMMAND,
            cwd=BuildConstants.REPO_ROOT,
            stdin=subprocess.DEVNULL,
            stdout=_stderr_target(),
            check=False,
        )
    except OSError as error:
        raise ScriptError(f"failed to run Cargo to build nvx: {error}") from error
    if completed.returncode != 0:
        raise ScriptError(
            f"Cargo failed to build nvx (exit {completed.returncode}); fix the "
            f"build, or set {VARIABLE} to an nvx executable"
        )
    path = binary_path()
    if not path.is_file():
        raise ScriptError(f"Cargo did not build {path}")
    # A Cargo build replaces the executable that an earlier record described.
    record_provenance()
    return path


def require_binary() -> Path:
    """Return the nvx that nvx.py runs, and build it first if needed."""
    return find_binary() or build_binary()


def command_line(arguments: Sequence[str], *, root: Path | None = None) -> list[str]:
    """Return the command line `nvx --root ROOT ARGUMENTS`, which runs the
    nvx that nvx.py runs, built first if needed, on the artifacts of `root`,
    this checkout by default, as the microVM scenarios run their sandboxes and
    microVMs."""
    root = ROOT if root is None else root
    return [os.fspath(require_binary()), "--root", os.fspath(root), *arguments]


# nvx reports an error on standard error as `error[CODE]: MESSAGE`, and a
# usage error as its parser's usage and then `PROG: error[CODE]: MESSAGE`,
# where CODE is one of the stable error codes of nvx/README.md. An error is
# one line, and its message may change: callers tell errors apart by their
# codes. The program's name is tried last, and is the shortest that fits, so
# that an error whose message names `error[...]: `, as a file of that name
# does, keeps its own code and the whole message.
_ERROR_LINE = re.compile(
    r"^(?:[^\n]*?: )??error\[(?P<code>[a-z]+(?:-[a-z]+)*)\]: ", re.MULTILINE
)


@dataclass(frozen=True)
class ReportedError:
    """An error that nvx reported: its stable code and its message."""

    code: str
    message: str


def reported_error(stderr: bytes | str) -> ReportedError | None:
    """Return the error that nvx reported on its standard error, `stderr`,
    or None if it reported none.

    nvx reports an error last, on one line of its own, so a workload's
    output before it, such as that of `sandbox exec`, does not count, even
    when the workload left its last line without an end, and neither does a
    line break in a value that the message names, which nvx prints as `\\n`:
    the error is the last line in the form of one, and its message runs from
    there to the end of the output, without its final line end.
    """
    text = (
        stderr.decode("utf-8", errors="replace")
        if isinstance(stderr, bytes)
        else stderr
    ).replace("\r\n", "\n")
    matches = list(_ERROR_LINE.finditer(text))
    if not matches:
        return None
    last = matches[-1]
    return ReportedError(last["code"], text[last.end() :].removesuffix("\n"))


class NvxError(ScriptError):
    """A failure of an nvx command, whose string is the message of the
    error that nvx reported, with its stable code, or None if it reported
    none."""

    def __init__(self, message: str, code: str | None = None) -> None:
        super().__init__(message)
        self.code = code


def _failure(stderr: bytes, returncode: int) -> NvxError:
    reported = reported_error(stderr)
    if reported is not None:
        return NvxError(reported.message, reported.code)
    text = stderr.decode("utf-8", errors="replace").replace("\r\n", "\n")
    return NvxError(text.removesuffix("\n") or f"nvx exited with status {returncode}")


def query(arguments: Sequence[str]) -> object:
    """Run `nvx --root ROOT ARGUMENTS`, and return the JSON that it prints.

    A failure raises NvxError with the code and the message of the error
    that nvx reported, which nvx.py prints after `error: `. Callers tell
    failures apart by the code, since the message may change.
    """
    command = command_line(arguments)
    try:
        completed = subprocess.run(
            command, stdin=subprocess.DEVNULL, capture_output=True, check=False
        )
    except OSError as error:
        raise ScriptError(f"failed to run {command[0]}: {error}") from error
    if completed.returncode != 0:
        raise _failure(completed.stderr, completed.returncode)
    try:
        return cast(object, json.loads(completed.stdout.decode("utf-8")))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ScriptError(
            f"nvx {' '.join(arguments)} printed invalid JSON: {error}"
        ) from error


def _object(value: object, what: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ScriptError(f"nvx printed {what} that is not a JSON object")
    return cast(dict[str, object], value)


def _strings(document: dict[str, object], key: str, what: str) -> tuple[str, ...]:
    value = document.get(key)
    if not isinstance(value, list) or not all(
        isinstance(item, str) for item in cast(list[object], value)
    ):
        raise ScriptError(f"nvx printed {what} without a {key} list of strings")
    return tuple(cast(list[str], value))


@dataclass(frozen=True)
class EgressRules:
    """The native OpenVMM rules that an egress policy file compiles to, in the
    order in which OpenVMM receives each action's rules."""

    allow: tuple[str, ...]
    deny: tuple[str, ...]


def compile_egress_policy(path: Path) -> EgressRules:
    """Compile the egress policy file at `path` with `nvx egress compile`.

    A refused policy raises ScriptError with the compiler's message. The path
    follows `--`, so that nvx reads one that starts with `-` as the path.
    """
    what = "egress rules"
    document = _object(query(("egress", "compile", "--", os.fspath(path))), what)
    return EgressRules(
        allow=_strings(document, "allow", what),
        deny=_strings(document, "deny", what),
    )
