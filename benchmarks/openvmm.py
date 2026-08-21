#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Build and benchmark the OpenVMM microVM entrypoint on WHP, KVM, and MSHV."""

from __future__ import annotations

import argparse
import ctypes
import datetime as dt
import errno
import json
import os
from pathlib import Path
import queue
import select
import shlex
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from typing import Sequence


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


def parse_args() -> argparse.Namespace:
    script_dir = Path(__file__).resolve().parent
    repository_dir = script_dir.parent
    cpu_count = os.cpu_count() or 1
    cpu_start = max(0, cpu_count - min(cpu_count, 4))
    default_cpus = (
        str(cpu_start)
        if cpu_start == cpu_count - 1
        else f"{cpu_start}-{cpu_count - 1}"
    )
    parser = argparse.ArgumentParser(
        description=(
            "Build and benchmark OpenVMM microVM boot or the host-side phase 2 "
            "snapshot foundations. No NVX VMM binary is built or run."
        )
    )
    parser.add_argument(
        "--suite",
        choices=("boot", "snapshot", "restore", "e2e", "phase2", "all"),
        default="boot",
        help=(
            "benchmark suite to run: e2e measures cold boot and full "
            "snapshot restore; phase2 measures host foundations (default: boot)"
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
        "--keep-kvm-stage",
        action="store_true",
        help="keep temporary staged KVM benchmark binaries",
    )

    # The Windows coordinator reinvokes this file inside WSL for KVM so the
    # timed process does not include one wsl.exe launch per sample.
    parser.add_argument("--_kvm-worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument(
        "--_stage-dir",
        default="/tmp/openvmm-microvm-benchmark",
        help=argparse.SUPPRESS,
    )
    return parser.parse_args()


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
    if sys.platform.startswith("linux") and hasattr(os, "pidfd_open"):
        pidfd = os.pidfd_open(process.pid)
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
            terminal_fd, child_fd = os.openpty()
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
                while chunk := self.process.stdout.read1(4096):
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


def measure_once(
    command: Sequence[str],
    *,
    environment: dict[str, str],
    timeout: float,
    marker: bytes = BOOT_MARKER,
    windows_cpus: set[int] | None = None,
    teardown_mode: str = "guest-exit",
) -> tuple[float, int, float | None]:
    started = time.perf_counter_ns()
    interaction = InteractiveProcess(command, environment)
    process = interaction.process
    if windows_cpus is not None:
        set_windows_affinity(process.pid, windows_cpus)

    chunks: queue.Queue[bytes | None] = queue.Queue()
    threading.Thread(target=interaction.read_output, args=(chunks,), daemon=True).start()
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
                    raise RuntimeError(f"OpenVMM exited with status {process.returncode}")
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
                    return elapsed_ms, peak_bytes, None
                teardown_ms = (time.perf_counter_ns() - teardown_started) / 1_000_000
                if teardown_mode == "guest-exit" and returncode != 0:
                    raise RuntimeError(
                        f"OpenVMM exited with status {returncode} during teardown"
                    )
                return elapsed_ms, peak_bytes, teardown_ms
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


def benchmark(
    command: Sequence[str],
    *,
    warmups: int,
    runs: int,
    timeout: float,
    marker: bytes = BOOT_MARKER,
    windows_cpus: set[int] | None = None,
    teardown_mode: str = "guest-exit",
) -> dict[str, object]:
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    for index in range(warmups):
        value, peak_bytes, teardown_ms = measure_once(
            command,
            environment=environment,
            timeout=timeout,
            marker=marker,
            windows_cpus=windows_cpus,
            teardown_mode=teardown_mode,
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

    samples = []
    peak_rss_samples = []
    teardown_samples = []
    for index in range(runs):
        value, peak_bytes, teardown_ms = measure_once(
            command,
            environment=environment,
            timeout=timeout,
            marker=marker,
            windows_cpus=windows_cpus,
            teardown_mode=teardown_mode,
        )
        samples.append(value)
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
    threading.Thread(target=interaction.read_output, args=(chunks,), daemon=True).start()
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
                    raise RuntimeError(f"OpenVMM exited with status {process.returncode}")
                continue
            if chunk is None:
                observe_snapshot_publication()
                break
            output.extend(chunk)
            if not snapshot_requested and BOOT_MARKER in output:
                snapshot_started_ns = time.perf_counter_ns()
                interaction.write_input(
                    b"nvx-snapshot; echo " + RESTORE_MARKER + b"\n"
                )
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
) -> dict[str, object]:
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
) -> dict[str, object]:
    samples = []
    request_to_publication_samples = []
    post_publication_exit_samples = []
    for index in range(args.warmups + args.runs):
        with tempfile.TemporaryDirectory(prefix="openvmm-snapshot-capture-") as temp_dir:
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


