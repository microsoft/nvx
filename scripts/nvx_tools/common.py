"""Shared process and configuration helpers for nvx scripts."""

from __future__ import annotations

import hashlib
import os
import platform
import shutil
import subprocess
import urllib.error
import urllib.request
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
BUILD_DIR = REPO_ROOT / "build"
SOURCE_DIR = BUILD_DIR / "sources"
OPENVMM_DIR = REPO_ROOT / "openvmm"


class ScriptError(RuntimeError):
    """Raised for an actionable command-line workflow failure."""


SUPPORTED_ARCHITECTURES = ("x86_64", "aarch64")


def host_architecture(machine: str | None = None) -> str:
    """Return the normalized native architecture used by NVX artifacts."""
    value = (machine or platform.machine()).lower()
    normalized = {
        "amd64": "x86_64",
        "x86_64": "x86_64",
        "arm64": "aarch64",
        "aarch64": "aarch64",
    }.get(value)
    if normalized is None:
        choices = ", ".join(SUPPORTED_ARCHITECTURES)
        raise ScriptError(
            f"unsupported host architecture {value!r}; expected one of {choices}"
        )
    return normalized


def artifact_path(name: str) -> Path:
    return BUILD_DIR / name


def openvmm_binary_path() -> Path:
    suffix = ".exe" if os.name == "nt" else ""
    return OPENVMM_DIR / "target" / "release" / f"openvmm{suffix}"


@dataclass(frozen=True)
class CommandResult:
    args: tuple[str, ...]
    returncode: int
    stdout: bytes
    stderr: bytes

    @property
    def text(self) -> str:
        stderr = self.stderr.decode("utf-8", errors="replace")
        stdout = self.stdout.decode("utf-8", errors="replace")
        return f"{stderr}\n{stdout}"


def diagnostic_tail(text: str, lines: int = 20) -> str:
    return "\n".join(text.splitlines()[-lines:])


def require_success(result: CommandResult, label: str) -> None:
    if result.returncode == 0:
        return
    diagnostic = diagnostic_tail(result.text)
    suffix = f"\n{diagnostic}" if diagnostic else ""
    raise ScriptError(f"{label} exited {result.returncode}{suffix}")


def run_capture(
    args: Sequence[str | os.PathLike[str]],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
) -> CommandResult:
    command = tuple(os.fspath(arg) for arg in args)
    result = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        capture_output=True,
    )
    return CommandResult(command, result.returncode, result.stdout, result.stderr)


def require_file(path: Path, description: str) -> Path:
    if not path.is_file():
        raise ScriptError(f"{description} not found: {path}")
    return path


def require_tool(name: str, message: str | None = None) -> str:
    executable = shutil.which(name)
    if executable is None:
        raise ScriptError(message or f"{name} was not found on PATH")
    return executable


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def write_sha256_sums(directory: Path) -> None:
    files = sorted(
        path
        for path in directory.rglob("*")
        if path.is_file() and path.name != "SHA256SUMS"
    )
    lines = [
        f"{sha256_file(path)}  {path.relative_to(directory).as_posix()}"
        for path in files
    ]
    (directory / "SHA256SUMS").write_text(
        "\n".join(lines) + "\n",
        encoding="ascii",
    )


def verify_sha256_sums(directory: Path) -> None:
    checksum_file = require_file(directory / "SHA256SUMS", "source checksums")
    root = directory.resolve()
    for line in checksum_file.read_text(encoding="ascii").splitlines():
        expected, separator, relative = line.partition("  ")
        if not separator:
            raise ScriptError(f"malformed checksum line in {checksum_file}: {line}")
        path = (directory / relative).resolve()
        if root not in path.parents or not path.is_file():
            raise ScriptError(f"invalid checksum path in {checksum_file}: {relative}")
        actual = sha256_file(path)
        if actual != expected:
            raise ScriptError(
                f"source checksum mismatch for {relative}: {actual}, "
                f"expected {expected}"
            )


def run_checked(
    args: Sequence[str | os.PathLike[str]],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    input_bytes: bytes | None = None,
) -> None:
    command = tuple(os.fspath(arg) for arg in args)
    try:
        subprocess.run(
            command,
            cwd=cwd,
            env=env,
            input=input_bytes,
            check=True,
        )
    except subprocess.CalledProcessError as error:
        raise ScriptError(
            f"command failed with exit {error.returncode}: {' '.join(command)}"
        ) from error


def download(
    url: str,
    destination: Path,
    attempts: int = 3,
    *,
    expected_sha256: str | None = None,
    headers: Mapping[str, str] | None = None,
) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f"{destination.name}.part")
    for attempt in range(1, attempts + 1):
        try:
            digest = hashlib.sha256()
            request = (
                urllib.request.Request(url, headers=dict(headers))
                if headers is not None
                else url
            )
            with (
                urllib.request.urlopen(request) as response,
                temporary.open("wb") as output,
            ):
                while chunk := response.read(1024 * 1024):
                    output.write(chunk)
                    digest.update(chunk)
            actual_sha256 = digest.hexdigest()
            if expected_sha256 is not None and actual_sha256 != expected_sha256:
                temporary.unlink(missing_ok=True)
                error = (
                    f"{destination.name} SHA-256 is {actual_sha256}, "
                    f"expected {expected_sha256}"
                )
                if attempt == attempts:
                    raise ScriptError(error)
                print(f">> download failed ({attempt}/{attempts}); retrying: {error}")
                continue
            temporary.replace(destination)
            return
        except (OSError, urllib.error.URLError) as error:
            temporary.unlink(missing_ok=True)
            if attempt == attempts:
                raise ScriptError(f"failed to download {url}: {error}") from error
            print(f">> download failed ({attempt}/{attempts}); retrying: {error}")


def format_size(size: int) -> str:
    units = ("B", "KiB", "MiB", "GiB")
    value = float(size)
    for unit in units:
        if value < 1024 or unit == units[-1]:
            return f"{value:.1f} {unit}" if unit != "B" else f"{size} B"
        value /= 1024
    raise AssertionError("unreachable")
