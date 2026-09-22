"""Deterministic canaries, watchdogs, and bounded process execution."""

from __future__ import annotations

import base64
import ctypes
import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Protocol, cast

from nvx_tools.adversarial_broker import append_json_line
from nvx_tools.common import ScriptError

MAX_CAPTURE_BYTES = 16 * 1024 * 1024
MAX_OBSERVATION_BYTES = 8192
WATCHDOG_INTERVAL_SECONDS = 0.25
PROCESS_TERMINATION_WAIT_SECONDS = 5.0
PROCESS_READER_JOIN_SECONDS = 5.0
PROCESS_CLEANUP_GRACE_SECONDS = (
    PROCESS_TERMINATION_WAIT_SECONDS * 2 + PROCESS_READER_JOIN_SECONDS * 2
)
_LINUX_CHILD_SUBREAPER = 36
_WINDOWS_CREATE_SUSPENDED = 0x00000004


def _enable_linux_child_subreaper() -> None:
    if not sys.platform.startswith("linux"):
        return
    libc = ctypes.CDLL(None, use_errno=True)
    prctl = libc.prctl
    prctl.argtypes = (
        ctypes.c_int,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
    )
    prctl.restype = ctypes.c_int
    if prctl(_LINUX_CHILD_SUBREAPER, 1, 0, 0, 0) != 0:
        error = ctypes.get_errno()
        raise OSError(error, "prctl(PR_SET_CHILD_SUBREAPER) failed")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _resource_sample(path: Path) -> dict[str, object]:
    usage = shutil.disk_usage(path)
    process_times = os.times()
    sample: dict[str, object] = {
        "wall_time_ns": time.time_ns(),
        "monotonic_ns": time.monotonic_ns(),
        "disk_free_bytes": usage.free,
        "disk_used_bytes": usage.used,
        "process_user_seconds": process_times.user,
        "process_system_seconds": process_times.system,
        "children_user_seconds": process_times.children_user,
        "children_system_seconds": process_times.children_system,
    }
    getloadavg = cast(
        Callable[[], tuple[float, float, float]] | None,
        getattr(os, "getloadavg", None),
    )
    if callable(getloadavg):
        load_1, load_5, load_15 = getloadavg()
        sample["load_average"] = [load_1, load_5, load_15]
    return sample


class NetworkCanary:
    """Loopback listener that treats every connection as a policy violation."""

    def __init__(self, log_path: Path) -> None:
        self._log_path = log_path
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._listener.bind(("127.0.0.1", 0))
        self._listener.listen()
        self._listener.settimeout(WATCHDOG_INTERVAL_SECONDS)
        self._connections = 0
        self._error: str | None = None
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = threading.Thread(
            target=self._serve,
            name="nvx-network-canary",
            daemon=True,
        )

    @property
    def port(self) -> int:
        address = self._listener.getsockname()
        if not isinstance(address, tuple) or not isinstance(address[1], int):
            raise ScriptError("network canary returned an invalid address")
        return address[1]

    @property
    def connections(self) -> int:
        with self._lock:
            return self._connections

    @property
    def error(self) -> str | None:
        with self._lock:
            return self._error

    def start(self) -> None:
        self._thread.start()

    def close(self) -> None:
        self._stop.set()
        self._listener.close()
        self._thread.join(timeout=2.0)
        if self._thread.is_alive():
            raise ScriptError("network canary did not stop")

    def _serve(self) -> None:
        try:
            while not self._stop.is_set():
                try:
                    connection, peer = self._listener.accept()
                except TimeoutError:
                    continue
                except OSError as error:
                    if self._stop.is_set():
                        return
                    with self._lock:
                        self._error = str(error)
                    return
                with connection:
                    with self._lock:
                        self._connections += 1
                    append_json_line(
                        self._log_path,
                        {
                            "event": "unexpected-connection",
                            "peer": str(peer),
                            "time_ns": time.time_ns(),
                        },
                    )
        finally:
            if not self._stop.is_set():
                with self._lock:
                    if self._error is None:
                        self._error = "network canary stopped unexpectedly"


