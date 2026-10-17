"""Find, build, and run nvx-rs, the Rust command-line interface that nvx.py
delegates to.

nvx.py runs nvx-rs for `nvx.py sandbox` and for the egress policy and artifact
steps of `nvx.py run` (nvx-rs/README.md). It runs, in this order:

1. the executable that the NVX_RS environment variable names;
2. nvx-rs/target/release/nvx-rs[.exe] in this checkout, where `cargo build
   --release` puts it, and where `nvx.py download` installs the nvx-rs of a
   release built from this checkout's nvx-rs sources; or else
3. that executable, after it builds it with Cargo, whose output goes to
   standard error so that it never mixes with the output of the command.

Without Cargo, nvx.py fails and names these choices.

The provenance of an nvx-rs, build/nvx-rs.provenance.json, binds the digest
of its executable to source_sha256() of the sources that built it, and to the
Rust release, target, and features of the build, as OpenVMM's provenance
binds it to its source revision. nvx.py records it for the nvx-rs that it
builds, and `nvx.py record-nvx-rs-provenance` for one built otherwise.
Packaging and the adversarial executor reject an nvx-rs that it does not
describe as built from this checkout's sources. Releases ship nvx-rs as
bin/nvx-rs[.exe], built for RELEASE_TARGETS with FEATURES, with its
provenance, and `nvx.py download` installs both only when that provenance
names this checkout's sources.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import threading
from collections.abc import Callable, Generator, Sequence
from dataclasses import dataclass
from pathlib import Path
from types import FrameType
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

# The environment variable that names the nvx-rs executable to run.
VARIABLE = "NVX_RS"
DIRECTORY = BuildConstants.REPO_ROOT / "nvx-rs"
# The artifact root that nvx-rs runs with: this checkout, whose build/ and
# openvmm/target/release/ hold the artifacts that nvx.py runs.
ROOT = BuildConstants.REPO_ROOT
BINARY_NAME = "nvx-rs"
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
    "nvx-rs/Cargo.toml",
    "--target-dir",
    "nvx-rs/target",
)
# The features of the nvx-rs that BUILD_COMMAND builds and that releases ship.
FEATURES = ("cli",)
# The Rust targets of the nvx-rs that releases ship, by operating system, as
# CI's NVX CLI jobs build it: a static musl executable, which runs on every
# x86-64 Linux host, and an MSVC executable that links the C runtime
# statically, which runs without the Visual C++ redistributable.
RELEASE_TARGETS = {
    "linux": OpenVMMBuildConstants.MUSL_RUST_TARGET,
    "windows": OpenVMMBuildConstants.WINDOWS_RUST_TARGET,
}
# The domain and format of the inventory that source_sha256() hashes.
SOURCE_DIGEST_DOMAIN = "nvx-rs-cli-sources"
SOURCE_DIGEST_FORMAT = 1
# The provenance of the nvx-rs that nvx.py runs, in build/, beside that of
# OpenVMM, and the format of its record.
PROVENANCE_NAME = "nvx-rs.provenance.json"
PROVENANCE_FORMAT = 1


def binary_path() -> Path:
    """Return the nvx-rs of this checkout, nvx-rs/target/release/nvx-rs[.exe],
    where BUILD_COMMAND puts it.

    This is also where `nvx.py download` installs a release's bin/nvx-rs[.exe],
    as it installs bin/openvmm[.exe] where the OpenVMM build puts it.
    """
    name = f"{BINARY_NAME}.exe" if os.name == "nt" else BINARY_NAME
    return DIRECTORY / "target" / "release" / name


def release_target() -> str:
    """Return the Rust target of the nvx-rs that releases for this host's
    operating system ship."""
    return RELEASE_TARGETS["windows" if os.name == "nt" else "linux"]


def source_files(root: Path | None = None) -> tuple[Path, ...]:
    """Return the files that determine the nvx-rs executable of the checkout at
    `root`, this one by default, in path order: the crate's manifest, lock
    file, build script, and sources, and rust-toolchain.toml, which pins the
    Rust release that builds it."""
    root = DIRECTORY.parent if root is None else root
    crate = root / DIRECTORY.name
    files = [
        crate / "Cargo.toml",
        crate / "Cargo.lock",
        crate / "build.rs",
        root / "rust-toolchain.toml",
    ]
    sources = crate / "src"
    if not sources.is_dir():
        raise ScriptError(f"nvx-rs source directory not found: {sources}")
    files.extend(path for path in sources.rglob("*") if path.is_file())
    return tuple(
        sorted(
            (require_file(path, "nvx-rs source") for path in files),
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
    """Return build/nvx-rs.provenance.json, the provenance of the nvx-rs that
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
    if target != RELEASE_TARGETS["linux"]:
        return
    try:
        static = read_elf_executable(executable).static
    except (OSError, ValueError) as error:
        raise ScriptError(f"invalid {target} nvx-rs executable: {error}") from error
    if not static:
        raise ScriptError(
            f"nvx-rs executable {executable} is not static, but a {target} build "
            f"is: build it with `cargo build --release --locked --features cli "
            f"--manifest-path nvx-rs/Cargo.toml --target {target}`, and copy "
            f"nvx-rs/target/{target}/release/{BINARY_NAME} to {binary_path()}"
        )


