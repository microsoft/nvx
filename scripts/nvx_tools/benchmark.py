#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Build and benchmark the OpenVMM microVM entrypoint on WHP, KVM, and MSHV."""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import datetime as dt
import errno
import io
import ipaddress
import json
import os
import queue
import re
import select
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import TextIO, TypedDict, cast

BOOT_MARKER = b"ALPINE-MICROVM-BOOT-OK"
RESTORE_MARKER = b"OPENVMM-SNAPSHOT-RESTORE-OK"
TEARDOWN_TIMEOUT_SECONDS = 5.0
BASE_TUNING = (
    "tsc=reliable no_timer_check random.trust_cpu=on "
    "rcupdate.rcu_expedited=1 nokaslr mitigations=off "
    "cryptomgr.notests quiet loglevel=0"
)
KVM_RESULT_PREFIX = "OPENVMM_KVM_RESULT="
KVM_RESTORE_RESULT_PREFIX = "OPENVMM_KVM_RESTORE_RESULT="
KVM_SNAPSHOT_RESULT_PREFIX = "OPENVMM_KVM_SNAPSHOT_RESULT="
PHASE2_RESULT_PREFIX = "OPENVMM_PHASE2_RESULT="
NVX_SCRIPT = Path(__file__).resolve().parents[1] / "nvx.py"
WORKLOAD_SUITES = frozenset(
    {"cold-start", "virtfs", "shell-snapshot", "network-snapshot", "performance"}
)
DD_RATE_PATTERN = re.compile(r"([0-9.]+)\s*([KMG]?)B/s")
VIRTFS_COMPLETION_MARKER = b"NVX-VIRTFS-WORKLOAD-COMPLETE"
VIRTFS_ROUNDTRIP_MARKER = b"VIRTFS-LIVE-ROUNDTRIP-OK"
SNAPSHOT_FILENAMES = ("manifest.bin", "state.bin", "memory.bin")
PERFORMANCE_LOG_FILENAMES = (
    "cold-start.log",
    "virtfs.log",
    "shell-snapshot.log",
    "network.log",
)
LEGACY_PYTHON_LOG_FILENAMES = ("snapshot.log", "snapshot-hello.log")


class BenchmarkResult(TypedDict):
    samples_ms: list[float]
    p50_ms: float
    min_ms: float
    max_ms: float
    wall_samples_ms: list[float]
    wall_p50_ms: float
    wall_min_ms: float
    wall_max_ms: float
    peak_rss_samples_bytes: list[int]
    peak_rss_p50_bytes: int
    peak_rss_min_bytes: int
    peak_rss_max_bytes: int
    teardown_samples_ms: list[float | None]
    teardown_completed_samples_ms: list[float]
    teardown_timeout_count: int
    teardown_timeout_seconds: float
    teardown_p50_ms: float | None
    teardown_min_ms: float | None
    teardown_max_ms: float | None


class SnapshotCaptureResult(TypedDict):
    samples_ms: list[float]
    p50_ms: float
    min_ms: float
    max_ms: float
    request_to_publication_samples_ms: list[float]
    request_to_publication_p50_ms: float
    request_to_publication_min_ms: float
    request_to_publication_max_ms: float
    post_publication_exit_samples_ms: list[float]
    post_publication_exit_p50_ms: float
    post_publication_exit_min_ms: float
    post_publication_exit_max_ms: float


class Phase2Metric(TypedDict):
    p50_ms: float


class Phase2Result(TypedDict):
    artifact_unchanged: bool
    memory_mib: int
    metrics: dict[str, Phase2Metric]


class ColdRestoreComparison(TypedDict):
    scope: str
    includes: list[str]
    excludes: list[str]
    cold_start_p50_ms: float
    restore_prepare_p50_ms: float
    repeat_restore_prepare_p50_ms: float
    new_process_restore_prepare_p50_ms: float
    cold_start_over_new_process_restore_prepare: float
    new_process_restore_prepare_savings_percent: float


class E2EComparison(TypedDict):
    scope: str
    cold_start_p50_ms: float
    snapshot_restore_p50_ms: float
    cold_start_over_snapshot_restore: float
    snapshot_restore_savings_percent: float


class ResultDocument(TypedDict):
    timestamp_utc: str
    controls: dict[str, object]
    backends: dict[str, BenchmarkResult]
    snapshot_capture: dict[str, SnapshotCaptureResult]
    snapshot_restore: dict[str, BenchmarkResult]
    phase2: dict[str, Phase2Result]
    comparison: dict[str, ColdRestoreComparison]
    e2e_comparison: dict[str, E2EComparison]


class GuestCommandResult(TypedDict):
    text: str
    wall_ms: float
    peak_rss_bytes: int


def configure_parser(
    parser: argparse.ArgumentParser,
    repository_dir: Path,
) -> None:
    cpu_count = os.cpu_count() or 1
    cpu_start = max(0, cpu_count - min(cpu_count, 4))
    default_cpus = (
        str(cpu_start) if cpu_start == cpu_count - 1 else f"{cpu_start}-{cpu_count - 1}"
    )
    parser.description = (
        "Build and benchmark OpenVMM microVM boot or the host-side phase 2 "
        "snapshot foundations. No NVX VMM binary is built or run."
    )
    parser.add_argument(
        "--suite",
        choices=(
            "boot",
            "snapshot",
            "restore",
            "e2e",
            "phase2",
            "all",
            *sorted(WORKLOAD_SUITES),
        ),
        default="boot",
        help=(
            "benchmark suite to run: e2e measures cold boot and full "
            "snapshot restore; performance runs the canonical non-Python "
            "workloads (default: boot)"
        ),
    )
    parser.add_argument(
        "--backend",
        choices=("whp", "kvm", "mshv", "both"),
        default="both" if os.name == "nt" else "kvm",
        help="backend to benchmark (default: both on Windows, kvm on Linux)",
    )
    parser.add_argument(
        "--openvmm-dir",
        type=Path,
        default=repository_dir / "openvmm",
        help="OpenVMM repository (default: repository submodule)",
    )
    parser.add_argument(
        "--nvx-dir",
        type=Path,
        default=repository_dir,
        help="repository containing build/vmlinux and build/initramfs.cpio.gz",
    )
    parser.add_argument("--warmups", type=positive_int, default=3)
    parser.add_argument("--runs", type=positive_int, default=11)
    parser.add_argument("--memory-mib", type=positive_int, default=128)
    parser.add_argument(
        "--virtfs-runs",
        type=positive_int,
        default=3,
        help="samples for virtfs within the performance suite (default: 3)",
    )
    parser.add_argument(
        "--virtfs-memory-mib",
        type=positive_int,
        default=512,
        help="guest memory for the virtfs workload (default: 512)",
    )
    parser.add_argument(
        "--payload-mib",
        type=positive_int,
        default=64,
        help="virtfs sequential I/O payload size (default: 64)",
    )
    parser.add_argument(
        "--shell-memories",
        type=positive_int,
        nargs="+",
        default=[64, 128, 256, 512],
        metavar="MIB",
        help="shell snapshot memory sizes (default: 64 128 256 512)",
    )
    parser.add_argument(
        "--network-memory-mib",
        type=positive_int,
        default=256,
        help="guest memory for the network snapshot workload (default: 256)",
    )
    parser.add_argument(
        "--net",
        metavar="IPV4/PREFIX",
        help=(
            "enable the microVM virtio-net device with the given static guest "
            "address (example: 10.0.0.2/24)"
        ),
    )
    memory_verification = parser.add_mutually_exclusive_group()
    memory_verification.add_argument(
        "--unsafe-skip-snapshot-memory-verification",
        dest="unsafe_skip_snapshot_memory_verification",
        action="store_true",
        help=(
            "skip restore-time SHA-256 verification of memory.bin; use only "
            "with trusted benchmark artifacts (default)"
        ),
    )
    memory_verification.add_argument(
        "--verify-snapshot-memory-sha256",
        dest="unsafe_skip_snapshot_memory_verification",
        action="store_false",
        help="enable restore-time SHA-256 verification of memory.bin",
    )
    parser.set_defaults(unsafe_skip_snapshot_memory_verification=True)
    parser.add_argument(
        "--cpus",
        default=default_cpus,
        help=f"logical CPUs used for affinity, in taskset syntax (default: {default_cpus})",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=10.0,
        help="seconds allowed for each boot marker (default: 10)",
    )
    parser.add_argument(
        "--teardown-mode",
        choices=("guest-exit", "host-terminate", "host-sigterm"),
        default="guest-exit",
        help=(
            "teardown trigger after the guest marker; host-sigterm is a "
            "deprecated alias for host-terminate (default: guest-exit)"
        ),
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="reuse existing release binaries",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="optional JSON result path",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help="write canonical workload logs to this directory",
    )
    parser.add_argument(
        "--keep-kvm-stage",
        action="store_true",
        help="keep temporary staged KVM benchmark binaries",
    )

    # The Windows coordinator reinvokes the NVX CLI inside WSL for KVM so the
    # timed process does not include one wsl.exe launch per sample.
    parser.add_argument("--_kvm-worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument(
        "--_stage-dir",
        default="/tmp/openvmm-microvm-benchmark",
        help=argparse.SUPPRESS,
    )
    parser.set_defaults(handler=run)


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than 0")
    return parsed


def run_checked(command: Sequence[str], *, cwd: Path | None = None) -> None:
    print("+", subprocess.list2cmdline(list(command)), flush=True)
    subprocess.run(command, cwd=cwd, check=True)


def require_file(path: Path, description: str) -> Path:
    path = path.resolve()
    if not path.is_file():
        raise FileNotFoundError(f"{description} not found: {path}")
    return path


def parse_cpu_set(spec: str) -> set[int]:
    cpus: set[int] = set()
    try:
        for part in spec.split(","):
            part = part.strip()
            if not part:
                raise ValueError
            if "-" in part:
                start_text, end_text = part.split("-", 1)
                start = int(start_text)
                end = int(end_text)
                if start > end:
                    raise ValueError
                cpus.update(range(start, end + 1))
            else:
                cpus.add(int(part))
    except ValueError as error:
        raise ValueError(f"invalid CPU set {spec!r}") from error
    if not cpus or min(cpus) < 0:
        raise ValueError(f"invalid CPU set {spec!r}")
    return cpus


def set_windows_affinity(pid: int, cpus: set[int]) -> None:
    if os.name != "nt":
        return
    mask = sum(1 << cpu for cpu in cpus)
    process_set_information = 0x0200
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.OpenProcess.argtypes = (ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32)
    kernel32.OpenProcess.restype = ctypes.c_void_p
    kernel32.SetProcessAffinityMask.argtypes = (ctypes.c_void_p, ctypes.c_size_t)
    kernel32.SetProcessAffinityMask.restype = ctypes.c_int
    kernel32.CloseHandle.argtypes = (ctypes.c_void_p,)

    handle = kernel32.OpenProcess(process_set_information, False, pid)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        if not kernel32.SetProcessAffinityMask(handle, mask):
            raise ctypes.WinError(ctypes.get_last_error())
    finally:
        kernel32.CloseHandle(handle)


