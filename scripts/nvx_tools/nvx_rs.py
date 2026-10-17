"""Find, build, and run nvx-rs, the Rust command-line interface that nvx.py
delegates to.

nvx.py runs nvx-rs for `nvx.py sandbox` and for the egress policy and artifact
steps of `nvx.py run` (nvx-rs/README.md). It runs, in this order:

1. the executable that the NVX_RS environment variable names;
2. nvx-rs/target/release/nvx-rs[.exe] in this checkout, where `cargo build
   --release` puts it; or else
3. that executable, after it builds it with Cargo, whose output goes to
   standard error so that it never mixes with the output of the command.

Without Cargo, nvx.py fails and names these choices.
"""

from __future__ import annotations

import contextlib
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

from .build_constants import BuildConstants
from .common import ScriptError

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


def binary_path() -> Path:
    """Return the nvx-rs of this checkout, nvx-rs/target/release/nvx-rs[.exe],
    where BUILD_COMMAND puts it.

    This is also where a release's bin/nvx-rs[.exe] belongs once `nvx.py
    download` installs it, as it installs bin/openvmm[.exe] where the OpenVMM
    build puts it.
    """
    name = f"{BINARY_NAME}.exe" if os.name == "nt" else BINARY_NAME
    return DIRECTORY / "target" / "release" / name


def _format_command(command: Sequence[str]) -> str:
    return subprocess.list2cmdline(command) if os.name == "nt" else shlex.join(command)


def _missing_cargo_message() -> str:
    return (
        f"nvx-rs is not built at {binary_path()}, and Cargo is not on PATH to "
        f"build it: set {VARIABLE} to an nvx-rs executable, install a release "
        "that includes it with `nvx.py download`, or install Rust "
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