class IndependentWatchdog:
    """Continuously samples host state without consulting the strategist."""

    def __init__(
        self,
        *,
        root: Path,
        canary_path: Path,
        expected_canary_hash: str,
        network_canary: NetworkCanary,
    ) -> None:
        self._root = root
        self._canary_path = canary_path
        self._expected_canary_hash = expected_canary_hash
        self._network_canary = network_canary
        self._heartbeat_path = root / "watchdog.jsonl"
        self._stop = threading.Event()
        self._canary_intact = True
        self._error: str | None = None
        self._lock = threading.Lock()
        self._thread = threading.Thread(
            target=self._monitor,
            name="nvx-adversarial-watchdog",
            daemon=True,
        )

    @property
    def canary_intact(self) -> bool:
        with self._lock:
            return self._canary_intact

    @property
    def error(self) -> str | None:
        with self._lock:
            return self._error

    def start(self) -> None:
        self._thread.start()

    def close(self) -> None:
        self._stop.set()
        self._thread.join(timeout=2.0)
        if self._thread.is_alive():
            raise ScriptError("independent watchdog did not stop")

    def sample(self, *, event: str) -> dict[str, object]:
        self._check_canary()
        sample = _resource_sample(self._root)
        sample.update(
            {
                "event": event,
                "host_canary_intact": self.canary_intact,
                "network_canary_connections": self._network_canary.connections,
            }
        )
        append_json_line(self._heartbeat_path, sample)
        return sample

    def _check_canary(self) -> bool:
        try:
            intact = sha256_file(self._canary_path) == self._expected_canary_hash
        except OSError:
            intact = False
        if not intact:
            with self._lock:
                self._canary_intact = False
        return intact

    def _monitor(self) -> None:
        try:
            while not self._stop.wait(WATCHDOG_INTERVAL_SECONDS):
                self.sample(event="heartbeat")
        except (OSError, ValueError) as error:
            with self._lock:
                self._error = str(error)
        finally:
            if not self._stop.is_set():
                with self._lock:
                    if self._error is None:
                        self._error = "independent watchdog stopped unexpectedly"


