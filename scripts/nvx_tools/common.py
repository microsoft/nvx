"""Shared process and configuration helpers for nvx scripts."""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import stat
import subprocess
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from http.client import HTTPMessage
from pathlib import Path, PurePosixPath
from typing import IO

REPO_ROOT = Path(__file__).resolve().parents[2]
BUILD_DIR = REPO_ROOT / "build"
SOURCE_DIR = BUILD_DIR / "sources"
OPENVMM_DIR = REPO_ROOT / "openvmm"


class ScriptError(RuntimeError):
    """Raised for an actionable command-line workflow failure."""


def artifact_path(name: str) -> Path:
    return BUILD_DIR / name


def cache_root() -> Path:
    configured = os.environ.get("NVX_CACHE_DIR")
    return (
        Path(configured).expanduser().resolve()
        if configured
        else (REPO_ROOT / ".cache").resolve()
    )


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


@dataclass(frozen=True)
class VerifiedChecksumInventory:
    files: tuple[tuple[str, str], ...]
    checksum_sha256: str


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


def verify_sha256_sums(directory: Path) -> VerifiedChecksumInventory:
    checksum_file = require_file(directory / "SHA256SUMS", "source checksums")
    if checksum_file.is_symlink() or not stat.S_ISREG(checksum_file.stat().st_mode):
        raise ScriptError(f"source checksums must be a regular file: {checksum_file}")
    checksum_bytes = checksum_file.read_bytes()
    checksum_sha256 = hashlib.sha256(checksum_bytes).hexdigest()
    checksum_text = checksum_bytes.decode("ascii")

    packaged_files: set[str] = set()
    for path in directory.rglob("*"):
        file_stat = path.lstat()
        relative = path.relative_to(directory).as_posix()
        if stat.S_ISLNK(file_stat.st_mode):
            raise ScriptError(f"symlink is not allowed in checksummed tree: {relative}")
        if stat.S_ISDIR(file_stat.st_mode):
            continue
        if not stat.S_ISREG(file_stat.st_mode):
            raise ScriptError(
                f"special file is not allowed in checksummed tree: {relative}"
            )
        if relative != "SHA256SUMS":
            packaged_files.add(relative)

    listed_files: dict[str, str] = {}
    for line in checksum_text.splitlines():
        expected, separator, relative = line.partition("  ")
        path = PurePosixPath(relative)
        if (
            not separator
            or re.fullmatch(r"[0-9a-f]{64}", expected) is None
            or not relative
            or "\\" in relative
            or path.is_absolute()
            or "." in path.parts
            or ".." in path.parts
            or path.as_posix() != relative
            or (path.parts and path.parts[0].endswith(":"))
            or relative == "SHA256SUMS"
        ):
            raise ScriptError(f"malformed checksum line in {checksum_file}: {line}")
        if relative in listed_files:
            raise ScriptError(f"duplicate checksum path in {checksum_file}: {relative}")
        listed_files[relative] = expected
        packaged_path = directory.joinpath(*path.parts)
        if relative not in packaged_files:
            raise ScriptError(f"invalid checksum path in {checksum_file}: {relative}")
        actual = sha256_file(packaged_path)
        if actual != expected:
            raise ScriptError(
                f"source checksum mismatch for {relative}: {actual}, "
                f"expected {expected}"
            )
    unlisted = sorted(packaged_files - listed_files.keys())
    if unlisted:
        raise ScriptError(f"unlisted file in checksummed tree: {unlisted[0]}")
    return VerifiedChecksumInventory(
        files=tuple(sorted(listed_files.items())),
        checksum_sha256=checksum_sha256,
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


class _CrossOriginRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Drops credentials when a download leaves its origin."""

    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: IO[bytes],
        code: int,
        msg: str,
        headers: HTTPMessage,
        newurl: str,
    ) -> urllib.request.Request | None:
        redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
        if redirected is None:
            return None
        source_url = urllib.parse.urlsplit(req.full_url)
        redirect_url = urllib.parse.urlsplit(newurl)
        if (
            source_url.scheme.lower() != redirect_url.scheme.lower()
            or source_url.netloc.lower() != redirect_url.netloc.lower()
        ):
            redirected.remove_header("Authorization")
        return redirected


def credential_safe_opener() -> urllib.request.OpenerDirector:
    """Builds an opener that never forwards credentials across origins."""

    return urllib.request.build_opener(_CrossOriginRedirectHandler)


def download(
    url: str,
    destination: Path,
    attempts: int = 3,
    *,
    expected_sha256: str | None = None,
    headers: Mapping[str, str] | None = None,
    opener: urllib.request.OpenerDirector | None = None,
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
            response_context = (
                opener.open(request)
                if opener is not None
                else urllib.request.urlopen(request)
            )
            with (
                response_context as response,
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


def download_verified(url: str, destination: Path, expected_sha256: str) -> None:
    if destination.is_file():
        actual_sha256 = sha256_file(destination)
        if actual_sha256 == expected_sha256:
            return
        print(
            f">> discarding {destination.name}: SHA-256 is {actual_sha256}, "
            f"expected {expected_sha256}"
        )
        destination.unlink()
    print(f">> downloading {destination.name}")
    download(url, destination, expected_sha256=expected_sha256)


def format_size(size: int) -> str:
    units = ("B", "KiB", "MiB", "GiB")
    value = float(size)
    for unit in units:
        if value < 1024 or unit == units[-1]:
            return f"{value:.1f} {unit}" if unit != "B" else f"{size} B"
        value /= 1024
    raise AssertionError("unreachable")