class ProcessMemoryCounters(ctypes.Structure):
    _fields_ = [
        ("cb", ctypes.c_uint32),
        ("page_fault_count", ctypes.c_uint32),
        ("peak_working_set_size", ctypes.c_size_t),
        ("working_set_size", ctypes.c_size_t),
        ("quota_peak_paged_pool_usage", ctypes.c_size_t),
        ("quota_paged_pool_usage", ctypes.c_size_t),
        ("quota_peak_nonpaged_pool_usage", ctypes.c_size_t),
        ("quota_nonpaged_pool_usage", ctypes.c_size_t),
        ("pagefile_usage", ctypes.c_size_t),
        ("peak_pagefile_usage", ctypes.c_size_t),
    ]


def windows_peak_rss_bytes(pid: int) -> int:
    if os.name != "nt":
        raise RuntimeError("Windows process memory counters are unavailable")
    process_query_limited_information = 0x1000
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    kernel32.OpenProcess.argtypes = (ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32)
    kernel32.OpenProcess.restype = ctypes.c_void_p
    kernel32.CloseHandle.argtypes = (ctypes.c_void_p,)
    psapi.GetProcessMemoryInfo.argtypes = (
        ctypes.c_void_p,
        ctypes.POINTER(ProcessMemoryCounters),
        ctypes.c_uint32,
    )
    psapi.GetProcessMemoryInfo.restype = ctypes.c_int

    handle = kernel32.OpenProcess(process_query_limited_information, False, pid)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        counters = ProcessMemoryCounters()
        counters.cb = ctypes.sizeof(counters)
        if not psapi.GetProcessMemoryInfo(
            handle,
            ctypes.byref(counters),
            counters.cb,
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        return int(counters.peak_working_set_size)
    finally:
        kernel32.CloseHandle(handle)


def linux_peak_rss_bytes(pid: int) -> int:
    status = Path(f"/proc/{pid}/status").read_text(encoding="ascii")
    for line in status.splitlines():
        if line.startswith("VmHWM:"):
            _, value, unit = line.split()
            if unit != "kB":
                raise RuntimeError(f"unexpected VmHWM unit {unit!r}")
            return int(value) * 1024
    raise RuntimeError(f"VmHWM is unavailable for process {pid}")


def peak_rss_bytes(pid: int) -> int:
    if os.name == "nt":
        return windows_peak_rss_bytes(pid)
    if sys.platform.startswith("linux"):
        return linux_peak_rss_bytes(pid)
    raise RuntimeError(f"peak RSS measurement is unsupported on {sys.platform}")


def bytes_to_mib(value: int | float) -> float:
    return value / (1024 * 1024)


def terminate(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is None:
        process.kill()
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def wait_for_process_exit(process: subprocess.Popen[bytes], timeout: float) -> int:
    pidfd_open = cast(
        Callable[[int], int] | None,
        getattr(os, "pidfd_open", None),
    )
    if sys.platform.startswith("linux") and pidfd_open is not None:
        pidfd = pidfd_open(process.pid)
        try:
            readable, _, _ = select.select([pidfd], [], [], timeout)
            if not readable:
                raise subprocess.TimeoutExpired(process.args, timeout)
            return process.wait()
        finally:
            os.close(pidfd)
    return process.wait(timeout=timeout)


def contains_output_line(output: bytes | bytearray, marker: bytes) -> bool:
    return any(line.removesuffix(b"\r") == marker for line in output.split(b"\n"))


class InteractiveProcess:
    def __init__(self, command: Sequence[str], environment: dict[str, str]) -> None:
        self.terminal_fd: int | None = None
        if sys.platform.startswith("linux"):
            openpty = cast(
                Callable[[], tuple[int, int]] | None,
                getattr(os, "openpty", None),
            )
            if openpty is None:
                raise RuntimeError("pseudo-terminal support is unavailable")
            terminal_fd, child_fd = openpty()
            try:
                self.process = subprocess.Popen(
                    command,
                    stdin=child_fd,
                    stdout=child_fd,
                    stderr=child_fd,
                    env=environment,
                )
            except Exception:
                os.close(terminal_fd)
                raise
            finally:
                os.close(child_fd)
            self.terminal_fd = terminal_fd
        else:
            self.process = subprocess.Popen(
                command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                env=environment,
            )

    def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
        try:
            if self.terminal_fd is not None:
                while True:
                    try:
                        chunk = os.read(self.terminal_fd, 4096)
                    except OSError as error:
                        if error.errno in (errno.EBADF, errno.EIO):
                            break
                        raise
                    if not chunk:
                        break
                    chunks.put(chunk)
            else:
                assert self.process.stdout is not None
                stream = cast(io.BufferedReader, self.process.stdout)
                while chunk := stream.read1(4096):
                    chunks.put(chunk)
        finally:
            chunks.put(None)

    def write_input(self, data: bytes) -> None:
        if self.terminal_fd is not None:
            remaining = memoryview(data)
            while remaining:
                remaining = remaining[os.write(self.terminal_fd, remaining) :]
        else:
            assert self.process.stdin is not None
            self.process.stdin.write(data)
            self.process.stdin.flush()

    def close(self) -> None:
        if self.terminal_fd is not None:
            os.close(self.terminal_fd)
            self.terminal_fd = None


def cleanup_managed_tap(pid: int) -> None:
    if not sys.platform.startswith("linux"):
        return
    name = f"ovm{pid}"
    query = subprocess.run(
        ["ip", "link", "show", "dev", name],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if query.returncode != 0:
        return
    geteuid = cast(Callable[[], int] | None, getattr(os, "geteuid", None))
    command = ["ip"] if geteuid is not None and geteuid() == 0 else ["sudo", "-n", "ip"]
    completed = subprocess.run(
        [*command, "tuntap", "del", "dev", name, "mode", "tap"],
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"failed to remove managed TAP {name}: {completed.stderr.strip()}"
        )


def measure_once(
    command: Sequence[str],
    *,
    environment: dict[str, str],
    timeout: float,
    marker: bytes = BOOT_MARKER,
    windows_cpus: set[int] | None = None,
    teardown_mode: str = "guest-exit",
    cleanup_managed_network: bool = False,
) -> tuple[float, int, float | None, float]:
    started = time.perf_counter_ns()
    interaction = InteractiveProcess(command, environment)
    process = interaction.process
    if windows_cpus is not None:
        set_windows_affinity(process.pid, windows_cpus)

    chunks: queue.Queue[bytes | None] = queue.Queue()
    threading.Thread(
        target=interaction.read_output, args=(chunks,), daemon=True
    ).start()
    deadline = time.monotonic() + timeout
    output = bytearray()
    try:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"guest marker was not observed within {timeout:g}s")
            try:
                chunk = chunks.get(timeout=min(remaining, 0.25))
            except queue.Empty:
                if process.poll() is not None:
                    raise RuntimeError(
                        f"OpenVMM exited with status {process.returncode}"
                    ) from None
                continue
            if chunk is None:
                raise RuntimeError(f"OpenVMM exited with status {process.poll()}")
            output.extend(chunk)
            if marker in output:
                marker_reached = time.perf_counter_ns()
                elapsed_ms = (marker_reached - started) / 1_000_000
                peak_bytes = peak_rss_bytes(process.pid)
                teardown_started = time.perf_counter_ns()
                if teardown_mode != "guest-exit":
                    process.terminate()
                else:
                    interaction.write_input(b"nvx-exit 0\n")
                try:
                    returncode = wait_for_process_exit(
                        process,
                        TEARDOWN_TIMEOUT_SECONDS,
                    )
                except subprocess.TimeoutExpired:
                    terminate(process)
                    wall_ms = (time.perf_counter_ns() - started) / 1_000_000
                    return elapsed_ms, peak_bytes, None, wall_ms
                process_exited = time.perf_counter_ns()
                teardown_ms = (process_exited - teardown_started) / 1_000_000
                wall_ms = (process_exited - started) / 1_000_000
                if teardown_mode == "guest-exit" and returncode != 0:
                    raise RuntimeError(
                        f"OpenVMM exited with status {returncode} during teardown"
                    )
                return elapsed_ms, peak_bytes, teardown_ms, wall_ms
            if len(output) > 1024 * 1024:
                del output[: len(output) - 1024 * 1024]
    except Exception as error:
        terminate(process)
        tail = output[-4096:].decode("utf-8", "replace")
        if tail:
            raise RuntimeError(f"{error}\n--- OpenVMM output ---\n{tail}") from error
        raise
    finally:
        interaction.close()
        if cleanup_managed_network:
            cleanup_managed_tap(process.pid)


def benchmark(
    command: Sequence[str],
    *,
    warmups: int,
    runs: int,
    timeout: float,
    marker: bytes = BOOT_MARKER,
    windows_cpus: set[int] | None = None,
    teardown_mode: str = "guest-exit",
    cleanup_managed_network: bool = False,
) -> BenchmarkResult:
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    for index in range(warmups):
        value, peak_bytes, teardown_ms, _wall_ms = measure_once(
            command,
            environment=environment,
            timeout=timeout,
            marker=marker,
            windows_cpus=windows_cpus,
            teardown_mode=teardown_mode,
            cleanup_managed_network=cleanup_managed_network,
        )
        teardown = (
            f"{teardown_ms:.3f} ms"
            if teardown_ms is not None
            else f"TIMEOUT (>{TEARDOWN_TIMEOUT_SECONDS:g}s)"
        )
        print(
            f"  warmup {index + 1}/{warmups}: {value:.3f} ms, "
            f"peak RSS={bytes_to_mib(peak_bytes):.3f} MiB, teardown={teardown}",
            flush=True,
        )

    samples: list[float] = []
    wall_samples: list[float] = []
    peak_rss_samples: list[int] = []
    teardown_samples: list[float | None] = []
    for index in range(runs):
        value, peak_bytes, teardown_ms, wall_ms = measure_once(
            command,
            environment=environment,
            timeout=timeout,
            marker=marker,
            windows_cpus=windows_cpus,
            teardown_mode=teardown_mode,
            cleanup_managed_network=cleanup_managed_network,
        )
        samples.append(value)
        wall_samples.append(wall_ms)
        peak_rss_samples.append(peak_bytes)
        teardown_samples.append(teardown_ms)
        teardown = (
            f"{teardown_ms:.3f} ms"
            if teardown_ms is not None
            else f"TIMEOUT (>{TEARDOWN_TIMEOUT_SECONDS:g}s)"
        )
        print(
            f"  sample {index + 1}/{runs}: {value:.3f} ms, "
            f"peak RSS={bytes_to_mib(peak_bytes):.3f} MiB, teardown={teardown}",
            flush=True,
        )

    completed_teardowns = [value for value in teardown_samples if value is not None]
    return {
        "samples_ms": samples,
        "p50_ms": statistics.median(samples),
        "min_ms": min(samples),
        "max_ms": max(samples),
        "wall_samples_ms": wall_samples,
        "wall_p50_ms": statistics.median(wall_samples),
        "wall_min_ms": min(wall_samples),
        "wall_max_ms": max(wall_samples),
        "peak_rss_samples_bytes": peak_rss_samples,
        "peak_rss_p50_bytes": int(statistics.median(peak_rss_samples)),
        "peak_rss_min_bytes": min(peak_rss_samples),
        "peak_rss_max_bytes": max(peak_rss_samples),
        "teardown_samples_ms": teardown_samples,
        "teardown_completed_samples_ms": completed_teardowns,
        "teardown_timeout_count": len(teardown_samples) - len(completed_teardowns),
        "teardown_timeout_seconds": TEARDOWN_TIMEOUT_SECONDS,
        "teardown_p50_ms": (
            statistics.median(completed_teardowns) if completed_teardowns else None
        ),
        "teardown_min_ms": min(completed_teardowns, default=None),
        "teardown_max_ms": max(completed_teardowns, default=None),
    }


def format_sample_summary(samples: Sequence[float], *, unit: str = "ms") -> str:
    if not samples:
        raise ValueError("cannot summarize an empty sample set")
    return (
        f"{statistics.median(samples):.1f} {unit}  "
        f"(min {min(samples):.1f}, max {max(samples):.1f}, n={len(samples)})"
    )


def clocksource_parameter(backend: str) -> str:
    return "clocksource=kvm-clock" if backend == "kvm" else "clocksource=tsc"


def network_gateway(spec: str) -> str:
    try:
        interface = ipaddress.IPv4Interface(spec)
    except ValueError as error:
        raise ValueError(f"invalid network IPv4 CIDR {spec!r}: {error}") from error
    if not 1 <= interface.network.prefixlen <= 30:
        raise ValueError(f"network prefix must be in 1..30, got {spec!r}")
    return str(interface.network.network_address + 1)


def parse_dd_rate(text: str, occurrence: int) -> float | None:
    lines = [line for line in text.splitlines() if "copied" in line]
    if len(lines) < occurrence:
        return None
    matches = DD_RATE_PATTERN.findall(lines[occurrence - 1])
    if not matches:
        return None
    raw_value, prefix = matches[-1]
    multiplier = {"": 1e-6, "K": 1e-3, "M": 1.0, "G": 1e3}[prefix]
    return float(raw_value) * multiplier


def workload_boot_command(
    executable: Path,
    backend: str,
    kernel: Path,
    initrd: Path,
    memory_mib: int,
    cmdline: str,
    *,
    command_prefix: Sequence[str] = (),
    network: str | None = None,
    mount: str | None = None,
) -> list[str]:
    command = [
        *command_prefix,
        str(executable),
        "--single-process",
        "--machine",
        "microvm",
        "--hypervisor",
        backend,
        "--memory",
        f"{memory_mib}M",
        "--kernel",
        str(kernel),
        "--initrd",
        str(initrd),
        "--cmdline",
        cmdline,
    ]
    if network is not None:
        command.extend(("--net", network))
    if mount is not None:
        command.extend(("--mount", mount))
    return command


def _try_peak_rss(process: subprocess.Popen[bytes], current: int) -> int:
    if process.poll() is not None:
        return current
    try:
        return max(current, peak_rss_bytes(process.pid))
    except (OSError, RuntimeError):
        return current


def run_guest_script(
    command: Sequence[str],
    script: str,
    completion_marker: bytes,
    *,
    timeout: float,
    windows_cpus: set[int] | None = None,
    teardown_mode: str = "guest-exit",
) -> GuestCommandResult:
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    started_ns = time.perf_counter_ns()
    interaction = InteractiveProcess(command, environment)
    process = interaction.process
    if windows_cpus is not None:
        set_windows_affinity(process.pid, windows_cpus)

    chunks: queue.Queue[bytes | None] = queue.Queue()
    threading.Thread(
        target=interaction.read_output, args=(chunks,), daemon=True
    ).start()
    deadline = time.monotonic() + timeout
    output = bytearray()
    input_sent = False
    completed = False
    peak_bytes = 0
    try:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"guest workload did not finish within {timeout:g}s")
            try:
                chunk = chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                peak_bytes = _try_peak_rss(process, peak_bytes)
                if process.poll() is not None:
                    continue
                continue
            if chunk is None:
                break
            output.extend(chunk)
            peak_bytes = _try_peak_rss(process, peak_bytes)
            if not input_sent and BOOT_MARKER in output:
                interaction.write_input(script.encode("utf-8"))
                input_sent = True
            if input_sent and contains_output_line(output, completion_marker):
                completed = True
                if teardown_mode != "guest-exit" and process.poll() is None:
                    process.terminate()
                    deadline = min(
                        deadline,
                        time.monotonic() + TEARDOWN_TIMEOUT_SECONDS,
                    )

        returncode = process.wait()
        if teardown_mode == "guest-exit" and returncode != 0:
            raise RuntimeError(f"OpenVMM exited with status {returncode}")
        if not input_sent:
            raise RuntimeError("guest exited before its boot marker")
        if not completed:
            raise RuntimeError(
                f"guest exited without completion marker {completion_marker.decode()!r}"
            )
        return {
            "text": output.decode("utf-8", "replace"),
            "wall_ms": (time.perf_counter_ns() - started_ns) / 1_000_000,
            "peak_rss_bytes": peak_bytes,
        }
    except Exception as error:
        terminate(process)
        tail = output[-4096:].decode("utf-8", "replace")
        if tail:
            raise RuntimeError(f"{error}\n--- OpenVMM output ---\n{tail}") from error
        raise
    finally:
        interaction.close()


def capture_automatic_snapshot(
    command: Sequence[str],
    snapshot_path: Path,
    *,
    timeout: float,
    required_markers: Sequence[bytes] = (),
    windows_cpus: set[int] | None = None,
) -> None:
    if snapshot_path.exists():
        shutil.rmtree(snapshot_path)
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    interaction = InteractiveProcess(command, environment)
    process = interaction.process
    if windows_cpus is not None:
        set_windows_affinity(process.pid, windows_cpus)

    chunks: queue.Queue[bytes | None] = queue.Queue()
    threading.Thread(
        target=interaction.read_output, args=(chunks,), daemon=True
    ).start()
    deadline = time.monotonic() + timeout
    output = bytearray()
    try:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"snapshot was not captured within {timeout:g}s")
            try:
                chunk = chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                continue
            if chunk is None:
                break
            output.extend(chunk)

        returncode = process.wait()
        if returncode != 0:
            raise RuntimeError(f"snapshot source exited with status {returncode}")
        for marker in required_markers:
            if marker not in output:
                raise RuntimeError(
                    f"snapshot source did not emit required marker {marker.decode()!r}"
                )
        for filename in SNAPSHOT_FILENAMES:
            if not (snapshot_path / filename).is_file():
                raise RuntimeError(
                    f"snapshot did not publish {snapshot_path / filename}"
                )
    except Exception as error:
        terminate(process)
        tail = output[-4096:].decode("utf-8", "replace")
        if tail:
            raise RuntimeError(f"{error}\n--- OpenVMM output ---\n{tail}") from error
        raise
    finally:
        interaction.close()