def _format_command(command: Sequence[str]) -> str:
    return subprocess.list2cmdline(command) if os.name == "nt" else shlex.join(command)


def _missing_cargo_message() -> str:
    return (
        f"nvx-rs is not built at {binary_path()}, and Cargo is not on PATH to "
        f"build it: set {VARIABLE} to an nvx-rs executable, install the nvx-rs "
        "of a release built from this checkout's nvx-rs sources with "
        "`nvx.py download`, or install Rust "
        "(https://rustup.rs) and build it from the repository root with "
        f"`{_format_command(BUILD_COMMAND)}`"
    )


def configured_binary() -> Path | None:
    """Return the executable that NVX_RS names, or None if it is unset."""
    configured = os.environ.get(VARIABLE)
    if not configured:
        return None
    path = Path(configured).absolute()
    if not path.is_file():
        raise ScriptError(f"{VARIABLE} does not name an nvx-rs executable: {path}")
    return path


def find_binary() -> Path | None:
    """Return the nvx-rs that nvx.py runs without building it, or None."""
    configured = configured_binary()
    if configured is not None:
        return configured
    path = binary_path()
    return path if path.is_file() else None


def provenance_record(executable: Path, target: str) -> dict[str, object]:
    """Return the provenance of the nvx-rs executable at `executable`, built
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
    """Record the provenance of the nvx-rs that nvx.py runs without building
    it, built from this checkout for `target`, by default the host's, and
    return where.

    The caller vouches for the build; the record binds its executable to this
    checkout's sources, which is what packaging and the adversarial executor
    check.
    """
    executable = find_binary()
    if executable is None:
        raise ScriptError(
            f"nvx-rs is not built at {binary_path()}: build it with "
            f"`{_format_command(BUILD_COMMAND)}`, or set {VARIABLE} to an "
            "nvx-rs executable"
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
    """Build the nvx-rs of this checkout with Cargo and return it."""
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
        raise ScriptError(f"failed to run Cargo to build nvx-rs: {error}") from error
    if completed.returncode != 0:
        raise ScriptError(
            f"Cargo failed to build nvx-rs (exit {completed.returncode}); fix the "
            f"build, or set {VARIABLE} to an nvx-rs executable"
        )
    path = binary_path()
    if not path.is_file():
        raise ScriptError(f"Cargo did not build {path}")
    # A Cargo build replaces the executable that an earlier record described.
    record_provenance()
    return path


def require_binary() -> Path:
    """Return the nvx-rs that nvx.py runs, and build it first if needed."""
    return find_binary() or build_binary()


def _command(arguments: Sequence[str], program: str | None) -> list[str]:
    command = [os.fspath(require_binary())]
    if program is not None:
        command.extend(("--prog", program))
    command.extend(("--root", os.fspath(ROOT), *arguments))
    return command


def _forward(process: subprocess.Popen[bytes], signum: int) -> None:
    # Popen.send_signal signals nothing once the process has exited.
    with contextlib.suppress(OSError):
        process.send_signal(signum)


@contextlib.contextmanager
def _signals_handed_over() -> Generator[Callable[[subprocess.Popen[bytes]], None]]:
    """Hand nvx-rs the signals that would end nvx.py while it runs, and yield
    the function that names the nvx-rs process once it has started.

    nvx-rs handles Ctrl-C itself: it prints Interrupted and exits with status
    130, which nvx.py then exits with, rather than printing Interrupted again.
    A terminal's Ctrl-C reaches both. On POSIX hosts, a SIGINT, SIGTERM, or
    SIGHUP sent to nvx.py alone reaches nvx-rs too, a signal that arrives
    before nvx-rs has started reaches it as it starts, and a second SIGINT
    changes nothing for nvx-rs. On Windows, where Ctrl-C reaches every process
    of the console, nvx.py only ignores it.
    """
    started: list[subprocess.Popen[bytes]] = []
    pending: list[int] = []

    def record(process: subprocess.Popen[bytes]) -> None:
        started.append(process)
        for signum in pending:
            _forward(process, signum)

    if threading.current_thread() is not threading.main_thread():
        yield record
        return
    names = ("SIGINT",) if os.name == "nt" else ("SIGINT", "SIGTERM", "SIGHUP")
    signums = [
        cast(int, getattr(signal, signal_name))
        for signal_name in names
        if hasattr(signal, signal_name)
    ]

    def hand_over(signum: int, _frame: FrameType | None) -> None:
        if os.name == "nt":
            return
        if started:
            _forward(started[0], signum)
        else:
            pending.append(signum)

    previous: list[tuple[int, Callable[[int, FrameType | None], object] | int]] = []
    try:
        for signum in signums:
            handler = signal.signal(signum, hand_over)
            previous.append((signum, signal.SIG_DFL if handler is None else handler))
        yield record
    finally:
        for signum, handler in reversed(previous):
            signal.signal(signum, handler)


def _wait(process: subprocess.Popen[bytes]) -> int:
    while True:
        try:
            return process.wait()
        except KeyboardInterrupt:
            # A handler other than nvx.py's raised it; nvx-rs, which Ctrl-C
            # reaches too, decides how the command ends.
            if os.name != "nt":
                _forward(process, signal.SIGINT)


def exit_status(returncode: int) -> int:
    """Return the status that nvx.py exits with for nvx-rs's `returncode`.

    That is nvx-rs's own status, 128 + N if signal N ended nvx-rs, as a shell
    reports it, and on Windows, a 32-bit status as the signed value that
    SystemExit passes on with every Python release.
    """
    if returncode < 0:
        return 128 - returncode
    if os.name == "nt" and returncode > 0x7FFF_FFFF:
        return returncode - (1 << 32)
    return returncode


def run(arguments: Sequence[str], *, program: str) -> int:
    """Run `nvx-rs --prog PROGRAM --root ROOT ARGUMENTS` on nvx.py's standard
    streams, and return the status that nvx.py exits with."""
    command = _command(arguments, program)
    with _signals_handed_over() as started:
        try:
            process = subprocess.Popen(command)
        except OSError as error:
            raise ScriptError(f"failed to run {command[0]}: {error}") from error
        started(process)
        returncode = _wait(process)
    if os.name != "nt" and returncode == -signal.SIGINT:
        # Ctrl-C ended nvx-rs before it could handle it, as it would have.
        print("Interrupted", file=sys.stderr)
    return exit_status(returncode)


def _error_message(stderr: bytes, returncode: int) -> str:
    text = stderr.decode("utf-8", errors="replace").replace("\r\n", "\n")
    text = text.removesuffix("\n")
    if text.startswith("error: "):
        return text.removeprefix("error: ")
    return text or f"nvx-rs exited with status {returncode}"


def query(arguments: Sequence[str]) -> object:
    """Run `nvx-rs --root ROOT ARGUMENTS`, and return the JSON that it prints.

    A failure raises ScriptError with the message that nvx-rs printed after
    `error: `, which nvx.py prints as it printed the message of the Python
    code that nvx-rs replaced.
    """
    command = _command(arguments, None)
    try:
        completed = subprocess.run(
            command, stdin=subprocess.DEVNULL, capture_output=True, check=False
        )
    except OSError as error:
        raise ScriptError(f"failed to run {command[0]}: {error}") from error
    if completed.returncode != 0:
        raise ScriptError(_error_message(completed.stderr, completed.returncode))
    try:
        return cast(object, json.loads(completed.stdout.decode("utf-8")))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ScriptError(
            f"nvx-rs {' '.join(arguments)} printed invalid JSON: {error}"
        ) from error


def _object(value: object, what: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ScriptError(f"nvx-rs printed {what} that is not a JSON object")
    return cast(dict[str, object], value)


def _string(document: dict[str, object], key: str, what: str) -> str:
    value = document.get(key)
    if not isinstance(value, str):
        raise ScriptError(f"nvx-rs printed {what} without a {key} string")
    return value


def _strings(document: dict[str, object], key: str, what: str) -> tuple[str, ...]:
    value = document.get(key)
    if not isinstance(value, list) or not all(
        isinstance(item, str) for item in cast(list[object], value)
    ):
        raise ScriptError(f"nvx-rs printed {what} without a {key} list of strings")
    return tuple(cast(list[str], value))


@dataclass(frozen=True)
class EgressRules:
    """The native OpenVMM rules that an egress policy file compiles to, in the
    order in which OpenVMM receives each action's rules."""

    allow: tuple[str, ...]
    deny: tuple[str, ...]


