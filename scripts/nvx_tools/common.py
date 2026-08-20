"""Shared process and configuration helpers for nvx scripts."""

from __future__ import annotations

import os
import signal
import shutil
import subprocess
import sys
import threading
import tempfile
import time
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
    returncode: int | None
    stdout: bytes
    stderr: bytes
    timed_out: bool = False
    peak_rss_bytes: int | None = None
    interrupted: bool = False
    killed: bool = False
    pid: int | None = None

    @property
    def text(self) -> str:
        stderr = self.stderr.decode("utf-8", errors="replace")
        stdout = self.stdout.decode("utf-8", errors="replace")
        return f"{stderr}\n{stdout}"


class _RssPopen(subprocess.Popen[bytes]):
    # Hooks the CPython reaping helper to read rusage; a future rename degrades to no RSS.
    def __init__(self, *args: object, **kwargs: object) -> None:
        self.peak_rss_bytes: int | None = None
        super().__init__(*args, **kwargs)

    def _try_wait(self, wait_flags: int) -> tuple[int, int]:
        try:
            pid, status, usage = os.wait4(self.pid, wait_flags)
        except ChildProcessError:
            pid, status = self.pid, 0
        else:
            if pid == self.pid:
                scale = 1 if sys.platform == "darwin" else 1024
                self.peak_rss_bytes = usage.ru_maxrss * scale
        return pid, status


class _WindowsRssSampler:
    # PeakWorkingSetSize is a kernel-maintained high-water mark, so a slow poll plus a
    # final sample after the child exits captures it without loading the benchmark host.
    _INTERVAL_SECONDS = 0.05

    def __init__(self, process: subprocess.Popen[bytes]) -> None:
        self._process = process
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._sample_until_stopped, daemon=True)
        self.peak_rss_bytes: int | None = None

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> int | None:
        self._stop.set()
        self._thread.join()
        return self.peak_rss_bytes

    def _sample_until_stopped(self) -> None:
        import ctypes
        from ctypes import wintypes

        class ProcessMemoryCounters(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("page_fault_count", wintypes.DWORD),
                ("peak_working_set_size", ctypes.c_size_t),
                ("working_set_size", ctypes.c_size_t),
                ("quota_peak_paged_pool_usage", ctypes.c_size_t),
                ("quota_paged_pool_usage", ctypes.c_size_t),
                ("quota_peak_non_paged_pool_usage", ctypes.c_size_t),
                ("quota_non_paged_pool_usage", ctypes.c_size_t),
                ("pagefile_usage", ctypes.c_size_t),
                ("peak_pagefile_usage", ctypes.c_size_t),
            ]

        get_process_memory_info = ctypes.WinDLL("psapi").GetProcessMemoryInfo
        get_process_memory_info.argtypes = [
            wintypes.HANDLE,
            ctypes.POINTER(ProcessMemoryCounters),
            wintypes.DWORD,
        ]
        get_process_memory_info.restype = wintypes.BOOL
        handle = wintypes.HANDLE(int(self._process._handle))
        stopped = False
        while True:
            counters = ProcessMemoryCounters()
            counters.cb = ctypes.sizeof(counters)
            if get_process_memory_info(handle, ctypes.byref(counters), counters.cb):
                sample = int(counters.peak_working_set_size)
                self.peak_rss_bytes = max(self.peak_rss_bytes or 0, sample)
            if stopped:
                return
            stopped = self._stop.wait(self._INTERVAL_SECONDS)


def diagnostic_tail(text: str, lines: int = 20) -> str:
    return "\n".join(text.splitlines()[-lines:])


def require_success(result: CommandResult, label: str) -> None:
    if result.timed_out:
        reason = "timed out"
    elif result.returncode != 0:
        reason = f"exited {result.returncode}"
    else:
        return
    diagnostic = diagnostic_tail(result.text)
    suffix = f"\n{diagnostic}" if diagnostic else ""
    raise ScriptError(f"{label} {reason}{suffix}")