@dataclass(slots=True)
class OracleSession:
    root: Path
    canary_path: Path
    canary_hash: str
    network_canary: NetworkCanary
    watchdog: IndependentWatchdog

    @classmethod
    def create(cls, root: Path, *, seed: int, run_id: str) -> OracleSession:
        root.mkdir(parents=True, exist_ok=False)
        canary_path = root / "host-canary.bin"
        payload_seed = hashlib.sha256(f"{seed}:{run_id}".encode()).digest()
        payload = (
            payload_seed * ((4096 + len(payload_seed) - 1) // len(payload_seed))
        )[:4096]
        canary_path.write_bytes(payload)
        canary_hash = sha256_file(canary_path)
        network_canary = NetworkCanary(root / "network-canary.jsonl")
        watchdog = IndependentWatchdog(
            root=root,
            canary_path=canary_path,
            expected_canary_hash=canary_hash,
            network_canary=network_canary,
        )
        network_canary.start()
        watchdog.start()
        watchdog.sample(event="start")
        return cls(
            root=root,
            canary_path=canary_path,
            canary_hash=canary_hash,
            network_canary=network_canary,
            watchdog=watchdog,
        )

    def close(self) -> dict[str, object]:
        cleanup_errors: list[str] = []
        try:
            self.watchdog.sample(event="stop")
        except (OSError, ScriptError, ValueError) as error:
            cleanup_errors.append(f"final watchdog sample failed: {error}")
        try:
            self.watchdog.close()
        except ScriptError as error:
            cleanup_errors.append(str(error))
        try:
            self.network_canary.close()
        except ScriptError as error:
            cleanup_errors.append(str(error))
        if cleanup_errors:
            raise ScriptError("; ".join(cleanup_errors))
        return {
            "host_canary_intact": self.watchdog.canary_intact,
            "network_canary_connections": self.network_canary.connections,
            "watchdog_error": self.watchdog.error,
            "network_canary_error": self.network_canary.error,
        }


@dataclass(frozen=True, slots=True)
class BoundedProcessResult:
    returncode: int | None
    timed_out: bool
    duration_ms: int
    stdout_path: Path
    stderr_path: Path
    stdout_bytes: int
    stderr_bytes: int
    stdout_truncated: bool
    stderr_truncated: bool
    teardown_complete: bool

    def encoded_stdout(self) -> str:
        return _encoded_sample(self.stdout_path)

    def encoded_stderr(self) -> str:
        return _encoded_sample(self.stderr_path)


@dataclass(slots=True)
class _DrainState:
    total: int = 0
    truncated: bool = False
    error: str | None = None


@dataclass(slots=True)
class _StdinState:
    error: str | None = None


def _drain_stream(stream: BinaryIO, destination: Path, state: _DrainState) -> None:
    try:
        with destination.open("wb") as output:
            while chunk := stream.read(64 * 1024):
                state.total += len(chunk)
                remaining = MAX_CAPTURE_BYTES - output.tell()
                if remaining > 0:
                    output.write(chunk[:remaining])
                if len(chunk) > remaining:
                    state.truncated = True
    except OSError as error:
        state.error = str(error)
    finally:
        stream.close()


def _write_stdin(stream: BinaryIO, data: bytes, state: _StdinState) -> None:
    try:
        stream.write(data)
        stream.flush()
    except OSError as error:
        state.error = str(error)
    finally:
        stream.close()


def _freeze_linux_process_tree(pid: int) -> list[int]:
    sigstop = cast(int, getattr(signal, "SIGSTOP"))  # noqa: B009
    try:
        os.kill(pid, sigstop)
    except ProcessLookupError:
        return []
    except PermissionError as error:
        raise ScriptError(f"cannot stop process {pid} for cleanup: {error}") from error
    children_path = Path(f"/proc/{pid}/task/{pid}/children")
    try:
        children = [int(value) for value in children_path.read_text().split()]
    except FileNotFoundError:
        children = []
    except (OSError, ValueError) as error:
        raise ScriptError(
            f"cannot enumerate process {pid} children: {error}"
        ) from error
    frozen: list[int] = []
    for child in children:
        frozen.extend(_freeze_linux_process_tree(child))
    frozen.append(pid)
    return frozen


def _linux_direct_children(pid: int) -> tuple[int, ...]:
    children_path = Path(f"/proc/{pid}/task/{pid}/children")
    try:
        return tuple(int(value) for value in children_path.read_text().split())
    except FileNotFoundError:
        return ()
    except (OSError, ValueError) as error:
        raise ScriptError(
            f"cannot enumerate process {pid} children: {error}"
        ) from error


def _reap_linux_descendants(pids: Sequence[int]) -> None:
    sigkill = cast(int, getattr(signal, "SIGKILL"))  # noqa: B009
    wnohang = cast(int, getattr(os, "WNOHANG"))  # noqa: B009
    process_tree: list[int] = []
    seen: set[int] = set()
    for pid in pids:
        for descendant in _freeze_linux_process_tree(pid):
            if descendant not in seen:
                seen.add(descendant)
                process_tree.append(descendant)
    for pid in process_tree:
        try:
            os.kill(pid, sigkill)
        except ProcessLookupError:
            continue
    deadline = time.monotonic() + PROCESS_TERMINATION_WAIT_SECONDS
    remaining = set(process_tree)
    while remaining:
        for pid in tuple(remaining):
            try:
                reaped, _ = os.waitpid(pid, wnohang)
            except ChildProcessError:
                if not Path(f"/proc/{pid}").exists():
                    remaining.remove(pid)
                continue
            if reaped == pid:
                remaining.remove(pid)
        if not remaining:
            return
        if time.monotonic() >= deadline:
            raise ScriptError(
                "timed out reaping descendant processes "
                + ", ".join(str(pid) for pid in sorted(remaining))
            )
        time.sleep(0.01)


class _WindowsDynamicFunction(Protocol):
    argtypes: object
    restype: object

    def __call__(self, *args: object, **kwargs: object) -> object: ...


class _WindowsKernel32(Protocol):
    CreateJobObjectW: _WindowsDynamicFunction
    SetInformationJobObject: _WindowsDynamicFunction
    AssignProcessToJobObject: _WindowsDynamicFunction
    QueryInformationJobObject: _WindowsDynamicFunction
    TerminateJobObject: _WindowsDynamicFunction
    CloseHandle: _WindowsDynamicFunction


class _WindowsNtdll(Protocol):
    NtResumeProcess: _WindowsDynamicFunction


class _WindowsBasicLimitInformation(ctypes.Structure):
    _fields_ = (
        ("PerProcessUserTimeLimit", ctypes.c_longlong),
        ("PerJobUserTimeLimit", ctypes.c_longlong),
        ("LimitFlags", ctypes.c_uint32),
        ("MinimumWorkingSetSize", ctypes.c_size_t),
        ("MaximumWorkingSetSize", ctypes.c_size_t),
        ("ActiveProcessLimit", ctypes.c_uint32),
        ("Affinity", ctypes.c_size_t),
        ("PriorityClass", ctypes.c_uint32),
        ("SchedulingClass", ctypes.c_uint32),
    )


class _WindowsIoCounters(ctypes.Structure):
    _fields_ = (
        ("ReadOperationCount", ctypes.c_ulonglong),
        ("WriteOperationCount", ctypes.c_ulonglong),
        ("OtherOperationCount", ctypes.c_ulonglong),
        ("ReadTransferCount", ctypes.c_ulonglong),
        ("WriteTransferCount", ctypes.c_ulonglong),
        ("OtherTransferCount", ctypes.c_ulonglong),
    )


class _WindowsExtendedLimitInformation(ctypes.Structure):
    _fields_ = (
        ("BasicLimitInformation", _WindowsBasicLimitInformation),
        ("IoInfo", _WindowsIoCounters),
        ("ProcessMemoryLimit", ctypes.c_size_t),
        ("JobMemoryLimit", ctypes.c_size_t),
        ("PeakProcessMemoryUsed", ctypes.c_size_t),
        ("PeakJobMemoryUsed", ctypes.c_size_t),
    )


class _WindowsBasicAccountingInformation(ctypes.Structure):
    _fields_ = (
        ("TotalUserTime", ctypes.c_longlong),
        ("TotalKernelTime", ctypes.c_longlong),
        ("ThisPeriodTotalUserTime", ctypes.c_longlong),
        ("ThisPeriodTotalKernelTime", ctypes.c_longlong),
        ("TotalPageFaultCount", ctypes.c_uint32),
        ("TotalProcesses", ctypes.c_uint32),
        ("ActiveProcesses", ctypes.c_uint32),
        ("TotalTerminatedProcesses", ctypes.c_uint32),
    )


class _WindowsJob:
    _KILL_ON_JOB_CLOSE = 0x00002000

    def __init__(self) -> None:
        win_dll = cast(
            Callable[..., object],
            getattr(ctypes, "WinDLL"),  # noqa: B009
        )
        get_last_error = cast(
            Callable[[], int],
            getattr(ctypes, "get_last_error"),  # noqa: B009
        )
        kernel32 = cast(
            _WindowsKernel32,
            win_dll("kernel32", use_last_error=True),
        )
        kernel32.CreateJobObjectW.argtypes = (ctypes.c_void_p, ctypes.c_wchar_p)
        kernel32.CreateJobObjectW.restype = ctypes.c_void_p
        kernel32.SetInformationJobObject.argtypes = (
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_uint32,
        )
        kernel32.SetInformationJobObject.restype = ctypes.c_int
        kernel32.AssignProcessToJobObject.argtypes = (
            ctypes.c_void_p,
            ctypes.c_void_p,
        )
        kernel32.AssignProcessToJobObject.restype = ctypes.c_int
        kernel32.QueryInformationJobObject.argtypes = (
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.c_void_p,
        )
        kernel32.QueryInformationJobObject.restype = ctypes.c_int
        kernel32.TerminateJobObject.argtypes = (ctypes.c_void_p, ctypes.c_uint32)
        kernel32.TerminateJobObject.restype = ctypes.c_int
        kernel32.CloseHandle.argtypes = (ctypes.c_void_p,)
        kernel32.CloseHandle.restype = ctypes.c_int

        handle = cast(int | None, kernel32.CreateJobObjectW(None, None))
        if not handle:
            raise OSError(get_last_error(), "CreateJobObjectW failed")
        limits = _WindowsExtendedLimitInformation()
        limits.BasicLimitInformation.LimitFlags = self._KILL_ON_JOB_CLOSE
        if not bool(
            kernel32.SetInformationJobObject(
                handle,
                9,
                ctypes.byref(limits),
                ctypes.sizeof(limits),
            )
        ):
            error = get_last_error()
            kernel32.CloseHandle(handle)
            raise OSError(error, "SetInformationJobObject failed")
        self._win_dll = win_dll
        self._get_last_error = get_last_error
        self._kernel32: _WindowsKernel32 = kernel32
        self._handle: int | None = handle

    def assign_and_resume(self, process: subprocess.Popen[bytes]) -> None:
        process_handle = cast(int | None, getattr(process, "_handle", None))
        if process_handle is None:
            raise ScriptError("Windows process handle is unavailable")
        handle = self._handle
        if handle is None:
            raise ScriptError("Windows job handle is closed")
        assigned = bool(
            self._kernel32.AssignProcessToJobObject(
                handle,
                process_handle,
            )
        )
        if not assigned:
            raise OSError(
                self._get_last_error(),
                "AssignProcessToJobObject failed",
            )
        ntdll = cast(_WindowsNtdll, self._win_dll("ntdll"))
        ntdll.NtResumeProcess.argtypes = (ctypes.c_void_p,)
        ntdll.NtResumeProcess.restype = ctypes.c_long
        resume_status = cast(int, ntdll.NtResumeProcess(process_handle))
        if resume_status != 0:
            raise OSError(
                int(resume_status),
                "NtResumeProcess failed",
            )

    def _active_processes(self) -> int:
        handle = self._handle
        if handle is None:
            raise ScriptError("Windows job handle is closed")
        accounting = _WindowsBasicAccountingInformation()
        if not bool(
            self._kernel32.QueryInformationJobObject(
                handle,
                1,
                ctypes.byref(accounting),
                ctypes.sizeof(accounting),
                None,
            )
        ):
            raise OSError(
                self._get_last_error(),
                "QueryInformationJobObject failed",
            )
        return int(accounting.ActiveProcesses)

    def terminate(self) -> None:
        if self._active_processes() == 0:
            return
        handle = self._handle
        if handle is None:
            raise ScriptError("Windows job handle is closed")
        if not bool(self._kernel32.TerminateJobObject(handle, 1)):
            raise OSError(self._get_last_error(), "TerminateJobObject failed")
        deadline = time.monotonic() + PROCESS_TERMINATION_WAIT_SECONDS
        while self._active_processes() != 0:
            if time.monotonic() >= deadline:
                raise ScriptError("Windows job cleanup timed out")
            time.sleep(0.01)

    def close(self) -> None:
        handle = self._handle
        if handle is not None:
            self._handle = None
            if not bool(self._kernel32.CloseHandle(handle)):
                raise OSError(self._get_last_error(), "CloseHandle failed")


def _cleanup_windows_job(job: _WindowsJob) -> None:
    try:
        job.terminate()
    finally:
        job.close()


def _reap_contained_descendants(
    *,
    linux_children_before: frozenset[int],
) -> None:
    if sys.platform.startswith("linux"):
        children = tuple(
            pid
            for pid in _linux_direct_children(os.getpid())
            if pid not in linux_children_before
        )
        _reap_linux_descendants(children)


def terminate_process_tree(
    process: subprocess.Popen[bytes],
    *,
    process_group: bool,
) -> None:
    if process.poll() is not None:
        return
    if os.name == "nt":
        completed = subprocess.run(
            ["taskkill.exe", "/PID", str(process.pid), "/T", "/F"],
            check=False,
            capture_output=True,
            timeout=PROCESS_TERMINATION_WAIT_SECONDS,
        )
        try:
            process.wait(timeout=PROCESS_TERMINATION_WAIT_SECONDS)
        except subprocess.TimeoutExpired as error:
            process.kill()
            process.wait(timeout=PROCESS_TERMINATION_WAIT_SECONDS)
            raise ScriptError("Windows process-tree cleanup timed out") from error
        if completed.returncode != 0 and process.returncode != 0:
            raise ScriptError(
                f"Windows process-tree cleanup failed with {completed.returncode}"
            )
        return
    if not process_group:
        process_tree = _freeze_linux_process_tree(process.pid)
        for pid in process_tree:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                continue
        process.wait(timeout=PROCESS_TERMINATION_WAIT_SECONDS)
        deadline = time.monotonic() + PROCESS_TERMINATION_WAIT_SECONDS
        for pid in process_tree[:-1]:
            while True:
                try:
                    reaped, _ = os.waitpid(pid, os.WNOHANG)
                except ChildProcessError:
                    break
                if reaped == pid:
                    break
                if time.monotonic() >= deadline:
                    raise ScriptError(f"timed out reaping descendant process {pid}")
                time.sleep(0.01)
        return
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=PROCESS_TERMINATION_WAIT_SECONDS)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    if process.poll() is None:
        process.wait(timeout=PROCESS_TERMINATION_WAIT_SECONDS)