def format_rss_summary(samples: Sequence[int]) -> str:
    if not samples:
        return "n/a"
    values = [bytes_to_mib(value) for value in samples]
    return (
        f"{statistics.median(values):.1f} MiB  "
        f"(min {min(values):.1f}, max {max(values):.1f}, n={len(values)})"
    )


def benchmark_cold_start_workload(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> None:
    clocksource = clocksource_parameter(backend)
    scenarios = (
        ("base", None),
        (clocksource, clocksource),
        ("tsc=reliable", "tsc=reliable"),
        ("no_timer_check", "no_timer_check"),
        ("random.trust_cpu=on", "random.trust_cpu=on"),
        ("rcupdate.rcu_expedited=1", "rcupdate.rcu_expedited=1"),
        ("nokaslr", "nokaslr"),
        ("mitigations=off", "mitigations=off"),
        ("cryptomgr.notests", "cryptomgr.notests"),
    )
    print(
        "cold-start (OpenVMM process launch -> shell marker), "
        f"median of {args.runs} runs, {args.memory_mib} MiB, 1 vCPU"
    )
    print()
    print("isolated kernel command-line scenarios:")
    for label, parameter in scenarios:
        cmdline = "quiet loglevel=0"
        if parameter is not None:
            cmdline = f"{cmdline} {parameter}"
        result = benchmark(
            workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                args.memory_mib,
                cmdline,
                command_prefix=command_prefix,
            ),
            warmups=args.warmups,
            runs=args.runs,
            timeout=args.timeout,
            windows_cpus=windows_cpus,
            teardown_mode=args.teardown_mode,
        )
        print(f"  {label:<25}: {format_sample_summary(result['samples_ms'])}")


def _format_rate_summary(samples: Sequence[float]) -> str:
    if not samples:
        raise ValueError("cannot summarize an empty throughput sample set")
    return (
        f"{statistics.median(samples):8.1f} MB/s  "
        f"(min {min(samples):.1f}, max {max(samples):.1f}, n={len(samples)})"
    )


def _guest_exit_script(teardown_mode: str) -> str:
    return "nvx-exit 0\n" if teardown_mode == "guest-exit" else ""


def _virtfs_script(payload_mib: int, teardown_mode: str) -> str:
    return (
        f"dd if=/dev/zero of=/mnt/host/bench.bin bs=1M count={payload_mib} "
        "conv=fsync 2>&1\n"
        "sync\n"
        "echo 3 > /proc/sys/vm/drop_caches 2>/dev/null\n"
        "dd if=/mnt/host/bench.bin of=/dev/null bs=1M 2>&1\n"
        "sync\n"
        f"echo {VIRTFS_COMPLETION_MARKER.decode()}\n"
        + _guest_exit_script(teardown_mode)
    )


def _virtfs_roundtrip_script(teardown_mode: str) -> str:
    return (
        "printf 'guest-to-host\\n' > /mnt/host/guest-visible\n"
        "tries=0\n"
        'while [ "$(cat /mnt/host/host-visible 2>/dev/null)" != host-to-guest ] '
        '&& [ "$tries" -lt 1200 ]; do sleep 0.05; tries=$((tries + 1)); done\n'
        'if [ "$(cat /mnt/host/host-visible 2>/dev/null)" = host-to-guest ]; then\n'
        f"  echo {VIRTFS_ROUNDTRIP_MARKER.decode()}\n"
        "fi\n" + _guest_exit_script(teardown_mode)
    )