def run_capture(
    args: Sequence[str | os.PathLike[str]],
    *,
    input_text: str = "",
    input_bytes: bytes | None = None,
    timeout: float | None = None,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    graceful_timeout: bool = False,
    collect_rss: bool = False,
    interrupt_marker: str | bytes | None = None,
    marker_timeout: float = 30,
) -> CommandResult:
    if input_text and input_bytes is not None:
        raise ValueError("input_text and input_bytes are mutually exclusive")
    command = tuple(os.fspath(arg) for arg in args)
    creationflags = (
        subprocess.CREATE_NEW_PROCESS_GROUP
        if graceful_timeout and os.name == "nt"
        else 0
    )
    process_input = (
        input_bytes if input_bytes is not None else input_text.encode("utf-8")
    )
    if interrupt_marker is not None:
        if not graceful_timeout:
            raise ValueError("interrupt_marker requires graceful_timeout")
        marker = (
            interrupt_marker.encode("utf-8")
            if isinstance(interrupt_marker, str)
            else interrupt_marker
        )
        return _run_capture_after_marker(
            command,
            process_input,
            timeout or 0,
            marker,
            marker_timeout,
            cwd,
            env,
            creationflags,
            collect_rss,
        )
    process_type = _RssPopen if collect_rss and os.name == "posix" else subprocess.Popen
    process = process_type(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=cwd,
        env=env,
        creationflags=creationflags,
    )
    rss_sampler = (
        _WindowsRssSampler(process) if collect_rss and os.name == "nt" else None
    )
    if rss_sampler is not None:
        rss_sampler.start()

    def peak_rss_bytes() -> int | None:
        if rss_sampler is not None:
            return rss_sampler.stop()
        return getattr(process, "peak_rss_bytes", None)

    try:
        stdout, stderr = process.communicate(input=process_input, timeout=timeout)
        return CommandResult(
            command,
            process.returncode,
            stdout,
            stderr,
            peak_rss_bytes=peak_rss_bytes(),
            pid=process.pid,
        )
    except subprocess.TimeoutExpired:
        interrupted = False
        killed = False
        if graceful_timeout:
            timeout_signal = (
                signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT
            )
            interrupted = True
            try:
                process.send_signal(timeout_signal)
            except ProcessLookupError:
                pass
            try:
                stdout, stderr = process.communicate(timeout=30)
            except subprocess.TimeoutExpired:
                process.kill()
                killed = True
                stdout, stderr = process.communicate()
        else:
            process.kill()
            killed = True
            stdout, stderr = process.communicate()
        return CommandResult(
            command,
            process.returncode,
            stdout,
            stderr,
            timed_out=True,
            peak_rss_bytes=peak_rss_bytes(),
            interrupted=interrupted,
            killed=killed,
            pid=process.pid,
        )
    finally:
        if rss_sampler is not None:
            rss_sampler.stop()


def _run_capture_after_marker(
    command: tuple[str, ...],
    process_input: bytes,
    interrupt_delay: float,
    marker: bytes,
    marker_timeout: float,
    cwd: Path | None,
    env: Mapping[str, str] | None,
    creationflags: int,
    collect_rss: bool,
) -> CommandResult:
    with tempfile.TemporaryDirectory(prefix="nvx-process-output-") as temporary:
        stdout_path = Path(temporary) / "stdout.bin"
        stderr_path = Path(temporary) / "stderr.bin"
        stdout_file = stdout_path.open("wb")
        stderr_file = stderr_path.open("wb")
        try:
            process_type = (
                _RssPopen if collect_rss and os.name == "posix" else subprocess.Popen
            )
            process = process_type(
                command,
                stdin=subprocess.PIPE,
                stdout=stdout_file,
                stderr=stderr_file,
                cwd=cwd,
                env=env,
                creationflags=creationflags,
            )
        finally:
            stdout_file.close()
            stderr_file.close()

        rss_sampler = (
            _WindowsRssSampler(process) if collect_rss and os.name == "nt" else None
        )
        if rss_sampler is not None:
            rss_sampler.start()

        if process.stdin is not None:
            try:
                process.stdin.write(process_input)
                process.stdin.close()
            except BrokenPipeError:
                pass

        deadline = time.monotonic() + marker_timeout
        marker_seen = False
        while process.poll() is None and time.monotonic() < deadline:
            if marker in stdout_path.read_bytes() or marker in stderr_path.read_bytes():
                marker_seen = True
                break
            time.sleep(0.01)

        timed_out = False
        interrupted = False
        killed = False
        if marker_seen:
            try:
                process.wait(timeout=interrupt_delay)
            except subprocess.TimeoutExpired:
                timed_out = True
                interrupted, killed = _interrupt_process(process)
        elif process.poll() is None:
            timed_out = True
            interrupted, killed = _interrupt_process(process)

        if process.poll() is None:
            process.wait()
        peak_rss_bytes = (
            rss_sampler.stop()
            if rss_sampler is not None
            else getattr(process, "peak_rss_bytes", None)
        )
        return CommandResult(
            command,
            process.returncode,
            stdout_path.read_bytes(),
            stderr_path.read_bytes(),
            timed_out=timed_out,
            peak_rss_bytes=peak_rss_bytes,
            interrupted=interrupted,
            killed=killed,
            pid=process.pid,
        )


def _interrupt_process(process: subprocess.Popen[bytes]) -> tuple[bool, bool]:
    timeout_signal = signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT
    try:
        process.send_signal(timeout_signal)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=30)
        return True, False
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
        return True, True


def require_file(path: Path, message: str) -> None:
    if not path.is_file():
        raise ScriptError(message)


def require_tool(name: str, message: str | None = None) -> str:
    executable = shutil.which(name)
    if executable is None:
        raise ScriptError(message or f"{name} was not found on PATH")
    return executable


def remove_tree(path: Path, *, label: str) -> None:
    target = path.expanduser().resolve()
    protected = (REPO_ROOT.resolve(), Path.cwd().resolve(), Path.home().resolve())
    if any(
        location == target or location.is_relative_to(target) for location in protected
    ):
        raise ScriptError(
            f"refusing to remove unsafe {label} directory {path} (resolves to {target})"
        )
    shutil.rmtree(target, ignore_errors=True)


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