def print_snapshot_summary(backend: str, result: dict[str, object]) -> None:
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
) -> dict[str, object]:
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


def print_summary(backend: str, result: dict[str, object]) -> None:
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


def print_phase2_summary(backend: str, result: dict[str, object]) -> None:
    memory_mib = int(result["memory_mib"])
    metrics = result["metrics"]
    if not isinstance(metrics, dict):
        raise TypeError("phase 2 result metrics must be an object")
    print(f"phase2/{backend} ({memory_mib} MiB RAM):", flush=True)
    for name in (
        "restore_prepare",
        "repeat_restore_prepare",
        "new_process_restore_prepare",
    ):
        metric = metrics[name]
        if not isinstance(metric, dict):
            raise TypeError(f"phase 2 metric {name} must be an object")
        print(f"  {name}: p50={float(metric['p50_ms']):.3f} ms", flush=True)
    for name in ("snapshot_publish", "snapshot_verify", "repeat_verify"):
        metric = metrics[name]
        if not isinstance(metric, dict):
            raise TypeError(f"phase 2 metric {name} must be an object")
        p50_ms = float(metric["p50_ms"])
        throughput = memory_mib / (p50_ms / 1000.0)
        print(
            f"  {name}: p50={p50_ms:.3f} ms ({throughput:.1f} MiB/s)",
            flush=True,
        )
    for name in ("cow_map", "repeat_cow_map", "cow_dirty_all"):
        metric = metrics[name]
        if not isinstance(metric, dict):
            raise TypeError(f"phase 2 metric {name} must be an object")
        p50_ms = float(metric["p50_ms"])
        print(f"  {name}: p50={p50_ms:.3f} ms", flush=True)


def compare_cold_start_to_restore_prepare(
    cold_start: dict[str, object],
    phase2: dict[str, object],
) -> dict[str, object]:
    metrics = phase2["metrics"]
    if not isinstance(metrics, dict):
        raise TypeError("phase 2 result metrics must be an object")
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
    cold_start: dict[str, object],
    snapshot_restore: dict[str, object],
) -> dict[str, object]:
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
    comparison: dict[str, object],
) -> None:
    print(
        f"comparison/{backend} (new-process host-only lower bound): "
        f"cold-start={comparison['cold_start_p50_ms']:.3f} ms "
        f"restore-prep={comparison['new_process_restore_prepare_p50_ms']:.3f} ms "
        f"cold/restore-prep={comparison['cold_start_over_new_process_restore_prepare']:.2f}x",
        flush=True,
    )


def print_e2e_comparison(backend: str, comparison: dict[str, object]) -> None:
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
        openvmm_dir
        / "target"
        / "release"
        / "examples"
        / "phase2_snapshot_bench.exe",
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
) -> dict[str, object]:
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
    output = []
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
            result = json.loads(line.removeprefix(PHASE2_RESULT_PREFIX))
            if not result.get("artifact_unchanged"):
                raise RuntimeError("phase 2 benchmark did not preserve the snapshot artifact")
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
) -> dict[str, object]:
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
) -> dict[str, object]:
    return {
        "timestamp_utc": dt.datetime.now(dt.UTC).isoformat(),
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
    run_guest = run_boot or run_snapshot or run_restore
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
) -> dict[str, object]:
    stage_dir = "/tmp/openvmm-microvm-benchmark"
    stage_kvm(executable, kernel, initrd, stage_dir)
    script_wsl = windows_to_wsl(Path(__file__))
    command = [
        "wsl.exe",
        "--exec",
        "python3",
        script_wsl,
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
                return json.loads(line.removeprefix(KVM_RESULT_PREFIX))
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
) -> dict[str, object]:
    stage_dir = "/tmp/openvmm-microvm-benchmark"
    stage_kvm(executable, kernel, initrd, stage_dir)
    script_wsl = windows_to_wsl(Path(__file__))
    command = [
        "wsl.exe",
        "--exec",
        "python3",
        script_wsl,
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
                return json.loads(line.removeprefix(KVM_RESTORE_RESULT_PREFIX))
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
) -> dict[str, object]:
    stage_dir = "/tmp/openvmm-microvm-benchmark"
    stage_kvm(executable, kernel, initrd, stage_dir)
    script_wsl = windows_to_wsl(Path(__file__))
    command = [
        "wsl.exe",
        "--exec",
        "python3",
        script_wsl,
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
                return json.loads(line.removeprefix(KVM_SNAPSHOT_RESULT_PREFIX))
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


def main() -> int:
    args = parse_args()
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
    run_guest = run_boot or run_snapshot or run_restore
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


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("Interrupted", file=sys.stderr)
        raise SystemExit(130) from None
    except Exception as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