def _run_virtfs_roundtrip(
    command: Sequence[str],
    directory: Path,
    run_number: int,
    runs: int,
    *,
    timeout: float,
    windows_cpus: set[int] | None,
    teardown_mode: str,
) -> GuestCommandResult:
    guest_visible = directory / "guest-visible"
    host_visible = directory / "host-visible"
    guest_visible.unlink(missing_ok=True)
    host_visible.write_text("waiting\n", encoding="ascii")
    errors: list[str] = []

    def exchange() -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                if guest_visible.read_text(encoding="ascii") == "guest-to-host\n":
                    host_visible.write_text("host-to-guest\n", encoding="ascii")
                    return
            except (FileNotFoundError, PermissionError, UnicodeError):
                pass
            time.sleep(0.01)
        errors.append("host did not observe the guest-created marker")

    worker = threading.Thread(
        target=exchange,
        name="virtfs-live-exchange",
        daemon=True,
    )
    worker.start()
    result = run_guest_script(
        command,
        _virtfs_roundtrip_script(teardown_mode),
        VIRTFS_ROUNDTRIP_MARKER,
        timeout=timeout,
        windows_cpus=windows_cpus,
        teardown_mode=teardown_mode,
    )
    worker.join(timeout=1)
    if worker.is_alive():
        errors.append("host exchange worker did not finish")
    if errors:
        raise RuntimeError(
            f"live virtfs exchange {run_number}/{runs} failed: {'; '.join(errors)}"
        )
    return result


def benchmark_virtfs_workload(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    runs: int,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> None:
    with tempfile.TemporaryDirectory(prefix="openvmm-virtfs-") as temporary:
        directory = Path(temporary)
        if "," in str(directory):
            raise ValueError("virtfs benchmark directory must not contain a comma")
        mount = f"/mnt/host,{directory},rw"
        command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            args.virtfs_memory_mib,
            "quiet loglevel=0",
            command_prefix=command_prefix,
            mount=mount,
        )
        print(
            f"virtfs benchmark: {args.payload_mib} MiB payload, "
            f"{args.virtfs_memory_mib} MiB guest, 1 vCPU, median of {runs} runs"
        )
        print()
        print("== sequential throughput (guest dd, conv=fsync writes) ==")
        write_rates: list[float] = []
        read_rates: list[float] = []
        io_rss: list[int] = []
        for run_number in range(1, runs + 1):
            result = run_guest_script(
                command,
                _virtfs_script(args.payload_mib, args.teardown_mode),
                VIRTFS_COMPLETION_MARKER,
                timeout=max(args.timeout, 300.0),
                windows_cpus=windows_cpus,
                teardown_mode=args.teardown_mode,
            )
            write_rate = parse_dd_rate(result["text"], 1)
            read_rate = parse_dd_rate(result["text"], 2)
            if write_rate is None or read_rate is None:
                raise RuntimeError(
                    f"virtfs run {run_number}/{runs} did not report both dd rates"
                )
            write_rates.append(write_rate)
            read_rates.append(read_rate)
            io_rss.append(result["peak_rss_bytes"])
        print(
            f"  {'rw live host directory':<27} write {_format_rate_summary(write_rates)}"
        )
        print(f"  {'':<27} read  {_format_rate_summary(read_rates)}")
        print(f"  OpenVMM peak RSS                  : {format_rss_summary(io_rss)}")
        print()

        print("== live host <-> guest visibility (same running VM) ==")
        roundtrip_samples: list[float] = []
        roundtrip_rss: list[int] = []
        for run_number in range(1, runs + 1):
            result = _run_virtfs_roundtrip(
                command,
                directory,
                run_number,
                runs,
                timeout=max(args.timeout, 120.0),
                windows_cpus=windows_cpus,
                teardown_mode=args.teardown_mode,
            )
            roundtrip_samples.append(result["wall_ms"])
            roundtrip_rss.append(result["peak_rss_bytes"])
        print(
            "  live exchange (cold each)         : "
            + format_sample_summary(roundtrip_samples)
        )
        print(
            f"  OpenVMM peak RSS                  : {format_rss_summary(roundtrip_rss)}"
        )


def _print_shell_snapshot_summary(
    memory_mib: int,
    cold: Sequence[float],
    restored: Sequence[float],
) -> None:
    split_ms = 900.0
    fast = [value for value in cold if value < split_ms]
    slow = [value for value in cold if value >= split_ms]

    def line(name: str, values: Sequence[float]) -> str:
        return (
            f"  {name:<20}: median {statistics.median(values):7.1f} ms   "
            f"(min {min(values):.1f}, max {max(values):.1f}, n={len(values)})"
        )

    print(f"== {memory_mib} MiB ==")
    print(line("cold boot", cold))
    if fast and slow:
        print(
            f"       fast path {statistics.median(fast):7.1f} ms (n={len(fast)})  |  "
            f"slow path {statistics.median(slow):7.1f} ms (n={len(slow)}, "
            f"+~{statistics.median(slow) - statistics.median(fast):.0f} ms "
            "TSC PIT-calib)"
        )
    print(line("snapshot restore", restored))
    restore_p50 = statistics.median(restored)
    if restore_p50 > 0:
        base = statistics.median(fast) if fast else statistics.median(cold)
        print(
            f"  {'speedup':<20}: {base / restore_p50:.0f}x (fast-path cold) .. "
            f"{statistics.median(cold) / restore_p50:.0f}x (median cold) "
            "faster via snapshot"
        )
    print()


def benchmark_shell_snapshot_workload(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> None:
    print(
        "boot-to-shell: cold OpenVMM launch vs snapshot restore, "
        f"median of {args.runs} runs, 1 vCPU"
    )
    print(f'marker : "{BOOT_MARKER.decode()}"')
    print(f"kernel : {kernel}")
    print(f"initrd : {initrd}")
    print()
    with tempfile.TemporaryDirectory(prefix="openvmm-shell-snapshot-") as temporary:
        root = Path(temporary)
        for memory_mib in args.shell_memories:
            cold_command = workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                command_prefix=command_prefix,
            )
            cold = benchmark(
                cold_command,
                warmups=args.warmups,
                runs=args.runs,
                timeout=args.timeout,
                windows_cpus=windows_cpus,
                teardown_mode=args.teardown_mode,
            )

            snapshot_path = root / f"shell-{memory_mib}-mib"
            capture_command = workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0 shellsnap",
                command_prefix=command_prefix,
            )
            capture_automatic_snapshot(
                [
                    *capture_command,
                    "--snapshot-destination",
                    str(snapshot_path),
                ],
                snapshot_path,
                timeout=max(args.timeout, 30.0),
                windows_cpus=windows_cpus,
            )
            restored = benchmark(
                [
                    *command_prefix,
                    *snapshot_restore_command(
                        executable,
                        backend,
                        snapshot_path,
                        args.unsafe_skip_snapshot_memory_verification,
                    ),
                ],
                warmups=args.warmups,
                runs=args.runs,
                timeout=args.timeout,
                marker=BOOT_MARKER,
                windows_cpus=windows_cpus,
                teardown_mode=args.teardown_mode,
            )
            _print_shell_snapshot_summary(
                memory_mib,
                cold["samples_ms"],
                restored["samples_ms"],
            )


def benchmark_network_snapshot_workload(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> None:
    network = args.net or "10.0.0.2/24"
    gateway = network_gateway(network)
    cold_marker = f"VIRTNET-PROBE-OK: {gateway}".encode()
    restore_marker = f"NETSNAP-RESTORE-PROBE-OK: {gateway}".encode()
    print(
        "networking + snapshot benchmark, "
        f"median of {args.runs}, {args.network_memory_mib} MiB, 1 vCPU, "
        f"--net {network}"
    )
    print()
    print("== cold boot -> verified gateway connectivity ==")
    probe_cmdline = f"quiet loglevel=0 virtnet_probe={gateway}"
    cleanup_managed_network = (
        args.teardown_mode != "guest-exit" and sys.platform.startswith("linux")
    )
    cold_command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        args.network_memory_mib,
        probe_cmdline,
        command_prefix=command_prefix,
        network=network,
    )
    cold = benchmark(
        cold_command,
        warmups=args.warmups,
        runs=args.runs,
        timeout=max(args.timeout, 40.0),
        marker=cold_marker,
        windows_cpus=windows_cpus,
        teardown_mode=args.teardown_mode,
        cleanup_managed_network=cleanup_managed_network,
    )
    print(
        "  cold  (guest start -> marker):   "
        + format_sample_summary(cold["samples_ms"])
    )
    print(
        "  cold OpenVMM peak RSS          : "
        + format_rss_summary(cold["peak_rss_samples_bytes"])
    )

    print("== capture a warmed, network-configured snapshot (one-off) ==")
    with tempfile.TemporaryDirectory(prefix="openvmm-network-snapshot-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        capture_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            args.network_memory_mib,
            f"{probe_cmdline} netsnap",
            command_prefix=command_prefix,
            network=network,
        )
        capture_automatic_snapshot(
            [
                *capture_command,
                "--snapshot-destination",
                str(snapshot_path),
            ],
            snapshot_path,
            timeout=max(args.timeout, 40.0),
            required_markers=(cold_marker, b"netsnap: pre-snapshot link OK"),
            windows_cpus=windows_cpus,
        )

        print("== restore -> verified gateway connectivity ==")
        restored = benchmark(
            [
                *command_prefix,
                *snapshot_restore_command(
                    executable,
                    backend,
                    snapshot_path,
                    args.unsafe_skip_snapshot_memory_verification,
                ),
            ],
            warmups=args.warmups,
            runs=args.runs,
            timeout=max(args.timeout, 30.0),
            marker=restore_marker,
            windows_cpus=windows_cpus,
            teardown_mode=args.teardown_mode,
            cleanup_managed_network=cleanup_managed_network,
        )
        if restored["teardown_timeout_count"] != 0:
            raise RuntimeError(
                "network restore did not complete every measured teardown"
            )
        wall_samples = restored["wall_samples_ms"]
        print(
            "  restore (guest resume -> marker):  "
            + format_sample_summary(restored["samples_ms"])
        )
        print(
            "  restore OpenVMM peak RSS         : "
            + format_rss_summary(restored["peak_rss_samples_bytes"])
        )
        print("== end-to-end wall-clock (process start -> exit) ==")
        print(
            "  restore wall-clock             : " + format_sample_summary(wall_samples)
        )


class TeeWriter:
    def __init__(self, *streams: TextIO) -> None:
        self.streams = streams

    def write(self, text: str) -> int:
        for stream in self.streams:
            stream.write(text)
        return len(text)

    def flush(self) -> None:
        for stream in self.streams:
            stream.flush()