def _terminate_process(
    process: subprocess.Popen[bytes],
    *,
    process_group: bool,
) -> None:
    try:
        terminate_process_tree(process, process_group=process_group)
    except (OSError, subprocess.SubprocessError, ScriptError):
        if process.poll() is None:
            process.kill()
            process.wait(timeout=PROCESS_TERMINATION_WAIT_SECONDS)
        raise


def run_bounded_process(
    command: Sequence[str],
    *,
    cwd: Path,
    output_dir: Path,
    timeout: float,
    environment: Mapping[str, str],
    stdin_data: bytes | None = None,
    contained_by_parent: bool = False,
) -> BoundedProcessResult:
    if timeout <= 0:
        raise ScriptError("process timeout must be greater than zero")
    if contained_by_parent:
        _enable_linux_child_subreaper()
    linux_children_before: frozenset[int] = (
        frozenset(_linux_direct_children(os.getpid()))
        if contained_by_parent and sys.platform.startswith("linux")
        else frozenset[int]()
    )
    output_dir.mkdir(parents=True, exist_ok=False)
    stdout_path = output_dir / "stdout.log"
    stderr_path = output_dir / "stderr.log"
    windows_job = _WindowsJob() if contained_by_parent and os.name == "nt" else None
    if os.name == "nt":
        creationflags = (
            _WINDOWS_CREATE_SUSPENDED
            if contained_by_parent
            else subprocess.CREATE_NEW_PROCESS_GROUP
        )
    else:
        creationflags = 0
    started = time.monotonic()
    try:
        process = subprocess.Popen(
            list(command),
            cwd=cwd,
            env=dict(environment),
            stdin=subprocess.PIPE if stdin_data is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=os.name != "nt" and not contained_by_parent,
            creationflags=creationflags,
        )
    except (OSError, subprocess.SubprocessError, ValueError):
        if windows_job is not None:
            windows_job.close()
        raise
    if windows_job is not None:
        try:
            windows_job.assign_and_resume(process)
        except (OSError, subprocess.SubprocessError, ScriptError, ValueError):
            try:
                windows_job.close()
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=PROCESS_TERMINATION_WAIT_SECONDS)
            raise
    if process.stdout is None or process.stderr is None:
        try:
            _terminate_process(process, process_group=not contained_by_parent)
        finally:
            if windows_job is not None:
                _cleanup_windows_job(windows_job)
        raise ScriptError("failed to capture adversarial executor output")
    stdout_state = _DrainState()
    stderr_state = _DrainState()
    stdout_thread = threading.Thread(
        target=_drain_stream,
        args=(process.stdout, stdout_path, stdout_state),
        name="nvx-adversarial-stdout",
        daemon=True,
    )
    stderr_thread = threading.Thread(
        target=_drain_stream,
        args=(process.stderr, stderr_path, stderr_state),
        name="nvx-adversarial-stderr",
        daemon=True,
    )
    stdout_thread.start()
    stderr_thread.start()
    stdin_state = _StdinState()
    stdin_thread: threading.Thread | None = None
    if stdin_data is not None:
        assert process.stdin is not None
        stdin_thread = threading.Thread(
            target=_write_stdin,
            args=(process.stdin, stdin_data, stdin_state),
            name="nvx-adversarial-stdin",
            daemon=True,
        )
        stdin_thread.start()
    timed_out = False
    try:
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            _terminate_process(process, process_group=not contained_by_parent)
    finally:
        if windows_job is not None:
            _cleanup_windows_job(windows_job)
        elif contained_by_parent:
            _reap_contained_descendants(
                linux_children_before=linux_children_before,
            )
    stdout_thread.join(timeout=PROCESS_READER_JOIN_SECONDS)
    stderr_thread.join(timeout=PROCESS_READER_JOIN_SECONDS)
    if stdin_thread is not None:
        stdin_thread.join(timeout=PROCESS_READER_JOIN_SECONDS)
    duration_ms = round((time.monotonic() - started) * 1000)
    if stdout_state.error is not None:
        raise ScriptError(f"failed to capture process stdout: {stdout_state.error}")
    if stderr_state.error is not None:
        raise ScriptError(f"failed to capture process stderr: {stderr_state.error}")
    if stdin_state.error is not None and not timed_out:
        raise ScriptError(f"failed to send process stdin: {stdin_state.error}")
    readers_stopped = not stdout_thread.is_alive() and not stderr_thread.is_alive()
    stdin_stopped = stdin_thread is None or not stdin_thread.is_alive()
    return BoundedProcessResult(
        returncode=process.returncode,
        timed_out=timed_out,
        duration_ms=duration_ms,
        stdout_path=stdout_path,
        stderr_path=stderr_path,
        stdout_bytes=stdout_state.total,
        stderr_bytes=stderr_state.total,
        stdout_truncated=stdout_state.truncated,
        stderr_truncated=stderr_state.truncated,
        teardown_complete=(
            process.poll() is not None and readers_stopped and stdin_stopped
        ),
    )


def _encoded_sample(path: Path) -> str:
    with path.open("rb") as stream:
        sample = stream.read(MAX_OBSERVATION_BYTES)
    return base64.b64encode(sample).decode("ascii")


def write_process_metadata(
    path: Path,
    *,
    command_name: str,
    result: BoundedProcessResult,
    resources_before: Mapping[str, object],
    resources_after: Mapping[str, object],
    openvmm_pids: Sequence[int],
    live_openvmm_pids: Sequence[int],
) -> None:
    path.write_text(
        json.dumps(
            {
                "command": command_name,
                "returncode": result.returncode,
                "timed_out": result.timed_out,
                "duration_ms": result.duration_ms,
                "stdout_bytes": result.stdout_bytes,
                "stderr_bytes": result.stderr_bytes,
                "stdout_truncated": result.stdout_truncated,
                "stderr_truncated": result.stderr_truncated,
                "teardown_complete": result.teardown_complete,
                "openvmm_pids": list(openvmm_pids),
                "live_openvmm_pids": list(live_openvmm_pids),
                "resources_before": dict(resources_before),
                "resources_after": dict(resources_after),
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
        newline="\n",
    )