def compile_egress_policy(path: Path) -> EgressRules:
    """Compile the egress policy file at `path` with `nvx-rs egress compile`.

    A refused policy raises ScriptError with the compiler's message.
    """
    what = "egress rules"
    document = _object(query(("egress", "compile", os.fspath(path))), what)
    return EgressRules(
        allow=_strings(document, "allow", what),
        deny=_strings(document, "deny", what),
    )


@dataclass(frozen=True)
class GuestArtifacts:
    """The artifacts of one guest that `nvx-rs artifacts` lists."""

    name: str
    distribution: str
    initramfs: Path


@dataclass(frozen=True)
class ArtifactPaths:
    """The paths of the artifacts that nvx.py runs, which nvx-rs does not check."""

    openvmm: Path
    kernel: Path
    guests: tuple[GuestArtifacts, ...]

    def guest(self, name: str) -> GuestArtifacts:
        for guest in self.guests:
            if guest.name == name:
                return guest
        raise ScriptError(f"nvx-rs printed no artifacts of the {name} guest")


def artifacts() -> ArtifactPaths:
    """Return the paths of the artifacts of ROOT with `nvx-rs artifacts`.

    OpenVMM is the executable that NVX_OPENVMM names, if set, as for nvx.py
    sandbox, and otherwise the checkout's.
    """
    what = "artifact paths"
    document = _object(query(("artifacts",)), what)
    raw_guests = document.get("guests")
    if not isinstance(raw_guests, list):
        raise ScriptError(f"nvx-rs printed {what} without a guests list")
    guests: list[GuestArtifacts] = []
    for raw_guest in cast(list[object], raw_guests):
        guest = _object(raw_guest, "a guest")
        guests.append(
            GuestArtifacts(
                name=_string(guest, "name", "a guest"),
                distribution=_string(guest, "distribution", "a guest"),
                initramfs=Path(_string(guest, "initramfs", "a guest")),
            )
        )
    return ArtifactPaths(
        openvmm=Path(_string(document, "openvmm", what)),
        kernel=Path(_string(document, "kernel", what)),
        guests=tuple(guests),
    )