def run_workload_benchmarks(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> int:
    if args.output is not None:
        raise ValueError("workload suites use --output-dir instead of --output")
    requested = (
        ("cold-start", "virtfs", "shell-snapshot", "network-snapshot")
        if args.suite == "performance"
        else (args.suite,)
    )
    output_dir = args.output_dir
    if output_dir is None and args.suite == "performance":
        platform = f"{'windows' if os.name == 'nt' else 'linux'}-{backend}"
        output_dir = args.nvx_dir.resolve() / "build" / "benchmarks" / platform
    if output_dir is not None:
        output_dir = output_dir.resolve()
        output_dir.mkdir(parents=True, exist_ok=True)
        if args.suite == "performance":
            for filename in (*PERFORMANCE_LOG_FILENAMES, *LEGACY_PYTHON_LOG_FILENAMES):
                (output_dir / filename).unlink(missing_ok=True)

    callbacks: dict[str, tuple[str, Callable[[], None]]] = {
        "cold-start": (
            "cold-start.log",
            lambda: benchmark_cold_start_workload(
                args,
                executable,
                kernel,
                initrd,
                backend,
                command_prefix=command_prefix,
                windows_cpus=windows_cpus,
            ),
        ),
        "virtfs": (
            "virtfs.log",
            lambda: benchmark_virtfs_workload(
                args,
                executable,
                kernel,
                initrd,
                backend,
                runs=args.virtfs_runs if args.suite == "performance" else args.runs,
                command_prefix=command_prefix,
                windows_cpus=windows_cpus,
            ),
        ),
        "shell-snapshot": (
            "shell-snapshot.log",
            lambda: benchmark_shell_snapshot_workload(
                args,
                executable,
                kernel,
                initrd,
                backend,
                command_prefix=command_prefix,
                windows_cpus=windows_cpus,
            ),
        ),
        "network-snapshot": (
            "network.log",
            lambda: benchmark_network_snapshot_workload(
                args,
                executable,
                kernel,
                initrd,
                backend,
                command_prefix=command_prefix,
                windows_cpus=windows_cpus,
            ),
        ),
    }
    for suite in requested:
        filename, callback = callbacks[suite]
        print(f"Benchmarking {suite} on OpenVMM/{backend}", flush=True)
        if output_dir is None:
            callback()
            continue
        path = output_dir / filename
        with path.open("w", encoding="utf-8", newline="\n") as log:
            with contextlib.redirect_stdout(cast(TextIO, TeeWriter(sys.stdout, log))):
                callback()
        print(f"Wrote {path}", flush=True)
    return 0


def capture_snapshot(
    command: Sequence[str],
    snapshot_path: Path,
    *,
    timeout: float,
    windows_cpus: set[int] | None = None,
) -> tuple[float, float, float]:
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off,openvmm_entry::vm_controller=info"
    interaction = InteractiveProcess(command, environment)
    process = interaction.process
    if windows_cpus is not None:
        set_windows_affinity(process.pid, windows_cpus)

    chunks: queue.Queue[bytes | None] = queue.Queue()
    threading.Thread(
        target=interaction.read_output, args=(chunks,), daemon=True
    ).start()
    deadline = time.monotonic() + timeout
    output = bytearray()
    snapshot_requested = False
    snapshot_started_ns = None
    snapshot_published_ns = None

    def observe_snapshot_publication() -> None:
        nonlocal snapshot_published_ns
        if (
            snapshot_requested
            and snapshot_published_ns is None
            and snapshot_path.is_dir()
        ):
            snapshot_published_ns = time.perf_counter_ns()

    try:
        while True:
            observe_snapshot_publication()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"snapshot was not captured within {timeout:g}s")
            try:
                poll_interval = 0.001 if snapshot_requested else 0.25
                chunk = chunks.get(timeout=min(remaining, poll_interval))
            except queue.Empty:
                observe_snapshot_publication()
                if process.poll() is not None and snapshot_published_ns is None:
                    raise RuntimeError(
                        f"OpenVMM exited with status {process.returncode}"
                    ) from None
                continue
            if chunk is None:
                observe_snapshot_publication()
                break
            output.extend(chunk)
            if not snapshot_requested and BOOT_MARKER in output:
                snapshot_started_ns = time.perf_counter_ns()
                interaction.write_input(b"nvx-snapshot; echo " + RESTORE_MARKER + b"\n")
                snapshot_requested = True
                deadline = time.monotonic() + timeout
            if snapshot_requested and contains_output_line(output, RESTORE_MARKER):
                raise RuntimeError("source guest continued past the snapshot boundary")
            if len(output) > 1024 * 1024:
                del output[: len(output) - 1024 * 1024]

        returncode = process.wait()
        source_exited_ns = time.perf_counter_ns()
        if snapshot_published_ns is None and snapshot_path.is_dir():
            snapshot_published_ns = source_exited_ns
        if not snapshot_requested:
            raise RuntimeError("source guest exited before its boot marker")
        if returncode != 0:
            raise RuntimeError(f"snapshot source exited with status {returncode}")
        if not snapshot_path.is_dir():
            raise RuntimeError(f"snapshot was not published at {snapshot_path}")
        assert snapshot_started_ns is not None
        assert snapshot_published_ns is not None
        request_to_publication_ms = (
            snapshot_published_ns - snapshot_started_ns
        ) / 1_000_000
        return (
            request_to_publication_ms,
            request_to_publication_ms,
            (source_exited_ns - snapshot_published_ns) / 1_000_000,
        )
    except Exception as error:
        terminate(process)
        tail = output[-4096:].decode("utf-8", "replace")
        if tail:
            raise RuntimeError(f"{error}\n--- OpenVMM output ---\n{tail}") from error
        raise
    finally:
        interaction.close()


def summarize_snapshot_samples(
    samples: list[float],
    request_to_publication_samples: list[float],
    post_publication_exit_samples: list[float],
) -> SnapshotCaptureResult:
    return {
        "samples_ms": samples,
        "p50_ms": statistics.median(samples),
        "min_ms": min(samples),
        "max_ms": max(samples),
        "request_to_publication_samples_ms": request_to_publication_samples,
        "request_to_publication_p50_ms": statistics.median(
            request_to_publication_samples
        ),
        "request_to_publication_min_ms": min(request_to_publication_samples),
        "request_to_publication_max_ms": max(request_to_publication_samples),
        "post_publication_exit_samples_ms": post_publication_exit_samples,
        "post_publication_exit_p50_ms": statistics.median(
            post_publication_exit_samples
        ),
        "post_publication_exit_min_ms": min(post_publication_exit_samples),
        "post_publication_exit_max_ms": max(post_publication_exit_samples),
    }


def benchmark_snapshot_capture(
    args: argparse.Namespace,
    boot_command: Sequence[str],
    *,
    windows_cpus: set[int] | None = None,
) -> SnapshotCaptureResult:
    samples: list[float] = []
    request_to_publication_samples: list[float] = []
    post_publication_exit_samples: list[float] = []
    for index in range(args.warmups + args.runs):
        with tempfile.TemporaryDirectory(
            prefix="openvmm-snapshot-capture-"
        ) as temp_dir:
            snapshot_path = Path(temp_dir) / "snapshot"
            value, request_to_publication, post_publication_exit = capture_snapshot(
                [*boot_command, "--snapshot-destination", str(snapshot_path)],
                snapshot_path,
                timeout=args.timeout,
                windows_cpus=windows_cpus,
            )
        if index < args.warmups:
            print(f"  warmup {index + 1}/{args.warmups}: {value:.3f} ms", flush=True)
        else:
            samples.append(value)
            request_to_publication_samples.append(request_to_publication)
            post_publication_exit_samples.append(post_publication_exit)
            print(
                f"  sample {index - args.warmups + 1}/{args.runs}: {value:.3f} ms, "
                f"request-to-publication={request_to_publication:.3f} ms, "
                f"post-publication exit={post_publication_exit:.3f} ms",
                flush=True,
            )
    return summarize_snapshot_samples(
        samples,
        request_to_publication_samples,
        post_publication_exit_samples,
    )


def print_snapshot_summary(backend: str, result: SnapshotCaptureResult) -> None:
    print(
        f"snapshot/{backend}: p50={result['p50_ms']:.3f} ms "
        f"min={result['min_ms']:.3f} ms max={result['max_ms']:.3f} ms "
        f"post-publication-exit-p50="
        f"{result['post_publication_exit_p50_ms']:.3f} ms",
        flush=True,
    )


def snapshot_restore_command(
    executable: Path,
    hypervisor: str,
    snapshot_path: Path,
    unsafe_skip_memory_verification: bool,
) -> list[str]:
    command = [
        str(executable),
        "--single-process",
        "--machine",
        "microvm",
        "--hypervisor",
        hypervisor,
        "--restore-snapshot",
        str(snapshot_path),
        "--restore-entropy",
    ]
    if unsafe_skip_memory_verification:
        command.append("--unsafe-skip-snapshot-memory-verification")
    return command


