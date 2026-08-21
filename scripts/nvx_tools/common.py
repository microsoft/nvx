"""Shared process and configuration helpers for nvx scripts."""

from __future__ import annotations

import os
import shutil
import subprocess
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping, Sequence


REPO_ROOT = Path(__file__).resolve().parents[2]


class ScriptError(RuntimeError):
    """Raised for an actionable command-line workflow failure."""


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


def require_file(path: Path, message: str) -> None:
    if not path.is_file():
        raise ScriptError(message)


def require_tool(name: str, message: str | None = None) -> str:
    executable = shutil.which(name)
    if executable is None:
        raise ScriptError(message or f"{name} was not found on PATH")
    return executable


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


def download(url: str, destination: Path, attempts: int = 3) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f"{destination.name}.part")
    for attempt in range(1, attempts + 1):
        try:
            with (
                urllib.request.urlopen(url) as response,
                temporary.open("wb") as output,
            ):
                shutil.copyfileobj(response, output)
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