def benchmark_snapshot_restore(
    args: argparse.Namespace,
    executable: Path,
    hypervisor: str,
    boot_command: Sequence[str],
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> BenchmarkResult:
    with tempfile.TemporaryDirectory(prefix="openvmm-e2e-restore-") as temp_dir:
        snapshot_path = Path(temp_dir) / "snapshot"
        capture_snapshot(
            [*boot_command, "--snapshot-destination", str(snapshot_path)],
            snapshot_path,
            timeout=args.timeout,
            windows_cpus=windows_cpus,
        )
        return benchmark(
            [
                *command_prefix,
                *snapshot_restore_command(
                    executable,
                    hypervisor,
                    snapshot_path,
                    args.unsafe_skip_snapshot_memory_verification,
                ),
            ],
            warmups=args.warmups,
            runs=args.runs,
            timeout=args.timeout,
            marker=RESTORE_MARKER,
            windows_cpus=windows_cpus,
            teardown_mode=args.teardown_mode,
        )


def print_summary(backend: str, result: BenchmarkResult) -> None:
    teardown_p50 = result["teardown_p50_ms"]
    teardown_max = result["teardown_max_ms"]
    teardown = (
        f"teardown-p50={teardown_p50:.3f} ms teardown-max={teardown_max:.3f} ms"
        if teardown_p50 is not None and teardown_max is not None
        else "teardown=no-completed-samples"
    )
    print(
        f"{backend}: p50={result['p50_ms']:.3f} ms "
        f"min={result['min_ms']:.3f} ms max={result['max_ms']:.3f} ms "
        f"peak-rss-p50={bytes_to_mib(result['peak_rss_p50_bytes']):.3f} MiB "
        f"peak-rss-max={bytes_to_mib(result['peak_rss_max_bytes']):.3f} MiB "
        f"{teardown} teardown-timeouts={result['teardown_timeout_count']}",
        flush=True,
    )


def print_phase2_summary(backend: str, result: Phase2Result) -> None:
    memory_mib = int(result["memory_mib"])
    metrics = result["metrics"]
    print(f"phase2/{backend} ({memory_mib} MiB RAM):", flush=True)
    for name in (
        "restore_prepare",
        "repeat_restore_prepare",
        "new_process_restore_prepare",
    ):
        metric = metrics[name]
        print(f"  {name}: p50={float(metric['p50_ms']):.3f} ms", flush=True)
    for name in ("snapshot_publish", "snapshot_verify", "repeat_verify"):
        metric = metrics[name]
        p50_ms = float(metric["p50_ms"])
        throughput = memory_mib / (p50_ms / 1000.0)
        print(
            f"  {name}: p50={p50_ms:.3f} ms ({throughput:.1f} MiB/s)",
            flush=True,
        )
    for name in ("cow_map", "repeat_cow_map", "cow_dirty_all"):
        metric = metrics[name]
        p50_ms = float(metric["p50_ms"])
        print(f"  {name}: p50={p50_ms:.3f} ms", flush=True)


def compare_cold_start_to_restore_prepare(
    cold_start: BenchmarkResult,
    phase2: Phase2Result,
) -> ColdRestoreComparison:
    metrics = phase2["metrics"]
    cold_start_ms = float(cold_start["p50_ms"])
    restore_prepare_ms = float(metrics["restore_prepare"]["p50_ms"])
    repeat_restore_prepare_ms = float(metrics["repeat_restore_prepare"]["p50_ms"])
    new_process_restore_prepare_ms = float(
        metrics["new_process_restore_prepare"]["p50_ms"]
    )
    return {
        "scope": "new_process_host_restore_preparation_lower_bound",
        "includes": [
            "new process launch",
            "bounded manifest and state read",
            "artifact length and SHA-256 verification",
            "writable private COW mapping",
            "artifact probes",
        ],
        "excludes": [
            "saved-state decode",
            "partition, device, interrupt, clock, and vCPU reconstruction",
            "guest resume to readiness marker",
        ],
        "cold_start_p50_ms": cold_start_ms,
        "restore_prepare_p50_ms": restore_prepare_ms,
        "repeat_restore_prepare_p50_ms": repeat_restore_prepare_ms,
        "new_process_restore_prepare_p50_ms": new_process_restore_prepare_ms,
        "cold_start_over_new_process_restore_prepare": (
            cold_start_ms / new_process_restore_prepare_ms
        ),
        "new_process_restore_prepare_savings_percent": (
            (1.0 - new_process_restore_prepare_ms / cold_start_ms) * 100.0
        ),
    }


def compare_cold_start_to_snapshot_restore(
    cold_start: BenchmarkResult,
    snapshot_restore: BenchmarkResult,
) -> E2EComparison:
    cold_start_ms = float(cold_start["p50_ms"])
    snapshot_restore_ms = float(snapshot_restore["p50_ms"])
    return {
        "scope": "new_process_to_guest_continuation_marker",
        "cold_start_p50_ms": cold_start_ms,
        "snapshot_restore_p50_ms": snapshot_restore_ms,
        "cold_start_over_snapshot_restore": cold_start_ms / snapshot_restore_ms,
        "snapshot_restore_savings_percent": (
            (1.0 - snapshot_restore_ms / cold_start_ms) * 100.0
        ),
    }


def print_cold_restore_comparison(
    backend: str,
    comparison: ColdRestoreComparison,
) -> None:
    print(
        f"comparison/{backend} (new-process host-only lower bound): "
        f"cold-start={comparison['cold_start_p50_ms']:.3f} ms "
        f"restore-prep={comparison['new_process_restore_prepare_p50_ms']:.3f} ms "
        f"cold/restore-prep={comparison['cold_start_over_new_process_restore_prepare']:.2f}x",
        flush=True,
    )


def print_e2e_comparison(backend: str, comparison: E2EComparison) -> None:
    print(
        f"comparison/{backend} (new process to guest marker): "
        f"cold-start={comparison['cold_start_p50_ms']:.3f} ms "
        f"snapshot-restore={comparison['snapshot_restore_p50_ms']:.3f} ms "
        f"cold/restore={comparison['cold_start_over_snapshot_restore']:.2f}x",
        flush=True,
    )


def windows_to_wsl(path: Path) -> str:
    completed = subprocess.run(
        ["wsl.exe", "--exec", "wslpath", "-a", "-u", str(path.resolve())],
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.strip()


def build_whp(openvmm_dir: Path) -> Path:
    run_checked(
        ["cargo", "build", "--release", "-p", "openvmm", "--bin", "openvmm"],
        cwd=openvmm_dir,
    )
    return require_file(
        openvmm_dir / "target" / "release" / "openvmm.exe",
        "native OpenVMM release binary",
    )


def build_kvm(openvmm_dir: Path) -> Path:
    openvmm_wsl = windows_to_wsl(openvmm_dir)
    run_checked(
        [
            "wsl.exe",
            "--cd",
            openvmm_wsl,
            "--exec",
            "sh",
            "-lc",
            "CARGO_TARGET_DIR=target/bench-linux PROTOC=/usr/bin/protoc "
            "cargo build --release -p openvmm --bin openvmm",
        ]
    )
    return require_file(
        openvmm_dir / "target" / "bench-linux" / "release" / "openvmm",
        "Linux OpenVMM release binary",
    )


def build_kvm_native(openvmm_dir: Path) -> Path:
    run_checked(
        [
            "cargo",
            "build",
            "--release",
            "-p",
            "openvmm",
            "--bin",
            "openvmm",
        ],
        cwd=openvmm_dir,
    )
    return require_file(
        openvmm_dir / "target" / "release" / "openvmm",
        "Linux OpenVMM release binary",
    )


def build_phase2_whp_host(openvmm_dir: Path) -> Path:
    run_checked(
        [
            "cargo",
            "build",
            "--release",
            "-p",
            "openvmm_helpers",
            "--example",
            "phase2_snapshot_bench",
        ],
        cwd=openvmm_dir,
    )
    return require_file(
        openvmm_dir / "target" / "release" / "examples" / "phase2_snapshot_bench.exe",
        "native phase 2 benchmark executable",
    )


def build_phase2_kvm_host(openvmm_dir: Path) -> Path:
    openvmm_wsl = windows_to_wsl(openvmm_dir)
    run_checked(
        [
            "wsl.exe",
            "--cd",
            openvmm_wsl,
            "--exec",
            "sh",
            "-lc",
            "CARGO_TARGET_DIR=target/bench-linux "
            "PROTOC=/usr/bin/protoc "
            "cargo build --release -p openvmm_helpers "
            "--example phase2_snapshot_bench",
        ]
    )
    return require_file(
        openvmm_dir
        / "target"
        / "bench-linux"
        / "release"
        / "examples"
        / "phase2_snapshot_bench",
        "Linux phase 2 benchmark executable",
    )


def whp_command(
    executable: Path,
    kernel: Path,
    initrd: Path,
    memory_mib: int,
    network: str | None,
) -> list[str]:
    command = [
        str(executable),
        "--single-process",
        "--machine",
        "microvm",
        "--hypervisor",
        "whp",
        "--memory",
        f"{memory_mib}M",
        "--kernel",
        str(kernel),
        "--initrd",
        str(initrd),
        "--cmdline",
        BASE_TUNING,
    ]
    if network is not None:
        command.extend(("--net", network))
    return command


def stage_kvm(
    executable: Path,
    kernel: Path,
    initrd: Path,
    stage_dir: str,
) -> None:
    executable_wsl = windows_to_wsl(executable)
    kernel_wsl = windows_to_wsl(kernel)
    initrd_wsl = windows_to_wsl(initrd)
    quoted_stage = shlex.quote(stage_dir)
    script = " && ".join(
        [
            f"rm -rf {quoted_stage}",
            f"mkdir -p {quoted_stage}",
            f"cp {shlex.quote(executable_wsl)} {quoted_stage}/openvmm",
            f"cp {shlex.quote(kernel_wsl)} {quoted_stage}/vmlinux",
            f"cp {shlex.quote(initrd_wsl)} {quoted_stage}/initramfs.cpio.gz",
            f"strip --strip-debug {quoted_stage}/openvmm",
        ]
    )
    run_checked(["wsl.exe", "--exec", "sh", "-lc", script])


def cleanup_kvm(stage_dir: str) -> None:
    run_checked(["wsl.exe", "--exec", "rm", "-rf", stage_dir])


def run_phase2_benchmark(
    command: Sequence[str],
    *,
    windows_cpus: set[int] | None = None,
) -> Phase2Result:
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        env=environment,
    )
    if windows_cpus is not None:
        set_windows_affinity(process.pid, windows_cpus)

    assert process.stdout is not None
    output: list[str] = []
    for line in process.stdout:
        output.append(line)
        print(line, end="", flush=True)
    returncode = process.wait()
    if returncode != 0:
        raise subprocess.CalledProcessError(
            returncode,
            list(command),
            output="".join(output),
        )
    for line in output:
        if line.startswith(PHASE2_RESULT_PREFIX):
            result = cast(
                Phase2Result,
                json.loads(line.removeprefix(PHASE2_RESULT_PREFIX)),
            )
            if not result.get("artifact_unchanged"):
                raise RuntimeError(
                    "phase 2 benchmark did not preserve the snapshot artifact"
                )
            return result
    raise RuntimeError("phase 2 benchmark did not emit a result")


def phase2_arguments(args: argparse.Namespace) -> list[str]:
    return [
        "--warmups",
        str(args.warmups),
        "--runs",
        str(args.runs),
        "--memory-mib",
        str(args.memory_mib),
    ]


def benchmark_phase2_kvm(
    args: argparse.Namespace,
    executable: Path,
) -> Phase2Result:
    stage_dir = "/tmp/openvmm-phase2-benchmark"
    executable_wsl = windows_to_wsl(executable)
    quoted_stage = shlex.quote(stage_dir)
    script = " && ".join(
        [
            f"rm -rf {quoted_stage}",
            f"mkdir -p {quoted_stage}",
            f"cp {shlex.quote(executable_wsl)} {quoted_stage}/phase2_snapshot_bench",
            f"strip --strip-debug {quoted_stage}/phase2_snapshot_bench",
        ]
    )
    run_checked(["wsl.exe", "--exec", "sh", "-lc", script])
    command = [
        "wsl.exe",
        "--exec",
        "taskset",
        "-c",
        args.cpus,
        f"{stage_dir}/phase2_snapshot_bench",
        *phase2_arguments(args),
    ]
    try:
        return run_phase2_benchmark(command)
    finally:
        if not args.keep_kvm_stage:
            cleanup_kvm(stage_dir)


def run_kvm_worker(args: argparse.Namespace) -> int:
    stage = Path(args._stage_dir)
    boot_command = [
        "taskset",
        "-c",
        args.cpus,
        str(stage / "openvmm"),
        "--single-process",
        "--machine",
        "microvm",
        "--hypervisor",
        "kvm",
        "--memory",
        f"{args.memory_mib}M",
        "--kernel",
        str(stage / "vmlinux"),
        "--initrd",
        str(stage / "initramfs.cpio.gz"),
        "--cmdline",
        f"clocksource=kvm-clock {BASE_TUNING}",
    ]
    if args.net is not None:
        boot_command.extend(("--net", args.net))
    if args.suite == "snapshot":
        print("Benchmarking OpenVMM/KVM snapshot capture", flush=True)
        result = benchmark_snapshot_capture(args, boot_command)
        print_snapshot_summary("kvm", result)
        print(
            KVM_SNAPSHOT_RESULT_PREFIX + json.dumps(result, separators=(",", ":")),
            flush=True,
        )
        return 0
    if args.suite == "restore":
        print("Benchmarking OpenVMM/KVM E2E snapshot restore", flush=True)
        result = benchmark_snapshot_restore(
            args,
            stage / "openvmm",
            "kvm",
            boot_command,
            command_prefix=("taskset", "-c", args.cpus),
        )
        print_summary("snapshot-restore/kvm", result)
        print(
            KVM_RESTORE_RESULT_PREFIX + json.dumps(result, separators=(",", ":")),
            flush=True,
        )
        return 0
    if args.suite != "boot":
        raise ValueError(f"unsupported KVM worker suite: {args.suite}")
    print("Benchmarking OpenVMM/KVM", flush=True)
    result = benchmark(
        boot_command,
        warmups=args.warmups,
        runs=args.runs,
        timeout=args.timeout,
        teardown_mode=args.teardown_mode,
    )
    print_summary("kvm", result)
    print(KVM_RESULT_PREFIX + json.dumps(result, separators=(",", ":")), flush=True)
    return 0


def result_document(
    args: argparse.Namespace,
    kernel: Path | None,
    initrd: Path | None,
) -> ResultDocument:
    return {
        "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "controls": {
            "suite": args.suite,
            "warmups": args.warmups,
            "runs": args.runs,
            "memory_mib": args.memory_mib,
            "network": args.net,
            "restore_memory_sha256": (
                not args.unsafe_skip_snapshot_memory_verification
            ),
            "cold_start_scope": (
                "OpenVMM process launch through guest readiness marker; "
                "teardown excluded and recorded separately"
            ),
            "snapshot_restore_scope": (
                "restored OpenVMM process launch through restored guest marker; "
                "teardown excluded and recorded separately"
            ),
            "peak_rss_scope": "OpenVMM process launch through guest marker",
            "snapshot_capture_scope": (
                "guest nvx-snapshot command dispatch through first host "
                "observation of atomic snapshot publication"
            ),
            "snapshot_capture_source": (
                "host perf_counter around guest dispatch and snapshot "
                "directory publication"
            ),
            "snapshot_publication_poll_interval_ms": 1.0,
            "snapshot_request_to_publication_scope": (
                "guest nvx-snapshot command dispatch through first observation "
                "of atomic snapshot publication"
            ),
            "snapshot_post_publication_exit_scope": (
                "first observation of atomic snapshot publication through "
                "successful source OpenVMM process exit"
            ),
            "teardown_scope": (
                "host process termination request through OpenVMM process exit; "
                "TerminateProcess on WHP and SIGTERM on Linux"
                if args.teardown_mode != "guest-exit"
                else "host dispatch of nvx-exit 0 at guest readiness through "
                "successful OpenVMM process exit"
            ),
            "teardown_mode": args.teardown_mode,
            "teardown_timeout_seconds": TEARDOWN_TIMEOUT_SECONDS,
            "cpus": args.cpus,
            "timeout_seconds": args.timeout,
            "kernel": str(kernel) if kernel is not None else None,
            "initrd": str(initrd) if initrd is not None else None,
            "marker": BOOT_MARKER.decode(),
            "restore_marker": RESTORE_MARKER.decode(),
        },
        "backends": {},
        "snapshot_capture": {},
        "snapshot_restore": {},
        "phase2": {},
        "comparison": {},
        "e2e_comparison": {},
    }


def run_native_linux(args: argparse.Namespace) -> int:
    if args.backend not in ("kvm", "mshv"):
        raise ValueError("native Linux benchmark runs support --backend kvm or mshv")

    backend = args.backend

    openvmm_dir = args.openvmm_dir.resolve()
    require_file(openvmm_dir / "Cargo.toml", "OpenVMM Cargo.toml")
    run_boot = args.suite in ("boot", "e2e", "all")
    run_snapshot = args.suite in ("snapshot", "all")
    run_restore = args.suite in ("restore", "e2e", "all")
    run_phase2 = args.suite in ("phase2", "all")
    run_workloads = args.suite in WORKLOAD_SUITES
    run_guest = run_boot or run_snapshot or run_restore or run_workloads
    kernel = None
    initrd = None
    executable = None
    if run_guest:
        artifact_dir = args.nvx_dir.resolve() / "build"
        kernel = require_file(artifact_dir / "vmlinux", "NVX PVH kernel")
        initrd = require_file(
            artifact_dir / "initramfs.cpio.gz",
            "NVX initramfs",
        )
        executable = (
            require_file(
                openvmm_dir / "target" / "release" / "openvmm",
                "Linux OpenVMM release binary",
            )
            if args.skip_build
            else build_kvm_native(openvmm_dir)
        )

    phase2_executable = None
    if run_phase2:
        if args.skip_build:
            phase2_executable = require_file(
                openvmm_dir
                / "target"
                / "release"
                / "examples"
                / "phase2_snapshot_bench",
                "Linux phase 2 benchmark executable",
            )
        else:
            run_checked(
                [
                    "cargo",
                    "build",
                    "--release",
                    "-p",
                    "openvmm_helpers",
                    "--example",
                    "phase2_snapshot_bench",
                ],
                cwd=openvmm_dir,
            )
            phase2_executable = require_file(
                openvmm_dir
                / "target"
                / "release"
                / "examples"
                / "phase2_snapshot_bench",
                "Linux phase 2 benchmark executable",
            )

    cpus = parse_cpu_set(args.cpus)
    available_cpus = os.cpu_count() or 1
    if max(cpus) >= available_cpus:
        raise ValueError(
            f"CPU set {args.cpus!r} exceeds the {available_cpus} available logical CPUs"
        )
    prefix = ["taskset", "-c", args.cpus]
    if run_workloads:
        assert executable is not None and kernel is not None and initrd is not None
        return run_workload_benchmarks(
            args,
            executable,
            kernel,
            initrd,
            backend,
            command_prefix=prefix,
        )
    results = result_document(args, kernel, initrd)
    if run_guest:
        assert executable is not None and kernel is not None and initrd is not None
        boot_command = [
            *prefix,
            str(executable),
            "--single-process",
            "--machine",
            "microvm",
            "--hypervisor",
            backend,
            "--memory",
            f"{args.memory_mib}M",
            "--kernel",
            str(kernel),
            "--initrd",
            str(initrd),
            "--cmdline",
            f"{'clocksource=kvm-clock ' if backend == 'kvm' else ''}{BASE_TUNING}",
        ]
        if args.net is not None:
            boot_command.extend(("--net", args.net))
        if run_boot:
            result = benchmark(
                boot_command,
                warmups=args.warmups,
                runs=args.runs,
                timeout=args.timeout,
                teardown_mode=args.teardown_mode,
            )
            results["backends"][backend] = result
            print_summary(backend, result)
        if run_snapshot:
            result = benchmark_snapshot_capture(args, boot_command)
            results["snapshot_capture"][backend] = result
            print_snapshot_summary(backend, result)
        if run_restore:
            result = benchmark_snapshot_restore(
                args,
                executable,
                backend,
                boot_command,
                command_prefix=prefix,
            )
            results["snapshot_restore"][backend] = result
            print_summary(f"snapshot-restore/{backend}", result)
    if run_phase2:
        assert phase2_executable is not None
        result = run_phase2_benchmark(
            [*prefix, str(phase2_executable), *phase2_arguments(args)]
        )
        results["phase2"][backend] = result
        print_phase2_summary(backend, result)
    if run_boot and run_phase2:
        comparison = compare_cold_start_to_restore_prepare(
            results["backends"][backend],
            results["phase2"][backend],
        )
        results["comparison"][backend] = comparison
        print_cold_restore_comparison(backend, comparison)
    if run_boot and run_restore:
        comparison = compare_cold_start_to_snapshot_restore(
            results["backends"][backend],
            results["snapshot_restore"][backend],
        )
        results["e2e_comparison"][backend] = comparison
        print_e2e_comparison(backend, comparison)
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
        print(f"Wrote {args.output}", flush=True)
    return 0


def benchmark_kvm(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
) -> BenchmarkResult:
    stage_dir = "/tmp/openvmm-microvm-benchmark"
    stage_kvm(executable, kernel, initrd, stage_dir)
    script_wsl = windows_to_wsl(NVX_SCRIPT)
    command = [
        "wsl.exe",
        "--exec",
        "python3",
        script_wsl,
        "benchmark",
        "--_kvm-worker",
        "--_stage-dir",
        stage_dir,
        "--suite",
        "boot",
        "--warmups",
        str(args.warmups),
        "--runs",
        str(args.runs),
        "--memory-mib",
        str(args.memory_mib),
        "--cpus",
        args.cpus,
        "--timeout",
        str(args.timeout),
        "--teardown-mode",
        args.teardown_mode,
    ]
    if args.net is not None:
        command.extend(("--net", args.net))
    try:
        completed = subprocess.run(command, check=True, capture_output=True, text=True)
        print(completed.stdout, end="")
        for line in completed.stdout.splitlines():
            if line.startswith(KVM_RESULT_PREFIX):
                return cast(
                    BenchmarkResult,
                    json.loads(line.removeprefix(KVM_RESULT_PREFIX)),
                )
        raise RuntimeError("KVM worker did not emit a result")
    except subprocess.CalledProcessError as error:
        if error.stdout:
            print(error.stdout, end="", file=sys.stderr)
        if error.stderr:
            print(error.stderr, end="", file=sys.stderr)
        raise
    finally:
        if not args.keep_kvm_stage:
            cleanup_kvm(stage_dir)


def benchmark_snapshot_restore_kvm(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
) -> BenchmarkResult:
    stage_dir = "/tmp/openvmm-microvm-benchmark"
    stage_kvm(executable, kernel, initrd, stage_dir)
    script_wsl = windows_to_wsl(NVX_SCRIPT)
    command = [
        "wsl.exe",
        "--exec",
        "python3",
        script_wsl,
        "benchmark",
        "--_kvm-worker",
        "--_stage-dir",
        stage_dir,
        "--suite",
        "restore",
        "--warmups",
        str(args.warmups),
        "--runs",
        str(args.runs),
        "--memory-mib",
        str(args.memory_mib),
        "--cpus",
        args.cpus,
        "--timeout",
        str(args.timeout),
        "--teardown-mode",
        args.teardown_mode,
    ]
    if args.net is not None:
        command.extend(("--net", args.net))
    if args.unsafe_skip_snapshot_memory_verification:
        command.append("--unsafe-skip-snapshot-memory-verification")
    try:
        completed = subprocess.run(command, check=True, capture_output=True, text=True)
        print(completed.stdout, end="")
        for line in completed.stdout.splitlines():
            if line.startswith(KVM_RESTORE_RESULT_PREFIX):
                return cast(
                    BenchmarkResult,
                    json.loads(line.removeprefix(KVM_RESTORE_RESULT_PREFIX)),
                )
        raise RuntimeError("KVM restore worker did not emit a result")
    except subprocess.CalledProcessError as error:
        if error.stdout:
            print(error.stdout, end="", file=sys.stderr)
        if error.stderr:
            print(error.stderr, end="", file=sys.stderr)
        raise
    finally:
        if not args.keep_kvm_stage:
            cleanup_kvm(stage_dir)


def benchmark_snapshot_kvm(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
) -> SnapshotCaptureResult:
    stage_dir = "/tmp/openvmm-microvm-benchmark"
    stage_kvm(executable, kernel, initrd, stage_dir)
    script_wsl = windows_to_wsl(NVX_SCRIPT)
    command = [
        "wsl.exe",
        "--exec",
        "python3",
        script_wsl,
        "benchmark",
        "--_kvm-worker",
        "--_stage-dir",
        stage_dir,
        "--suite",
        "snapshot",
        "--warmups",
        str(args.warmups),
        "--runs",
        str(args.runs),
        "--memory-mib",
        str(args.memory_mib),
        "--cpus",
        args.cpus,
        "--timeout",
        str(args.timeout),
    ]
    if args.net is not None:
        command.extend(("--net", args.net))
    try:
        completed = subprocess.run(command, check=True, capture_output=True, text=True)
        print(completed.stdout, end="")
        for line in completed.stdout.splitlines():
            if line.startswith(KVM_SNAPSHOT_RESULT_PREFIX):
                return cast(
                    SnapshotCaptureResult,
                    json.loads(line.removeprefix(KVM_SNAPSHOT_RESULT_PREFIX)),
                )
        raise RuntimeError("KVM snapshot worker did not emit a result")
    except subprocess.CalledProcessError as error:
        if error.stdout:
            print(error.stdout, end="", file=sys.stderr)
        if error.stderr:
            print(error.stderr, end="", file=sys.stderr)
        raise
    finally:
        if not args.keep_kvm_stage:
            cleanup_kvm(stage_dir)


def run(args: argparse.Namespace) -> int:
    if args._kvm_worker:
        return run_kvm_worker(args)
    if os.name != "nt":
        if sys.platform.startswith("linux"):
            return run_native_linux(args)
        raise RuntimeError(f"benchmark coordinator is unsupported on {sys.platform}")

    openvmm_dir = args.openvmm_dir.resolve()
    require_file(openvmm_dir / "Cargo.toml", "OpenVMM Cargo.toml")
    run_boot = args.suite in ("boot", "e2e", "all")
    run_snapshot = args.suite in ("snapshot", "all")
    run_restore = args.suite in ("restore", "e2e", "all")
    run_phase2 = args.suite in ("phase2", "all")
    run_workloads = args.suite in WORKLOAD_SUITES
    if run_workloads and args.backend != "whp":
        raise ValueError(
            "Windows workload suites require --backend whp; run KVM/MSHV "
            "workloads on native Linux"
        )
    run_guest = run_boot or run_snapshot or run_restore or run_workloads
    kernel = None
    initrd = None
    if run_guest:
        nvx_dir = args.nvx_dir.resolve()
        kernel = require_file(nvx_dir / "build" / "vmlinux", "NVX PVH kernel")
        initrd = require_file(
            nvx_dir / "build" / "initramfs.cpio.gz",
            "NVX initramfs",
        )
    cpus = parse_cpu_set(args.cpus)
    available_cpus = os.cpu_count() or 1
    if max(cpus) >= available_cpus:
        raise ValueError(
            f"CPU set {args.cpus!r} exceeds the {available_cpus} available logical CPUs"
        )

    selected = ("whp", "kvm") if args.backend == "both" else (args.backend,)
    boot_binaries: dict[str, Path] = {}
    phase2_binaries: dict[str, Path] = {}
    if args.skip_build:
        if run_guest and "whp" in selected:
            boot_binaries["whp"] = require_file(
                openvmm_dir / "target" / "release" / "openvmm.exe",
                "native OpenVMM release binary",
            )
        if run_guest and "kvm" in selected:
            boot_binaries["kvm"] = require_file(
                openvmm_dir / "target" / "bench-linux" / "release" / "openvmm",
                "Linux OpenVMM release binary",
            )
        if run_phase2 and "whp" in selected:
            phase2_binaries["whp"] = require_file(
                openvmm_dir
                / "target"
                / "release"
                / "examples"
                / "phase2_snapshot_bench.exe",
                "native phase 2 benchmark executable",
            )
        if run_phase2 and "kvm" in selected:
            phase2_binaries["kvm"] = require_file(
                openvmm_dir
                / "target"
                / "bench-linux"
                / "release"
                / "examples"
                / "phase2_snapshot_bench",
                "Linux phase 2 benchmark executable",
            )
    else:
        if run_guest and "whp" in selected:
            boot_binaries["whp"] = build_whp(openvmm_dir)
        if run_guest and "kvm" in selected:
            boot_binaries["kvm"] = build_kvm(openvmm_dir)
        if run_phase2 and "whp" in selected:
            phase2_binaries["whp"] = build_phase2_whp_host(openvmm_dir)
        if run_phase2 and "kvm" in selected:
            phase2_binaries["kvm"] = build_phase2_kvm_host(openvmm_dir)

    if run_workloads:
        assert kernel is not None and initrd is not None
        return run_workload_benchmarks(
            args,
            boot_binaries["whp"],
            kernel,
            initrd,
            "whp",
            windows_cpus=cpus,
        )

    results = result_document(args, kernel, initrd)

    backends = results["backends"]
    assert isinstance(backends, dict)
    if run_boot and "whp" in selected:
        assert kernel is not None and initrd is not None
        print("Benchmarking OpenVMM/WHP", flush=True)
        whp_result = benchmark(
            whp_command(
                boot_binaries["whp"],
                kernel,
                initrd,
                args.memory_mib,
                args.net,
            ),
            warmups=args.warmups,
            runs=args.runs,
            timeout=args.timeout,
            windows_cpus=cpus,
            teardown_mode=args.teardown_mode,
        )
        backends["whp"] = whp_result
        print_summary("whp", whp_result)

    if run_boot and "kvm" in selected:
        assert kernel is not None and initrd is not None
        backends["kvm"] = benchmark_kvm(
            args,
            boot_binaries["kvm"],
            kernel,
            initrd,
        )

    snapshot_capture_results = results["snapshot_capture"]
    assert isinstance(snapshot_capture_results, dict)
    if run_snapshot and "whp" in selected:
        assert kernel is not None and initrd is not None
        print("Benchmarking OpenVMM/WHP snapshot capture", flush=True)
        whp_snapshot = benchmark_snapshot_capture(
            args,
            whp_command(
                boot_binaries["whp"],
                kernel,
                initrd,
                args.memory_mib,
                args.net,
            ),
            windows_cpus=cpus,
        )
        snapshot_capture_results["whp"] = whp_snapshot
        print_snapshot_summary("whp", whp_snapshot)

    if run_snapshot and "kvm" in selected:
        assert kernel is not None and initrd is not None
        snapshot_capture_results["kvm"] = benchmark_snapshot_kvm(
            args,
            boot_binaries["kvm"],
            kernel,
            initrd,
        )

    snapshot_restore_results = results["snapshot_restore"]
    assert isinstance(snapshot_restore_results, dict)
    if run_restore and "whp" in selected:
        assert kernel is not None and initrd is not None
        print("Benchmarking OpenVMM/WHP E2E snapshot restore", flush=True)
        whp_restore = benchmark_snapshot_restore(
            args,
            boot_binaries["whp"],
            "whp",
            whp_command(
                boot_binaries["whp"],
                kernel,
                initrd,
                args.memory_mib,
                args.net,
            ),
            windows_cpus=cpus,
        )
        snapshot_restore_results["whp"] = whp_restore
        print_summary("snapshot-restore/whp", whp_restore)

    if run_restore and "kvm" in selected:
        assert kernel is not None and initrd is not None
        snapshot_restore_results["kvm"] = benchmark_snapshot_restore_kvm(
            args,
            boot_binaries["kvm"],
            kernel,
            initrd,
        )

    phase2_results = results["phase2"]
    assert isinstance(phase2_results, dict)
    if run_phase2 and "whp" in selected:
        print("Benchmarking phase 2 foundations on Windows/WHP host", flush=True)
        whp_phase2 = run_phase2_benchmark(
            [str(phase2_binaries["whp"]), *phase2_arguments(args)],
            windows_cpus=cpus,
        )
        phase2_results["whp"] = whp_phase2
        print_phase2_summary("whp", whp_phase2)

    if run_phase2 and "kvm" in selected:
        print("Benchmarking phase 2 foundations on Linux/KVM host", flush=True)
        kvm_phase2 = benchmark_phase2_kvm(args, phase2_binaries["kvm"])
        phase2_results["kvm"] = kvm_phase2
        print_phase2_summary("kvm", kvm_phase2)

    comparisons = results["comparison"]
    assert isinstance(comparisons, dict)
    if run_boot and run_phase2:
        for backend in selected:
            comparison = compare_cold_start_to_restore_prepare(
                backends[backend],
                phase2_results[backend],
            )
            comparisons[backend] = comparison
            print_cold_restore_comparison(backend, comparison)

    e2e_comparisons = results["e2e_comparison"]
    assert isinstance(e2e_comparisons, dict)
    if run_boot and run_restore:
        for backend in selected:
            comparison = compare_cold_start_to_snapshot_restore(
                backends[backend],
                snapshot_restore_results[backend],
            )
            e2e_comparisons[backend] = comparison
            print_e2e_comparison(backend, comparison)

    if args.output:
        output = args.output.resolve()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
        print(f"Wrote {output}")
    return 0
