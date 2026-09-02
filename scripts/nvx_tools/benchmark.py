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
from collections.abc import Callable, Generator, Sequence
from pathlib import Path
from typing import TextIO, TypedDict, cast

BOOT_MARKER = b"ALPINE-MICROVM-BOOT-OK"
RESTORE_MARKER = b"OPENVMM-SNAPSHOT-RESTORE-OK"
TEARDOWN_TIMEOUT_SECONDS = 15.0
BASE_TUNING = (
    "tsc=reliable no_timer_check random.trust_cpu=on "
    "rcupdate.rcu_expedited=1 nokaslr mitigations=off "
    "cryptomgr.notests quiet loglevel=0"
)
KVM_RESULT_PREFIX = "OPENVMM_KVM_RESULT="
KVM_E2E_RESULT_PREFIX = "OPENVMM_KVM_E2E_RESULT="
KVM_RESTORE_RESULT_PREFIX = "OPENVMM_KVM_RESTORE_RESULT="
KVM_SNAPSHOT_RESULT_PREFIX = "OPENVMM_KVM_SNAPSHOT_RESULT="
PHASE2_RESULT_PREFIX = "OPENVMM_PHASE2_RESULT="
SNAPSHOT_PROFILE_ENV = "OPENVMM_STARTUP_PROFILE"
SNAPSHOT_PROFILE_PREFIX = b"OPENVMM_SNAPSHOT_PROFILE_V1 "
NVX_SCRIPT = Path(__file__).resolve().parents[1] / "nvx.py"
WORKLOAD_SUITES = frozenset(
    {
        "cold-start",
        "virtfs",
        "shell-snapshot",
        "shell-snapshot-restore",
        "snapshot-restore-vcpu",
        "network-snapshot",
        "performance",
    }
)
NETWORK_SNAPSHOT_BACKENDS = frozenset({"kvm", "mshv", "whp"})
DD_RATE_PATTERN = re.compile(r"([0-9.]+)\s*([KMG]?)B/s")
VIRTFS_COMPLETION_MARKER = b"NVX-VIRTFS-WORKLOAD-COMPLETE"
VIRTFS_ROUNDTRIP_MARKER = b"VIRTFS-LIVE-ROUNDTRIP-OK"
VIRTFS_GUEST_TO_HOST = b"guest-to-host\n"
VIRTFS_HOST_WAITING = b"waiting\n"
VIRTFS_HOST_TO_GUEST = b"host-to-guest\n"
SMP_PROBE_COMPLETION_MARKER = b"NVX-SMP-PROBE-OK"
SNAPSHOT_FILENAMES = ("manifest.bin", "state.bin", "memory.bin")
SHELL_SNAPSHOT_MEMORY_MIB = (64, 128, 256, 512)
SNAPSHOT_PROFILE_MEMORY_MIB = (*SHELL_SNAPSHOT_MEMORY_MIB, 1024)
RESTORE_VCPU_TARGETS = (1, 2, 4, 8)
PERFORMANCE_LOG_FILENAMES = (
    "cold-start.log",
    "virtfs.log",
    "shell-snapshot.log",
    "shell-snapshot-restore.log",
    "network.log",
)
LEGACY_PYTHON_LOG_FILENAMES = ("snapshot.log", "snapshot-hello.log")
BENCHMARK_METADATA_FILENAME = "benchmark-metadata.json"
MICROVM_ABI_VERSION = 2


class ProfiledResult(TypedDict, total=False):
    profile: LifecycleProfileSummary


class LifecycleProfileSummary(TypedDict):
    schema_version: int
    raw_samples: list[dict[str, object]]
    phases: dict[str, dict[str, object]]


class BenchmarkResult(ProfiledResult):
    samples_ms: list[float]
    p50_ms: float
    p95_ms: float
    min_ms: float
    max_ms: float
    wall_samples_ms: list[float]
    wall_p50_ms: float
    wall_p95_ms: float
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
    teardown_p95_ms: float | None
    teardown_min_ms: float | None
    teardown_max_ms: float | None


class SnapshotCaptureResult(ProfiledResult):
    samples_ms: list[float]
    p50_ms: float
    p95_ms: float
    min_ms: float
    max_ms: float
    request_to_publication_samples_ms: list[float]
    request_to_publication_p50_ms: float
    request_to_publication_p95_ms: float
    request_to_publication_min_ms: float
    request_to_publication_max_ms: float
    post_publication_exit_samples_ms: list[float]
    post_publication_exit_p50_ms: float
    post_publication_exit_p95_ms: float
    post_publication_exit_min_ms: float
    post_publication_exit_max_ms: float
    peak_rss_samples_bytes: list[int]
    peak_rss_p50_bytes: int
    peak_rss_min_bytes: int
    peak_rss_max_bytes: int


class Phase2Metric(TypedDict):
    samples_ms: list[float]
    p50_ms: float
    p95_ms: float
    min_ms: float
    max_ms: float


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
    snapshot_profile_matrix: dict[str, dict[str, dict[str, object]]]


class KvmE2EResult(TypedDict):
    cold_start: BenchmarkResult
    snapshot_capture: SnapshotCaptureResult
    snapshot_restore: BenchmarkResult


class GuestCommandResult(TypedDict):
    text: str
    wall_ms: float
    peak_rss_bytes: int


def configure_parser(
    parser: argparse.ArgumentParser,
    repository_dir: Path,
) -> None:
    default_cpus = ",".join(str(cpu) for cpu in sorted(physical_cpu_representatives()))
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
            "snapshot-profile",
            "all",
            *sorted(WORKLOAD_SUITES),
        ),
        default="boot",
        help=(
            "benchmark suite to run: e2e measures cold boot, snapshot "
            "generation, and full snapshot restore; performance runs the "
            "canonical non-Python workloads (default: boot)"
        ),
    )
    parser.add_argument(
        "--backend",
        choices=("whp", "kvm", "mshv", "both"),
        default="both" if os.name == "nt" else "kvm",
        help="backend to benchmark (default: both on Windows, kvm on Linux)",
    )
    parser.add_argument(
        "--platform",
        help=("host-typed performance series (for example, linux-kvm-baremetal)"),
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
        "--processors",
        type=int,
        choices=(1, 2, 4, 8),
        default=1,
        help="microVM ABI-v2 processor count (default: 1)",
    )
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
        default=None,
        metavar="MIB",
        help=(
            "snapshot memory sizes (default: 64 128 256 512, or "
            "64 128 256 512 1024 for --suite snapshot-profile)"
        ),
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
    parser.add_argument(
        "--network-profile",
        choices=("portable",),
        help="required portable network capability profile when --net is specified",
    )
    parser.add_argument(
        "--cpus",
        default=default_cpus,
        help=f"logical CPUs used for affinity, in taskset syntax (default: {default_cpus})",
    )
    parser.add_argument(
        "--host-cpu-reserve",
        type=nonnegative_int,
        default=2,
        help="affinity CPUs reserved for VMM/device work (default: 2)",
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
        "--snapshot-profile",
        action="store_true",
        help=(
            "retain opt-in OpenVMM snapshot lifecycle phase samples and host "
            "counters; implied by --suite snapshot-profile"
        ),
    )
    parser.add_argument(
        "--cache-state",
        choices=("warm", "cold", "both"),
        default="both",
        help=(
            "snapshot artifact cache states for --suite snapshot-profile "
            "(default: both)"
        ),
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


def apply_benchmark_suite_defaults(args: argparse.Namespace) -> None:
    if args.shell_memories is None:
        default = (
            SNAPSHOT_PROFILE_MEMORY_MIB
            if args.suite == "snapshot-profile"
            else SHELL_SNAPSHOT_MEMORY_MIB
        )
        args.shell_memories = list(default)


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than 0")
    return parsed


def nonnegative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be at least 0")
    return parsed


def run_checked(command: Sequence[str], *, cwd: Path | None = None) -> None:
    print("+", subprocess.list2cmdline(list(command)), flush=True)
    subprocess.run(command, cwd=cwd, check=True)


def append_network_arguments(
    command: list[str],
    network: str,
    profile: str = "portable",
) -> None:
    command.extend(("--net", network, "--network-profile", profile))


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


def physical_cpu_representatives() -> set[int]:
    if os.name == "nt":
        return windows_physical_cpu_representatives()
    if sys.platform.startswith("linux"):
        representatives: set[int] = set()
        sibling_sets: set[str] = set()
        for topology in sorted(
            Path("/sys/devices/system/cpu").glob("cpu[0-9]*/topology")
        ):
            siblings_path = topology / "thread_siblings_list"
            try:
                siblings = siblings_path.read_text(encoding="ascii").strip()
                available = parse_cpu_set(siblings)
            except (OSError, ValueError):
                continue
            key = ",".join(str(cpu) for cpu in sorted(available))
            if key not in sibling_sets:
                sibling_sets.add(key)
                representatives.add(min(available))
        if representatives:
            return representatives
    return set(range(os.cpu_count() or 1))


def windows_physical_cpu_representatives() -> set[int]:
    if os.name != "nt":
        raise RuntimeError("Windows processor topology is unavailable")

    class GroupAffinity(ctypes.Structure):
        _fields_ = [
            ("mask", ctypes.c_size_t),
            ("group", ctypes.c_ushort),
            ("reserved", ctypes.c_ushort * 3),
        ]

    relation_processor_core = 0
    error_insufficient_buffer = 122
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    query = kernel32.GetLogicalProcessorInformationEx
    query.argtypes = (ctypes.c_int, ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong))
    query.restype = ctypes.c_int
    length = ctypes.c_ulong()
    if query(relation_processor_core, None, ctypes.byref(length)):
        raise RuntimeError("processor topology size query unexpectedly succeeded")
    if ctypes.get_last_error() != error_insufficient_buffer:
        raise ctypes.WinError(ctypes.get_last_error())
    buffer = ctypes.create_string_buffer(length.value)
    if not query(relation_processor_core, buffer, ctypes.byref(length)):
        raise ctypes.WinError(ctypes.get_last_error())

    representatives: set[int] = set()
    offset = 0
    while offset < length.value:
        relationship = ctypes.c_uint32.from_buffer(buffer, offset).value
        record_size = ctypes.c_uint32.from_buffer(buffer, offset + 4).value
        if relationship == relation_processor_core:
            group_count = ctypes.c_ushort.from_buffer(buffer, offset + 30).value
            group_offset = offset + 32
            for index in range(group_count):
                affinity = GroupAffinity.from_buffer(
                    buffer, group_offset + index * ctypes.sizeof(GroupAffinity)
                )
                if affinity.group == 0 and affinity.mask:
                    representatives.add(
                        (int(affinity.mask) & -int(affinity.mask)).bit_length() - 1
                    )
        if record_size == 0:
            raise RuntimeError("Windows processor topology contains an empty record")
        offset += record_size
    if not representatives:
        raise RuntimeError("Windows did not report any physical processor cores")
    return representatives


def validate_benchmark_cpu_set(
    cpus: set[int], processors: int, host_cpu_reserve: int = 2
) -> None:
    required = processors + host_cpu_reserve
    if len(cpus) < required:
        raise ValueError(
            f"CPU set selects {len(cpus)} logical CPUs; a {processors}-vCPU benchmark "
            f"requires at least {required} for the guest plus "
            f"{host_cpu_reserve} reserved for VMM/device work"
        )


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
        ("private_usage", ctypes.c_size_t),
    ]


def windows_process_memory_counters(pid: int) -> ProcessMemoryCounters:
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
        return counters
    finally:
        kernel32.CloseHandle(handle)


def windows_peak_rss_bytes(pid: int) -> int:
    return int(windows_process_memory_counters(pid).peak_working_set_size)


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


def _linux_status_bytes(status: str, name: str) -> int | None:
    prefix = f"{name}:"
    for line in status.splitlines():
        if line.startswith(prefix):
            _, value, unit = line.split()
            if unit != "kB":
                raise RuntimeError(f"unexpected {name} unit {unit!r}")
            return int(value) * 1024
    return None


def process_resource_counters(pid: int) -> dict[str, int]:
    """Return cheap process counters available on the current host."""
    try:
        if os.name == "nt":
            counters = windows_process_memory_counters(pid)
            return {
                "rss_bytes": int(counters.working_set_size),
                "peak_rss_bytes": int(counters.peak_working_set_size),
                "commit_bytes": int(counters.private_usage),
                "page_faults": int(counters.page_fault_count),
            }
        if sys.platform.startswith("linux"):
            status = Path(f"/proc/{pid}/status").read_text(encoding="ascii")
            values: dict[str, int] = {}
            for field, name in (
                ("rss_bytes", "VmRSS"),
                ("peak_rss_bytes", "VmHWM"),
            ):
                value = _linux_status_bytes(status, name)
                if value is not None:
                    values[field] = value

            stat = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
            fields = stat.rsplit(")", 1)[1].split()
            values["minor_faults"] = int(fields[7])
            values["major_faults"] = int(fields[9])
            values["page_faults"] = values["minor_faults"] + values["major_faults"]

            try:
                rollup = Path(f"/proc/{pid}/smaps_rollup").read_text(encoding="ascii")
                private_clean = _linux_status_bytes(rollup, "Private_Clean") or 0
                private_dirty = _linux_status_bytes(rollup, "Private_Dirty") or 0
                values["private_dirty_bytes"] = private_dirty
                values["private_rss_bytes"] = private_clean + private_dirty
            except (OSError, RuntimeError, ValueError):
                pass
            return values
    except (OSError, RuntimeError, ValueError):
        return {}
    return {}


def parse_snapshot_profile_line(line: bytes) -> dict[str, object] | None:
    line = line.removesuffix(b"\r")
    if not line.startswith(SNAPSHOT_PROFILE_PREFIX):
        return None
    fields: dict[str, object] = {}
    for token in line.removeprefix(SNAPSHOT_PROFILE_PREFIX).decode("ascii").split():
        name, separator, value = token.partition("=")
        if not separator or not name or not value:
            raise ValueError(f"invalid snapshot profile field {token!r}")
        if name in {
            "duration_ns",
            "process_elapsed_ns",
            "pid",
            "logical_bytes",
            "allocated_bytes",
            "gpa_faults",
            "populated_bytes",
        }:
            fields[name] = int(value)
        elif name == "exclusive":
            if value not in ("0", "1"):
                raise ValueError(f"invalid exclusive value {value!r}")
            fields[name] = value == "1"
        else:
            fields[name] = value
    required = {
        "operation",
        "phase",
        "exclusive",
        "duration_ns",
        "process_elapsed_ns",
        "pid",
    }
    missing = required.difference(fields)
    if missing:
        raise ValueError(f"snapshot profile record is missing {sorted(missing)}")
    fields["source"] = "openvmm"
    return fields


def _profile_int(record: dict[str, object], name: str) -> int:
    value = record[name]
    if not isinstance(value, int):
        raise ValueError(f"snapshot profile field {name!r} is not an integer")
    return value


def _profile_float(record: dict[str, object], name: str) -> float:
    value = record[name]
    if not isinstance(value, (int, float)):
        raise ValueError(f"snapshot profile field {name!r} is not numeric")
    return float(value)


class SnapshotProfileCollector:
    def __init__(self, pid: int, process_started_ns: int) -> None:
        self.pid = pid
        self.process_started_ns = process_started_ns
        self.pending = bytearray()
        self.records: list[dict[str, object]] = []

    def feed(self, chunk: bytes) -> None:
        self.pending.extend(chunk)
        while b"\n" in self.pending:
            raw_line, _, remaining = self.pending.partition(b"\n")
            self.pending = bytearray(remaining)
            self._parse_line(bytes(raw_line), time.perf_counter_ns())

    def _parse_line(self, line: bytes, observed_ns: int) -> None:
        record = parse_snapshot_profile_line(line)
        if record is None:
            return
        record["observer_elapsed_ns"] = max(0, observed_ns - self.process_started_ns)
        counters = process_resource_counters(self.pid)
        if counters:
            record["host_counters"] = counters
        self.records.append(record)

    def _finish_pending(self) -> None:
        if self.pending:
            self._parse_line(bytes(self.pending), time.perf_counter_ns())
            self.pending.clear()

    def _external_record(
        self,
        operation: str,
        phase: str,
        duration_ns: int,
        ended_ns: int,
        *,
        exclusive: bool,
    ) -> dict[str, object]:
        record: dict[str, object] = {
            "operation": operation,
            "phase": phase,
            "exclusive": exclusive,
            "duration_ns": max(0, duration_ns),
            "observer_elapsed_ns": max(0, ended_ns - self.process_started_ns),
            "pid": self.pid,
            "source": "benchmark_observer",
        }
        counters = process_resource_counters(self.pid)
        if counters:
            record["host_counters"] = counters
        return record

    def finish_restore(self, marker_reached_ns: int) -> dict[str, object]:
        self._finish_pending()
        if self.records:
            first = self.records[0]
            first_elapsed = _profile_int(first, "process_elapsed_ns")
            first_duration = _profile_int(first, "duration_ns")
            self.records.insert(
                0,
                {
                    "operation": "startup",
                    "phase": "process_startup",
                    "exclusive": True,
                    "duration_ns": max(0, first_elapsed - first_duration),
                    "process_elapsed_ns": max(0, first_elapsed - first_duration),
                    "pid": self.pid,
                    "source": "openvmm_clock",
                },
            )
        device_start = next(
            (
                record
                for record in reversed(self.records)
                if record.get("operation") == "restore"
                and record.get("phase") == "device_start"
            ),
            None,
        )
        if device_start is not None:
            device_end_ns = self.process_started_ns + int(
                _profile_int(device_start, "observer_elapsed_ns")
            )
            self.records.append(
                self._external_record(
                    "restore",
                    "resume_to_readiness",
                    marker_reached_ns - device_end_ns,
                    marker_reached_ns,
                    exclusive=True,
                )
            )
        self.records.append(
            self._external_record(
                "restore",
                "process_launch_to_readiness",
                marker_reached_ns - self.process_started_ns,
                marker_reached_ns,
                exclusive=False,
            )
        )
        return {"records": self.records}

    def finish_capture(
        self,
        snapshot_started_ns: int,
        snapshot_published_ns: int,
        source_exited_ns: int,
    ) -> dict[str, object]:
        self._finish_pending()
        self.records.extend(
            (
                self._external_record(
                    "capture",
                    "request_to_publication",
                    snapshot_published_ns - snapshot_started_ns,
                    snapshot_published_ns,
                    exclusive=False,
                ),
                self._external_record(
                    "capture",
                    "source_teardown",
                    source_exited_ns - snapshot_published_ns,
                    source_exited_ns,
                    exclusive=True,
                ),
            )
        )
        return {"records": self.records}


def summarize_lifecycle_profiles(
    raw_samples: list[dict[str, object]],
) -> LifecycleProfileSummary:
    duration_samples: dict[str, list[float]] = {}
    attributes: dict[str, tuple[bool, str]] = {}
    for sample in raw_samples:
        records = cast(list[dict[str, object]], sample.get("records", []))
        for record in records:
            operation = str(record["operation"])
            phase = str(record["phase"])
            key = f"{operation}.{phase}"
            duration_samples.setdefault(key, []).append(
                _profile_int(record, "duration_ns") / 1_000_000
            )
            attributes[key] = (
                bool(record["exclusive"]),
                str(record.get("source", "unknown")),
            )
    phases: dict[str, dict[str, object]] = {}
    for key, samples in duration_samples.items():
        exclusive, source = attributes[key]
        phases[key] = {
            "exclusive": exclusive,
            "source": source,
            "samples_ms": samples,
            "p50_ms": statistics.median(samples),
            "p95_ms": nearest_rank_percentile(samples, 95),
            "min_ms": min(samples),
            "max_ms": max(samples),
        }
    return {
        "schema_version": 1,
        "raw_samples": raw_samples,
        "phases": phases,
    }


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
    marker_must_be_line: bool = False,
    windows_cpus: set[int] | None = None,
    teardown_mode: str = "guest-exit",
    guest_exit_prequeued: bool = False,
    cleanup_managed_network: bool = False,
    snapshot_profile: bool = False,
    profile_sink: list[dict[str, object]] | None = None,
) -> tuple[float, int, float | None, float]:
    started = time.perf_counter_ns()
    interaction = InteractiveProcess(command, environment)
    process = interaction.process
    profile = (
        SnapshotProfileCollector(process.pid, started) if snapshot_profile else None
    )
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
            if profile is not None:
                profile.feed(chunk)
            output.extend(chunk)
            marker_seen = (
                contains_output_line(output, marker)
                if marker_must_be_line
                else marker in output
            )
            if marker_seen:
                marker_reached = time.perf_counter_ns()
                # A prequeued guest exit can terminate OpenVMM immediately
                # after writing the marker. Sample RSS before the more detailed
                # opt-in profile counters, and tolerate an already-gone process.
                peak_bytes = _try_peak_rss(process, 0)
                if profile is not None and profile_sink is not None:
                    profile_sink.append(profile.finish_restore(marker_reached))
                elapsed_ms = (marker_reached - started) / 1_000_000
                teardown_started = time.perf_counter_ns()
                if teardown_mode != "guest-exit":
                    process.terminate()
                elif not guest_exit_prequeued:
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
    marker_must_be_line: bool = False,
    windows_cpus: set[int] | None = None,
    teardown_mode: str = "guest-exit",
    guest_exit_prequeued: bool = False,
    cleanup_managed_network: bool = False,
    snapshot_profile: bool = False,
    before_each: Callable[[], None] | None = None,
) -> BenchmarkResult:
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    environment.pop(SNAPSHOT_PROFILE_ENV, None)
    if snapshot_profile:
        environment[SNAPSHOT_PROFILE_ENV] = "1"
    for index in range(warmups):
        if before_each is not None:
            before_each()
        value, peak_bytes, teardown_ms, _wall_ms = measure_once(
            command,
            environment=environment,
            timeout=timeout,
            marker=marker,
            marker_must_be_line=marker_must_be_line,
            windows_cpus=windows_cpus,
            teardown_mode=teardown_mode,
            guest_exit_prequeued=guest_exit_prequeued,
            cleanup_managed_network=cleanup_managed_network,
            snapshot_profile=snapshot_profile,
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
    profile_samples: list[dict[str, object]] = []
    for index in range(runs):
        if before_each is not None:
            before_each()
        value, peak_bytes, teardown_ms, wall_ms = measure_once(
            command,
            environment=environment,
            timeout=timeout,
            marker=marker,
            marker_must_be_line=marker_must_be_line,
            windows_cpus=windows_cpus,
            teardown_mode=teardown_mode,
            guest_exit_prequeued=guest_exit_prequeued,
            cleanup_managed_network=cleanup_managed_network,
            snapshot_profile=snapshot_profile,
            profile_sink=profile_samples,
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
    result: BenchmarkResult = {
        "samples_ms": samples,
        "p50_ms": statistics.median(samples),
        "p95_ms": nearest_rank_percentile(samples, 95),
        "min_ms": min(samples),
        "max_ms": max(samples),
        "wall_samples_ms": wall_samples,
        "wall_p50_ms": statistics.median(wall_samples),
        "wall_p95_ms": nearest_rank_percentile(wall_samples, 95),
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
        "teardown_p95_ms": (
            nearest_rank_percentile(completed_teardowns, 95)
            if completed_teardowns
            else None
        ),
        "teardown_min_ms": min(completed_teardowns, default=None),
        "teardown_max_ms": max(completed_teardowns, default=None),
    }
    if snapshot_profile:
        result["profile"] = summarize_lifecycle_profiles(profile_samples)
    return result


def nearest_rank_percentile(samples: Sequence[float], percentile: int) -> float:
    if not samples:
        raise ValueError("cannot calculate a percentile without samples")
    if not 1 <= percentile <= 100:
        raise ValueError("percentile must be in 1..100")
    ordered = sorted(samples)
    index = (percentile * len(ordered) + 99) // 100 - 1
    return ordered[index]


def format_sample_summary(samples: Sequence[float], *, unit: str = "ms") -> str:
    if not samples:
        raise ValueError("cannot summarize an empty sample set")
    return (
        f"{statistics.median(samples):.1f} {unit}  "
        f"(p95 {nearest_rank_percentile(samples, 95):.1f}, "
        f"min {min(samples):.1f}, max {max(samples):.1f}, n={len(samples)})"
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
    processors: int = 1,
    command_prefix: Sequence[str] = (),
    network: str | None = None,
    mount: str | None = None,
) -> list[str]:
    command = [
        *command_prefix,
        str(executable),
        "--single-process",
        "--machine",
        "microvm-v2",
        "--processors",
        str(processors),
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
        append_network_arguments(command, network)
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
        f"median of {args.runs} runs, {args.memory_mib} MiB, {args.processors} vCPU"
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
                processors=args.processors,
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


def smp_probe_script(
    processors: int,
    *,
    exit_guest: bool = True,
    network_gateway: str | None = None,
    ioapic_irq: int | None = None,
) -> str:
    if processors not in (1, 2, 4, 8):
        raise ValueError("microVM ABI-v2 SMP probe supports 1, 2, 4, or 8 vCPUs")
    if (network_gateway is None) != (ioapic_irq is None):
        raise ValueError("network gateway and IOAPIC IRQ must be specified together")
    apic_ids = ",".join(str(cpu) for cpu in range(processors))
    lines = [
        "set -eu",
        'trap \'status=$?; if [ "$status" -ne 0 ]; then nvx-exit "$status"; fi\' EXIT',
        f"expected={processors}",
        'online="$(getconf _NPROCESSORS_ONLN)"',
        '[ "$online" -eq "$expected" ] || { echo "SMP-ONLINE-FAIL expected=$expected actual=$online"; exit 81; }',
        'loc_before="$(awk -v expected="$expected" \'/^LOC:/ { for (cpu = 0; cpu < expected; cpu++) printf "%s%s", $(cpu + 2), (cpu + 1 == expected ? "" : " "); exit }\' /proc/interrupts)"',
        '[ "$(printf "%s\n" "$loc_before" | awk \'{ print NF }\')" -eq "$expected" ] || { echo "SMP-LAPIC-FAIL missing-local-timer-counters"; exit 87; }',
        'worker_dir="/tmp/nvx-smp-probe-$$"',
        'worker_script="$worker_dir/worker"',
        'rm -rf "$worker_dir"',
        'mkdir -p "$worker_dir"',
        "cat >\"$worker_script\" <<'NVX_SMP_WORKER'",
        "#!/bin/sh",
        "set -eu",
        'cpu="$1"',
        'before="$2"',
        'result="$3"',
        'apic_id="$4"',
        "actual=\"$(awk '{ print $39 }' /proc/self/stat)\"",
        '[ "$actual" -eq "$cpu" ] || { echo "SMP-WORKER-FAIL requested=$cpu actual=$actual"; exit 87; }',
        "read_loc_counter() {",
        "  while read -r label cpu0 cpu1 cpu2 cpu3 cpu4 cpu5 cpu6 cpu7 rest; do",
        '    if [ "$label" = "LOC:" ]; then',
        '      case "$cpu" in',
        "        0) current=$cpu0 ;;",
        "        1) current=$cpu1 ;;",
        "        2) current=$cpu2 ;;",
        "        3) current=$cpu3 ;;",
        "        4) current=$cpu4 ;;",
        "        5) current=$cpu5 ;;",
        "        6) current=$cpu6 ;;",
        "        7) current=$cpu7 ;;",
        "        *) return 1 ;;",
        "      esac",
        "      return 0",
        "    fi",
        "  done </proc/interrupts",
        "  return 1",
        "}",
        "while :; do",
        "  read_loc_counter",
        '  [ "$current" -gt "$before" ] && break',
        "done",
        'printf \'%s %s %s\\n\' "$actual" "$current" "$apic_id" >"$result"',
        "NVX_SMP_WORKER",
        'chmod +x "$worker_script"',
        "workers=0",
        'worker_pids=""',
        "for cpu in $(seq 0 $((expected - 1))); do",
        '  topology="/sys/devices/system/cpu/cpu${cpu}/topology"',
        '  [ "$(cat "$topology/physical_package_id")" -eq 0 ] || exit 82',
        '  [ "$(cat "$topology/die_id")" -eq 0 ] || exit 83',
        '  [ "$(cat "$topology/core_id")" -eq "$cpu" ] || exit 84',
        '  [ "$(cat "$topology/thread_siblings_list")" = "$cpu" ] || exit 85',
        '  apic_id="$(awk -v target="$cpu" \'$1 == "processor" { processor = $3 } $1 == "apicid" && processor == target { print $3; exit }\' /proc/cpuinfo)"',
        '  [ "$apic_id" -eq "$cpu" ] || { echo "SMP-APIC-FAIL cpu=$cpu apic=$apic_id"; exit 86; }',
        "  field=$((cpu + 1))",
        '  before="$(printf "%s\\n" "$loc_before" | awk -v field="$field" \'{ print $field }\')"',
        '  result="$worker_dir/$cpu"',
        '  taskset -c "$cpu" "$worker_script" "$cpu" "$before" "$result" "$apic_id" &',
        '  worker_pids="$worker_pids $!"',
        "  workers=$((workers + 1))",
        "done",
        "for pid in $worker_pids; do",
        '  wait "$pid"',
        "done",
        "for cpu in $(seq 0 $((expected - 1))); do",
        '  read -r actual current apic_id <"$worker_dir/$cpu"',
        '  echo "SMP-WORKER-OK cpu=$cpu apic=$apic_id actual=$actual loc_after=$current"',
        "done",
        'loc_after="$(awk -v expected="$expected" \'/^LOC:/ { for (cpu = 0; cpu < expected; cpu++) printf "%s%s", $(cpu + 2), (cpu + 1 == expected ? "" : " "); exit }\' /proc/interrupts)"',
        "for cpu in $(seq 0 $((expected - 1))); do",
        "  field=$((cpu + 1))",
        '  before="$(printf "%s\n" "$loc_before" | awk -v field="$field" \'{ print $field }\')"',
        '  after="$(printf "%s\n" "$loc_after" | awk -v field="$field" \'{ print $field }\')"',
        '  [ "$after" -gt "$before" ] || { echo "SMP-LAPIC-FAIL cpu=$cpu before=$before after=$after"; exit 88; }',
        '  if [ "$cpu" -gt 0 ]; then',
        "    ipi=\"$(awk -v field=$((cpu + 2)) '/^(RES|CAL):/ { total += $field } END { print total + 0 }' /proc/interrupts)\"",
        '    [ "$ipi" -gt 0 ] || { echo "SMP-IPI-FAIL cpu=$cpu count=$ipi"; exit 89; }',
        "  fi",
        "done",
        'echo "SMP-INTERRUPTS-OK loc_before=$loc_before loc_after=$loc_after"',
        f'echo "SMP-TOPOLOGY-OK requested=$expected online=$online sockets=1 cores=$expected threads=1 apic_ids={apic_ids} bsp=0 workers=$workers"',
        'rm -rf "$worker_dir"',
    ]
    if network_gateway is not None and ioapic_irq is not None:
        lines.extend(
            (
                f"ioapic_irq={ioapic_irq}",
                'irq_before="$(awk -v irq="$ioapic_irq" -v expected="$expected" \'$1 == irq ":" { for (cpu = 0; cpu < expected; cpu++) total += $(cpu + 2) } END { print total + 0 }\' /proc/interrupts)"',
                f'ping -c 2 -W 1 "{network_gateway}" >/dev/null',
                'irq_after="$(awk -v irq="$ioapic_irq" -v expected="$expected" \'$1 == irq ":" { for (cpu = 0; cpu < expected; cpu++) total += $(cpu + 2) } END { print total + 0 }\' /proc/interrupts)"',
                '[ "$irq_after" -gt "$irq_before" ] || { echo "SMP-IOAPIC-FAIL irq=$ioapic_irq before=$irq_before after=$irq_after"; exit 91; }',
                'echo "SMP-IOAPIC-OK irq=$ioapic_irq before=$irq_before after=$irq_after"',
            )
        )
    lines.append(f"echo {SMP_PROBE_COMPLETION_MARKER.decode()}")
    if exit_guest:
        lines.append("nvx-exit 0")
    return "\n".join(lines) + "\n"


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
    host_visible.write_bytes(VIRTFS_HOST_WAITING)
    errors: list[str] = []

    def exchange() -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                if guest_visible.read_bytes() == VIRTFS_GUEST_TO_HOST:
                    host_visible.write_bytes(VIRTFS_HOST_TO_GUEST)
                    return
            except (FileNotFoundError, PermissionError):
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
            processors=args.processors,
            command_prefix=command_prefix,
            mount=mount,
        )
        print(
            f"virtfs benchmark: {args.payload_mib} MiB payload, "
            f"{args.virtfs_memory_mib} MiB guest, {args.processors} vCPU, "
            f"median of {runs} runs"
        )
        print()
        print("== sequential throughput (guest dd, conv=fsync writes) ==")
        write_rates: list[float] = []
        read_rates: list[float] = []
        io_rss: list[int] = []
        for index in range(args.warmups + runs):
            run_number = index - args.warmups + 1
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
                    f"virtfs run {index + 1}/{args.warmups + runs} did not report both dd rates"
                )
            if index < args.warmups:
                print(f"  warmup {index + 1}/{args.warmups}: excluded")
                continue
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
        for index in range(args.warmups + runs):
            run_number = index - args.warmups + 1
            result = _run_virtfs_roundtrip(
                command,
                directory,
                run_number,
                runs,
                timeout=max(args.timeout, 120.0),
                windows_cpus=windows_cpus,
                teardown_mode=args.teardown_mode,
            )
            if index < args.warmups:
                print(f"  warmup {index + 1}/{args.warmups}: excluded")
                continue
            roundtrip_samples.append(result["wall_ms"])
            roundtrip_rss.append(result["peak_rss_bytes"])
        print(
            "  live exchange (cold each)         : "
            + format_sample_summary(roundtrip_samples)
        )
        print(
            f"  OpenVMM peak RSS                  : {format_rss_summary(roundtrip_rss)}"
        )


def _format_shell_snapshot_line(name: str, values: Sequence[float]) -> str:
    return (
        f"  {name:<20}: median {statistics.median(values):7.1f} ms   "
        f"(p95 {nearest_rank_percentile(values, 95):.1f}, "
        f"min {min(values):.1f}, max {max(values):.1f}, n={len(values)})"
    )


def _print_shell_snapshot_summary(
    memory_mib: int,
    cold: Sequence[float],
    restored: Sequence[float],
) -> None:
    split_ms = 900.0
    fast = [value for value in cold if value < split_ms]
    slow = [value for value in cold if value >= split_ms]

    print(f"== {memory_mib} MiB ==")
    print(_format_shell_snapshot_line("cold boot", cold))
    if fast and slow:
        print(
            f"       fast path {statistics.median(fast):7.1f} ms (n={len(fast)})  |  "
            f"slow path {statistics.median(slow):7.1f} ms (n={len(slow)}, "
            f"+~{statistics.median(slow) - statistics.median(fast):.0f} ms "
            "TSC PIT-calib)"
        )
    print(_format_shell_snapshot_line("snapshot restore", restored))
    restore_p50 = statistics.median(restored)
    if restore_p50 > 0:
        base = statistics.median(fast) if fast else statistics.median(cold)
        print(
            f"  {'speedup':<20}: {base / restore_p50:.0f}x (fast-path cold) .. "
            f"{statistics.median(cold) / restore_p50:.0f}x (median cold) "
            "faster via snapshot"
        )
    print()


def _benchmark_shell_snapshot_restore(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    memory_mib: int,
    snapshot_path: Path,
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> BenchmarkResult:
    capture_command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0 shellsnap",
        processors=args.processors,
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
    return benchmark(
        [
            *command_prefix,
            *snapshot_restore_command(
                executable,
                backend,
                snapshot_path,
                processors=args.processors,
            ),
        ],
        warmups=args.warmups,
        runs=args.runs,
        timeout=args.timeout,
        marker=BOOT_MARKER,
        windows_cpus=windows_cpus,
        teardown_mode=args.teardown_mode,
        snapshot_profile=bool(getattr(args, "snapshot_profile", False)),
    )


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
        f"median of {args.runs} runs, {args.processors} vCPU"
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
                processors=args.processors,
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
            restored = _benchmark_shell_snapshot_restore(
                args,
                executable,
                kernel,
                initrd,
                backend,
                memory_mib,
                snapshot_path,
                command_prefix=command_prefix,
                windows_cpus=windows_cpus,
            )
            _print_shell_snapshot_summary(
                memory_mib,
                cold["samples_ms"],
                restored["samples_ms"],
            )


def benchmark_shell_snapshot_restore_workload(
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
        "boot-to-shell snapshot restore, "
        f"median of {args.runs} runs, {args.processors} vCPU"
    )
    print(f'marker : "{BOOT_MARKER.decode()}"')
    print(f"kernel : {kernel}")
    print(f"initrd : {initrd}")
    print()
    with tempfile.TemporaryDirectory(prefix="openvmm-shell-restore-") as temporary:
        root = Path(temporary)
        for memory_mib in args.shell_memories:
            restored = _benchmark_shell_snapshot_restore(
                args,
                executable,
                kernel,
                initrd,
                backend,
                memory_mib,
                root / f"shell-{memory_mib}-mib",
                command_prefix=command_prefix,
                windows_cpus=windows_cpus,
            )
            print(f"== {memory_mib} MiB ==")
            print(
                _format_shell_snapshot_line("snapshot restore", restored["samples_ms"])
            )
            restore_profile = restored.get("profile")
            if restore_profile is not None:
                print_lifecycle_profile_summary(
                    f"shell-snapshot-restore/{backend}/{args.processors}vcpu/"
                    f"{memory_mib}-mib",
                    restore_profile,
                )
            print()


def benchmark_snapshot_restore_vcpu_workload(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> None:
    if args.processors != 8:
        raise ValueError("snapshot-restore-vcpu requires --processors 8")
    snapshot_profile = bool(getattr(args, "snapshot_profile", False))

    print(
        "snapshot restore vCPU activation, "
        f"median of {args.runs} runs, capacity 8, boot-online 1"
    )
    print(f'marker : "{BOOT_MARKER.decode()}"')
    print(f"kernel : {kernel}")
    print(f"initrd : {initrd}")
    print()
    with tempfile.TemporaryDirectory(prefix="openvmm-vcpu-restore-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        capture_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            args.memory_mib,
            "quiet loglevel=0 shellsnap maxcpus=1",
            processors=args.processors,
            command_prefix=command_prefix,
        )
        capture_automatic_snapshot(
            [
                *capture_command,
                "--snapshot-destination",
                str(snapshot_path),
            ],
            snapshot_path,
            timeout=args.timeout,
            windows_cpus=windows_cpus,
        )

        for target in RESTORE_VCPU_TARGETS:
            restored = benchmark(
                [
                    *command_prefix,
                    *snapshot_restore_command(
                        executable,
                        backend,
                        snapshot_path,
                        processors=args.processors,
                        restore_processors=target,
                    ),
                ],
                warmups=args.warmups,
                runs=args.runs,
                timeout=args.timeout,
                marker=BOOT_MARKER,
                windows_cpus=windows_cpus,
                teardown_mode=args.teardown_mode,
                snapshot_profile=snapshot_profile,
            )
            print(f"== restore-online {target} vCPU ==")
            print(
                _format_shell_snapshot_line(
                    "snapshot restore", restored["samples_ms"]
                )
            )
            print(f"  OpenVMM peak RSS    : {format_rss_summary(restored['peak_rss_samples_bytes'])}")
            restore_profile = restored.get("profile")
            if restore_profile is not None:
                print_lifecycle_profile_summary(
                    f"snapshot-restore-vcpu/{backend}/capacity-{args.processors}/"
                    f"online-{target}",
                    restore_profile,
                )
            print()


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
        f"median of {args.runs}, {args.network_memory_mib} MiB, "
        f"{args.processors} vCPU, "
        f"--net {network} --network-profile portable"
    )
    print()
    print("== cold boot -> verified gateway connectivity ==")
    probe_cmdline = f"quiet loglevel=0 virtnet_probe={gateway}"
    cold_command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        args.network_memory_mib,
        probe_cmdline,
        processors=args.processors,
        command_prefix=command_prefix,
        network=network,
    )
    ioapic_irq = 5 if backend == "whp" else 10
    run_guest_script(
        cold_command,
        smp_probe_script(
            args.processors,
            network_gateway=gateway,
            ioapic_irq=ioapic_irq,
        ),
        SMP_PROBE_COMPLETION_MARKER,
        timeout=max(args.timeout, 40.0),
        windows_cpus=windows_cpus,
    )
    cold = benchmark(
        cold_command,
        warmups=args.warmups,
        runs=args.runs,
        timeout=max(args.timeout, 40.0),
        marker=cold_marker,
        windows_cpus=windows_cpus,
        teardown_mode=args.teardown_mode,
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
            processors=args.processors,
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
                    processors=args.processors,
                    network_profile="portable",
                ),
            ],
            warmups=args.warmups,
            runs=args.runs,
            timeout=max(args.timeout, 30.0),
            marker=restore_marker,
            windows_cpus=windows_cpus,
            teardown_mode=args.teardown_mode,
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


def _git_revision(repository: Path) -> str | None:
    try:
        return subprocess.run(
            ["git", "-C", str(repository), "rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def write_benchmark_metadata(
    args: argparse.Namespace,
    output_dir: Path,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
) -> Path:
    platform = args.platform or f"{'windows' if os.name == 'nt' else 'linux'}-{backend}"
    effective_network = (
        args.net or "10.0.0.2/24"
        if args.suite in {"network-snapshot", "performance"}
        else args.net
    )
    document = {
        "schema_version": 1,
        "platform": platform,
        "backend": backend,
        "microvm_abi_version": MICROVM_ABI_VERSION,
        "processors": args.processors,
        "restore_processor_targets": (
            list(RESTORE_VCPU_TARGETS)
            if args.suite == "snapshot-restore-vcpu"
            else None
        ),
        "network": effective_network,
        "lifecycle_network": args.net,
        "host_affinity_set": args.cpus,
        "host_cpu_reserve": args.host_cpu_reserve,
        "memory_mib": {
            "lifecycle": args.memory_mib,
            "virtfs": args.virtfs_memory_mib,
            "shell": args.shell_memories,
            "network": args.network_memory_mib,
        },
        "warmups": args.warmups,
        "measured_runs": args.runs,
        "virtfs_measured_runs": args.virtfs_runs,
        "payload_mib": args.payload_mib,
        "virtfs_memory_mib": args.virtfs_memory_mib,
        "shell_memories_mib": args.shell_memories,
        "network_memory_mib": args.network_memory_mib,
        "artifacts": {
            "openvmm": str(executable),
            "kernel": str(kernel),
            "initrd": str(initrd),
        },
        "artifact_revisions": {
            "nvx": _git_revision(args.nvx_dir.resolve()),
            "openvmm": _git_revision(args.openvmm_dir.resolve()),
        },
    }
    path = output_dir / BENCHMARK_METADATA_FILENAME
    path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    return path


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
    if args.suite == "network-snapshot" and backend not in NETWORK_SNAPSHOT_BACKENDS:
        raise ValueError(f"network-snapshot is unsupported on OpenVMM/{backend}")
    if args.suite == "performance":
        requested = ["cold-start", "virtfs", "shell-snapshot"]
        if backend in NETWORK_SNAPSHOT_BACKENDS:
            requested.append("network-snapshot")
    else:
        requested = [args.suite]
    output_dir = args.output_dir
    if output_dir is None and args.suite == "performance":
        platform = (
            args.platform or f"{'windows' if os.name == 'nt' else 'linux'}-{backend}"
        )
        output_dir = (
            args.nvx_dir.resolve()
            / "data"
            / "runs"
            / f"{platform}-microvm-v{MICROVM_ABI_VERSION}-{args.processors}vcpu"
        )
    if output_dir is not None:
        output_dir = output_dir.resolve()
        output_dir.mkdir(parents=True, exist_ok=True)
        if args.suite in {"performance", "shell-snapshot-restore"}:
            for filename in (*PERFORMANCE_LOG_FILENAMES, *LEGACY_PYTHON_LOG_FILENAMES):
                (output_dir / filename).unlink(missing_ok=True)
        if args.suite == "shell-snapshot-restore":
            (output_dir / "acceptance.json").unlink(missing_ok=True)
        if args.suite == "snapshot-restore-vcpu":
            (output_dir / "snapshot-restore-vcpu.log").unlink(missing_ok=True)
        metadata_path = write_benchmark_metadata(
            args, output_dir, executable, kernel, initrd, backend
        )
        print(f"Wrote {metadata_path}", flush=True)

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
        "shell-snapshot-restore": (
            "shell-snapshot-restore.log",
            lambda: benchmark_shell_snapshot_restore_workload(
                args,
                executable,
                kernel,
                initrd,
                backend,
                command_prefix=command_prefix,
                windows_cpus=windows_cpus,
            ),
        ),
        "snapshot-restore-vcpu": (
            "snapshot-restore-vcpu.log",
            lambda: benchmark_snapshot_restore_vcpu_workload(
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


def snapshot_post_restore_script(
    processors: int | None,
    *,
    teardown_mode: str,
    network_gateway: str | None = None,
    ioapic_irq: int | None = None,
) -> str:
    probe = (
        smp_probe_script(
            processors,
            exit_guest=False,
            network_gateway=network_gateway,
            ioapic_irq=ioapic_irq,
        )
        if processors is not None
        else ""
    )
    return (
        probe + f"echo {RESTORE_MARKER.decode()}\n" + _guest_exit_script(teardown_mode)
    )


def capture_snapshot(
    command: Sequence[str],
    snapshot_path: Path,
    *,
    timeout: float,
    windows_cpus: set[int] | None = None,
    processors: int | None = None,
    teardown_mode: str = "guest-exit",
    smp_network_gateway: str | None = None,
    smp_ioapic_irq: int | None = None,
    snapshot_profile: bool = False,
    profile_sink: list[dict[str, object]] | None = None,
) -> tuple[float, float, float, int]:
    if snapshot_path.exists():
        shutil.rmtree(snapshot_path)
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off,openvmm_entry::vm_controller=info"
    environment.pop(SNAPSHOT_PROFILE_ENV, None)
    if snapshot_profile:
        environment[SNAPSHOT_PROFILE_ENV] = "1"
    process_started_ns = time.perf_counter_ns()
    interaction = InteractiveProcess(command, environment)
    process = interaction.process
    profile = (
        SnapshotProfileCollector(process.pid, process_started_ns)
        if snapshot_profile
        else None
    )
    if windows_cpus is not None:
        set_windows_affinity(process.pid, windows_cpus)

    chunks: queue.Queue[bytes | None] = queue.Queue()
    threading.Thread(
        target=interaction.read_output, args=(chunks,), daemon=True
    ).start()
    deadline = time.monotonic() + timeout
    output = bytearray()
    boot_seen = False
    snapshot_requested = False
    snapshot_started_ns = None
    snapshot_published_ns = None
    peak_bytes = 0

    def observe_snapshot_publication() -> None:
        nonlocal snapshot_published_ns
        if (
            snapshot_requested
            and snapshot_published_ns is None
            and snapshot_path.is_dir()
        ):
            snapshot_published_ns = time.perf_counter_ns()

    def request_snapshot() -> None:
        nonlocal snapshot_requested, snapshot_started_ns, deadline
        snapshot_started_ns = time.perf_counter_ns()
        interaction.write_input(
            (
                "nvx-snapshot\n"
                + snapshot_post_restore_script(
                    processors,
                    teardown_mode=teardown_mode,
                    network_gateway=smp_network_gateway,
                    ioapic_irq=smp_ioapic_irq,
                )
            ).encode("utf-8")
        )
        snapshot_requested = True
        deadline = time.monotonic() + timeout

    try:
        while True:
            peak_bytes = _try_peak_rss(process, peak_bytes)
            observe_snapshot_publication()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"snapshot was not captured within {timeout:g}s")
            try:
                poll_interval = 0.001 if snapshot_requested else 0.25
                chunk = chunks.get(timeout=min(remaining, poll_interval))
            except queue.Empty:
                peak_bytes = _try_peak_rss(process, peak_bytes)
                observe_snapshot_publication()
                if process.poll() is not None and snapshot_published_ns is None:
                    raise RuntimeError(
                        f"OpenVMM exited with status {process.returncode}"
                    ) from None
                continue
            if chunk is None:
                observe_snapshot_publication()
                break
            if profile is not None:
                profile.feed(chunk)
            output.extend(chunk)
            peak_bytes = _try_peak_rss(process, peak_bytes)
            if not boot_seen and BOOT_MARKER in output:
                boot_seen = True
                if processors is None:
                    request_snapshot()
                else:
                    interaction.write_input(
                        smp_probe_script(
                            processors,
                            exit_guest=False,
                            network_gateway=smp_network_gateway,
                            ioapic_irq=smp_ioapic_irq,
                        ).encode("utf-8")
                    )
            if (
                boot_seen
                and not snapshot_requested
                and contains_output_line(output, SMP_PROBE_COMPLETION_MARKER)
            ):
                request_snapshot()
            if snapshot_requested and contains_output_line(output, RESTORE_MARKER):
                raise RuntimeError("source guest continued past the snapshot boundary")
            if len(output) > 1024 * 1024:
                del output[: len(output) - 1024 * 1024]

        returncode = process.wait()
        source_exited_ns = time.perf_counter_ns()
        if snapshot_published_ns is None and snapshot_path.is_dir():
            snapshot_published_ns = source_exited_ns
        if not snapshot_requested:
            raise RuntimeError("source guest exited before its snapshot request")
        if returncode != 0:
            raise RuntimeError(f"snapshot source exited with status {returncode}")
        if not snapshot_path.is_dir():
            raise RuntimeError(f"snapshot was not published at {snapshot_path}")
        assert snapshot_started_ns is not None
        assert snapshot_published_ns is not None
        if profile is not None and profile_sink is not None:
            profile_sink.append(
                profile.finish_capture(
                    snapshot_started_ns,
                    snapshot_published_ns,
                    source_exited_ns,
                )
            )
        request_to_publication_ms = (
            snapshot_published_ns - snapshot_started_ns
        ) / 1_000_000
        return (
            request_to_publication_ms,
            request_to_publication_ms,
            (source_exited_ns - snapshot_published_ns) / 1_000_000,
            peak_bytes,
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
    peak_rss_samples: list[int],
    profile_samples: list[dict[str, object]] | None = None,
) -> SnapshotCaptureResult:
    result: SnapshotCaptureResult = {
        "samples_ms": samples,
        "p50_ms": statistics.median(samples),
        "p95_ms": nearest_rank_percentile(samples, 95),
        "min_ms": min(samples),
        "max_ms": max(samples),
        "request_to_publication_samples_ms": request_to_publication_samples,
        "request_to_publication_p50_ms": statistics.median(
            request_to_publication_samples
        ),
        "request_to_publication_p95_ms": nearest_rank_percentile(
            request_to_publication_samples, 95
        ),
        "request_to_publication_min_ms": min(request_to_publication_samples),
        "request_to_publication_max_ms": max(request_to_publication_samples),
        "post_publication_exit_samples_ms": post_publication_exit_samples,
        "post_publication_exit_p50_ms": statistics.median(
            post_publication_exit_samples
        ),
        "post_publication_exit_p95_ms": nearest_rank_percentile(
            post_publication_exit_samples, 95
        ),
        "post_publication_exit_min_ms": min(post_publication_exit_samples),
        "post_publication_exit_max_ms": max(post_publication_exit_samples),
        "peak_rss_samples_bytes": peak_rss_samples,
        "peak_rss_p50_bytes": int(statistics.median(peak_rss_samples)),
        "peak_rss_min_bytes": min(peak_rss_samples),
        "peak_rss_max_bytes": max(peak_rss_samples),
    }
    if profile_samples is not None:
        result["profile"] = summarize_lifecycle_profiles(profile_samples)
    return result


def benchmark_snapshot_capture(
    args: argparse.Namespace,
    boot_command: Sequence[str],
    *,
    windows_cpus: set[int] | None = None,
    retained_snapshot_path: Path | None = None,
) -> SnapshotCaptureResult:
    snapshot_profile = bool(getattr(args, "snapshot_profile", False))
    samples: list[float] = []
    request_to_publication_samples: list[float] = []
    post_publication_exit_samples: list[float] = []
    peak_rss_samples: list[int] = []
    profile_samples: list[dict[str, object]] = []
    for index in range(args.warmups + args.runs):
        retain = (
            retained_snapshot_path is not None and index == args.warmups + args.runs - 1
        )
        with contextlib.ExitStack() as temporary:
            snapshot_path = (
                retained_snapshot_path
                if retain
                else Path(
                    temporary.enter_context(
                        tempfile.TemporaryDirectory(prefix="openvmm-snapshot-capture-")
                    )
                )
                / "snapshot"
            )
            assert snapshot_path is not None
            value, request_to_publication, post_publication_exit, peak_bytes = (
                capture_snapshot(
                    [*boot_command, "--snapshot-destination", str(snapshot_path)],
                    snapshot_path,
                    timeout=args.timeout,
                    windows_cpus=windows_cpus,
                    processors=args.processors,
                    teardown_mode=args.teardown_mode,
                    snapshot_profile=snapshot_profile,
                    profile_sink=(profile_samples if index >= args.warmups else None),
                )
            )
        if index < args.warmups:
            print(
                f"  warmup {index + 1}/{args.warmups}: {value:.3f} ms, "
                f"peak RSS={bytes_to_mib(peak_bytes):.3f} MiB",
                flush=True,
            )
        else:
            samples.append(value)
            request_to_publication_samples.append(request_to_publication)
            post_publication_exit_samples.append(post_publication_exit)
            peak_rss_samples.append(peak_bytes)
            print(
                f"  sample {index - args.warmups + 1}/{args.runs}: {value:.3f} ms, "
                f"request-to-publication={request_to_publication:.3f} ms, "
                f"post-publication exit={post_publication_exit:.3f} ms, "
                f"peak RSS={bytes_to_mib(peak_bytes):.3f} MiB",
                flush=True,
            )
    return summarize_snapshot_samples(
        samples,
        request_to_publication_samples,
        post_publication_exit_samples,
        peak_rss_samples,
        profile_samples if snapshot_profile else None,
    )


def print_snapshot_summary(backend: str, result: SnapshotCaptureResult) -> None:
    print(
        f"snapshot/{backend}: p50={result['p50_ms']:.3f} ms "
        f"p95={result['p95_ms']:.3f} ms "
        f"min={result['min_ms']:.3f} ms max={result['max_ms']:.3f} ms "
        f"post-publication-exit-p50="
        f"{result['post_publication_exit_p50_ms']:.3f} ms "
        f"peak-rss-p50={bytes_to_mib(result['peak_rss_p50_bytes']):.3f} MiB "
        f"peak-rss-max={bytes_to_mib(result['peak_rss_max_bytes']):.3f} MiB",
        flush=True,
    )


def snapshot_restore_command(
    executable: Path,
    hypervisor: str,
    snapshot_path: Path,
    *,
    processors: int = 1,
    restore_processors: int | None = None,
    network_profile: str | None = None,
) -> list[str]:
    command = [
        str(executable),
        "--single-process",
        "--machine",
        "microvm-v2",
        "--processors",
        str(processors),
        "--hypervisor",
        hypervisor,
        "--restore-snapshot",
        str(snapshot_path),
        "--restore-entropy",
    ]
    if restore_processors is not None:
        command.extend(("--restore-processors", str(restore_processors)))
    if network_profile is not None:
        command.extend(("--network-profile", network_profile))
    return command


def benchmark_snapshot_restore(
    args: argparse.Namespace,
    executable: Path,
    hypervisor: str,
    boot_command: Sequence[str],
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
    snapshot_path: Path | None = None,
) -> BenchmarkResult:
    snapshot_profile = bool(getattr(args, "snapshot_profile", False))
    if snapshot_path is not None:
        return benchmark(
            [
                *command_prefix,
                *snapshot_restore_command(
                    executable,
                    hypervisor,
                    snapshot_path,
                    processors=args.processors,
                    network_profile=args.network_profile,
                ),
            ],
            warmups=args.warmups,
            runs=args.runs,
            timeout=args.timeout,
            marker=RESTORE_MARKER,
            marker_must_be_line=True,
            windows_cpus=windows_cpus,
            teardown_mode=args.teardown_mode,
            guest_exit_prequeued=args.teardown_mode == "guest-exit",
            snapshot_profile=snapshot_profile,
        )

    with tempfile.TemporaryDirectory(prefix="openvmm-e2e-restore-") as temp_dir:
        generated_snapshot_path = Path(temp_dir) / "snapshot"
        capture_snapshot(
            [
                *boot_command,
                "--snapshot-destination",
                str(generated_snapshot_path),
            ],
            generated_snapshot_path,
            timeout=args.timeout,
            windows_cpus=windows_cpus,
            processors=args.processors,
            teardown_mode=args.teardown_mode,
        )
        return benchmark_snapshot_restore(
            args,
            executable,
            hypervisor,
            boot_command,
            command_prefix=command_prefix,
            windows_cpus=windows_cpus,
            snapshot_path=generated_snapshot_path,
        )


def print_summary(backend: str, result: BenchmarkResult) -> None:
    teardown_p50 = result["teardown_p50_ms"]
    teardown_p95 = result["teardown_p95_ms"]
    teardown_max = result["teardown_max_ms"]
    teardown = (
        f"teardown-p50={teardown_p50:.3f} ms "
        f"teardown-p95={teardown_p95:.3f} ms teardown-max={teardown_max:.3f} ms"
        if teardown_p50 is not None
        and teardown_p95 is not None
        and teardown_max is not None
        else "teardown=no-completed-samples"
    )
    print(
        f"{backend}: p50={result['p50_ms']:.3f} ms "
        f"p95={result['p95_ms']:.3f} ms "
        f"min={result['min_ms']:.3f} ms max={result['max_ms']:.3f} ms "
        f"peak-rss-p50={bytes_to_mib(result['peak_rss_p50_bytes']):.3f} MiB "
        f"peak-rss-max={bytes_to_mib(result['peak_rss_max_bytes']):.3f} MiB "
        f"{teardown} teardown-timeouts={result['teardown_timeout_count']}",
        flush=True,
    )


def print_lifecycle_profile_summary(
    label: str, profile: LifecycleProfileSummary
) -> None:
    print(f"{label} lifecycle phases:", flush=True)
    for name, metric in profile["phases"].items():
        print(
            f"  {name}: p50={_profile_float(metric, 'p50_ms'):.3f} ms "
            f"p95={_profile_float(metric, 'p95_ms'):.3f} ms "
            f"n={len(cast(list[float], metric['samples_ms']))} "
            f"exclusive={str(bool(metric['exclusive'])).lower()}",
            flush=True,
        )


def warm_snapshot_artifacts(snapshot_path: Path) -> None:
    for name in SNAPSHOT_FILENAMES:
        with (snapshot_path / name).open("rb", buffering=0) as artifact:
            while artifact.read(4 * 1024 * 1024):
                pass


def drop_linux_snapshot_artifacts(snapshot_path: Path) -> None:
    posix_fadvise = cast(
        Callable[[int, int, int, int], None] | None,
        getattr(os, "posix_fadvise", None),
    )
    dontneed = cast(int | None, getattr(os, "POSIX_FADV_DONTNEED", None))
    if posix_fadvise is None or dontneed is None:
        raise RuntimeError("POSIX_FADV_DONTNEED is unavailable on this Linux host")
    for name in SNAPSHOT_FILENAMES:
        with (snapshot_path / name).open("rb", buffering=0) as artifact:
            posix_fadvise(artifact.fileno(), 0, 0, dontneed)


def copy_windows_snapshot_unbuffered(source: Path, destination: Path) -> None:
    if destination.exists():
        shutil.rmtree(destination)
    completed = subprocess.run(
        [
            "robocopy.exe",
            str(source),
            str(destination),
            "/E",
            "/J",
            "/NFL",
            "/NDL",
            "/NJH",
            "/NJS",
            "/NP",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode >= 8:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise RuntimeError(
            f"unbuffered snapshot copy failed with status {completed.returncode}: "
            f"{detail}"
        )


@contextlib.contextmanager
def prepared_snapshot_cache(
    snapshot_path: Path,
    cache_state: str,
) -> Generator[tuple[Path, Callable[[], None], str]]:
    if cache_state == "warm":
        yield (
            snapshot_path,
            lambda: warm_snapshot_artifacts(snapshot_path),
            "sequential-read",
        )
        return
    if cache_state != "cold":
        raise ValueError(f"unsupported snapshot cache state {cache_state!r}")
    if sys.platform.startswith("linux"):
        yield (
            snapshot_path,
            lambda: drop_linux_snapshot_artifacts(snapshot_path),
            "posix-fadvise-dontneed",
        )
        return
    if os.name == "nt":
        with tempfile.TemporaryDirectory(prefix="openvmm-cold-snapshot-") as temporary:
            cold_path = Path(temporary) / "snapshot"
            yield (
                cold_path,
                lambda: copy_windows_snapshot_unbuffered(snapshot_path, cold_path),
                "robocopy-unbuffered-clone",
            )
        return
    raise RuntimeError(f"cold snapshot cache control is unsupported on {sys.platform}")


def benchmark_snapshot_profile_matrix(
    args: argparse.Namespace,
    executable: Path,
    backend: str,
    make_boot_command: Callable[[int], list[str]],
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> dict[str, dict[str, object]]:
    profile_args_values = vars(args).copy()
    profile_args_values["snapshot_profile"] = True
    profile_args = argparse.Namespace(**profile_args_values)
    cache_states = (
        ("warm", "cold") if args.cache_state == "both" else (args.cache_state,)
    )
    matrix: dict[str, dict[str, object]] = {}
    with tempfile.TemporaryDirectory(prefix="openvmm-snapshot-profile-") as temporary:
        root = Path(temporary)
        for memory_mib in args.shell_memories:
            print(
                f"Profiling {backend} snapshot lifecycle at {memory_mib} MiB",
                flush=True,
            )
            snapshot_path = root / f"snapshot-{memory_mib}-mib"
            boot_command = make_boot_command(memory_mib)
            capture = benchmark_snapshot_capture(
                profile_args,
                boot_command,
                windows_cpus=windows_cpus,
                retained_snapshot_path=snapshot_path,
            )
            print_snapshot_summary(backend, capture)
            if "profile" in capture:
                print_lifecycle_profile_summary(
                    f"snapshot-capture/{backend}/{memory_mib}-mib",
                    capture["profile"],
                )

            restore_results: dict[str, object] = {}
            for cache_state in cache_states:
                with prepared_snapshot_cache(snapshot_path, cache_state) as (
                    restore_path,
                    condition_cache,
                    cache_control,
                ):
                    restore = benchmark(
                        [
                            *command_prefix,
                            *snapshot_restore_command(
                                executable,
                                backend,
                                restore_path,
                                processors=args.processors,
                                network_profile=args.network_profile,
                            ),
                        ],
                        warmups=args.warmups,
                        runs=args.runs,
                        timeout=args.timeout,
                        marker=RESTORE_MARKER,
                        marker_must_be_line=True,
                        windows_cpus=windows_cpus,
                        teardown_mode=args.teardown_mode,
                        guest_exit_prequeued=args.teardown_mode == "guest-exit",
                        snapshot_profile=True,
                        before_each=condition_cache,
                    )
                print_summary(
                    f"snapshot-restore/{backend}/{memory_mib}-mib/{cache_state}",
                    restore,
                )
                restore_profile = restore.get("profile")
                assert restore_profile is not None
                print_lifecycle_profile_summary(
                    f"snapshot-restore/{backend}/{memory_mib}-mib/{cache_state}",
                    restore_profile,
                )
                restore_results[cache_state] = {
                    "cache_control": cache_control,
                    "result": restore,
                }
            matrix[str(memory_mib)] = {
                "memory_mib": memory_mib,
                "capture": capture,
                "restore": restore_results,
            }
    return matrix


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
        print(
            f"  {name}: p50={float(metric['p50_ms']):.3f} ms "
            f"p95={float(metric['p95_ms']):.3f} ms",
            flush=True,
        )
    for name in ("snapshot_publish", "snapshot_verify", "repeat_verify"):
        metric = metrics[name]
        p50_ms = float(metric["p50_ms"])
        throughput = memory_mib / (p50_ms / 1000.0)
        print(
            f"  {name}: p50={p50_ms:.3f} ms "
            f"p95={float(metric['p95_ms']):.3f} ms ({throughput:.1f} MiB/s)",
            flush=True,
        )
    for name in ("cow_map", "repeat_cow_map", "cow_dirty_all"):
        metric = metrics[name]
        p50_ms = float(metric["p50_ms"])
        print(
            f"  {name}: p50={p50_ms:.3f} ms p95={float(metric['p95_ms']):.3f} ms",
            flush=True,
        )


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
            "artifact structure and exact-length validation",
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
    *,
    processors: int = 1,
) -> list[str]:
    command = [
        str(executable),
        "--single-process",
        "--machine",
        "microvm-v2",
        "--processors",
        str(processors),
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
        append_network_arguments(command, network)
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
        "microvm-v2",
        "--processors",
        str(args.processors),
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
        append_network_arguments(boot_command, args.net, args.network_profile)
    if args.suite == "e2e":
        print("Benchmarking OpenVMM/KVM lifecycle", flush=True)
        cold_start = benchmark(
            boot_command,
            warmups=args.warmups,
            runs=args.runs,
            timeout=args.timeout,
            teardown_mode=args.teardown_mode,
        )
        print_summary("kvm", cold_start)
        with tempfile.TemporaryDirectory(prefix="openvmm-e2e-") as temp_dir:
            snapshot_path = Path(temp_dir) / "snapshot"
            snapshot_capture = benchmark_snapshot_capture(
                args,
                boot_command,
                retained_snapshot_path=snapshot_path,
            )
            print_snapshot_summary("kvm", snapshot_capture)
            snapshot_restore = benchmark_snapshot_restore(
                args,
                stage / "openvmm",
                "kvm",
                boot_command,
                command_prefix=("taskset", "-c", args.cpus),
                snapshot_path=snapshot_path,
            )
        print_summary("snapshot-restore/kvm", snapshot_restore)
        e2e_result: KvmE2EResult = {
            "cold_start": cold_start,
            "snapshot_capture": snapshot_capture,
            "snapshot_restore": snapshot_restore,
        }
        print(
            KVM_E2E_RESULT_PREFIX + json.dumps(e2e_result, separators=(",", ":")),
            flush=True,
        )
        return 0
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
    backend: str | None = None,
) -> ResultDocument:
    return {
        "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "controls": {
            "suite": args.suite,
            "warmups": args.warmups,
            "runs": args.runs,
            "memory_mib": args.memory_mib,
            "platform": args.platform,
            "backend": backend or args.backend,
            "microvm_abi_version": MICROVM_ABI_VERSION,
            "processors": args.processors,
            "artifact_revisions": {
                "nvx": _git_revision(args.nvx_dir.resolve()),
                "openvmm": _git_revision(args.openvmm_dir.resolve()),
            },
            "network": args.net,
            "snapshot_artifact_validation": "structural-and-semantic",
            "cold_start_scope": (
                "OpenVMM process launch through guest readiness marker; "
                "teardown excluded and recorded separately"
            ),
            "snapshot_restore_scope": (
                "restored OpenVMM process launch through restored guest marker; "
                "teardown excluded and recorded separately"
            ),
            "peak_rss_scope": (
                "per-process high-water mark; cold and restore samples end at "
                "their guest marker, snapshot generation ends when the source "
                "OpenVMM process exits after publishing the snapshot"
            ),
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
                else "cold start dispatches nvx-exit 0 after readiness; snapshot "
                "restore executes a prequeued nvx-exit 0 immediately after its "
                "marker; both end at successful OpenVMM process exit"
            ),
            "teardown_mode": args.teardown_mode,
            "teardown_timeout_seconds": TEARDOWN_TIMEOUT_SECONDS,
            "cpus": args.cpus,
            "host_cpu_reserve": args.host_cpu_reserve,
            "timeout_seconds": args.timeout,
            "kernel": str(kernel) if kernel is not None else None,
            "initrd": str(initrd) if initrd is not None else None,
            "marker": BOOT_MARKER.decode(),
            "restore_marker": RESTORE_MARKER.decode(),
            "snapshot_profile": bool(
                args.snapshot_profile or args.suite == "snapshot-profile"
            ),
            "snapshot_profile_environment": SNAPSHOT_PROFILE_ENV,
            "cache_state": args.cache_state,
        },
        "backends": {},
        "snapshot_capture": {},
        "snapshot_restore": {},
        "phase2": {},
        "comparison": {},
        "e2e_comparison": {},
        "snapshot_profile_matrix": {},
    }


def run_native_linux(args: argparse.Namespace) -> int:
    apply_benchmark_suite_defaults(args)
    if args.backend not in ("kvm", "mshv"):
        raise ValueError("native Linux benchmark runs support --backend kvm or mshv")

    backend = args.backend

    openvmm_dir = args.openvmm_dir.resolve()
    require_file(openvmm_dir / "Cargo.toml", "OpenVMM Cargo.toml")
    run_boot = args.suite in ("boot", "e2e", "all")
    run_snapshot = args.suite in ("snapshot", "e2e", "all")
    run_restore = args.suite in ("restore", "e2e", "all")
    run_phase2 = args.suite in ("phase2", "all")
    run_profile = args.suite == "snapshot-profile"
    run_workloads = args.suite in WORKLOAD_SUITES
    run_guest = run_boot or run_snapshot or run_restore or run_profile or run_workloads
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
    if run_guest:
        validate_benchmark_cpu_set(cpus, args.processors, args.host_cpu_reserve)
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
    results = result_document(args, kernel, initrd, backend)
    if run_guest:
        assert executable is not None and kernel is not None and initrd is not None

        def make_boot_command(memory_mib: int) -> list[str]:
            command = [
                *prefix,
                str(executable),
                "--single-process",
                "--machine",
                "microvm-v2",
                "--processors",
                str(args.processors),
                "--hypervisor",
                backend,
                "--memory",
                f"{memory_mib}M",
                "--kernel",
                str(kernel),
                "--initrd",
                str(initrd),
                "--cmdline",
                f"{'clocksource=kvm-clock ' if backend == 'kvm' else ''}{BASE_TUNING}",
            ]
            if args.net is not None:
                append_network_arguments(command, args.net, args.network_profile)
            return command

        boot_command = make_boot_command(args.memory_mib)
        if run_profile:
            results["snapshot_profile_matrix"][backend] = (
                benchmark_snapshot_profile_matrix(
                    args,
                    executable,
                    backend,
                    make_boot_command,
                    command_prefix=prefix,
                )
            )
            if args.output is not None:
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(
                    json.dumps(results, indent=2) + "\n", encoding="utf-8"
                )
                print(f"Wrote {args.output}", flush=True)
            return 0
        if run_boot:
            result = benchmark(
                boot_command,
                warmups=args.warmups,
                runs=args.runs,
                timeout=args.timeout,
                teardown_mode=args.teardown_mode,
                snapshot_profile=args.snapshot_profile,
            )
            results["backends"][backend] = result
            print_summary(backend, result)
        with contextlib.ExitStack() as snapshots:
            retained_snapshot_path = (
                Path(
                    snapshots.enter_context(
                        tempfile.TemporaryDirectory(prefix="openvmm-e2e-")
                    )
                )
                / "snapshot"
                if args.suite == "e2e"
                else None
            )
            if run_snapshot:
                result = benchmark_snapshot_capture(
                    args,
                    boot_command,
                    retained_snapshot_path=retained_snapshot_path,
                )
                results["snapshot_capture"][backend] = result
                print_snapshot_summary(backend, result)
            if run_restore:
                result = benchmark_snapshot_restore(
                    args,
                    executable,
                    backend,
                    boot_command,
                    command_prefix=prefix,
                    snapshot_path=retained_snapshot_path,
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
        "--processors",
        str(args.processors),
        "--host-cpu-reserve",
        str(args.host_cpu_reserve),
        "--cpus",
        args.cpus,
        "--timeout",
        str(args.timeout),
        "--teardown-mode",
        args.teardown_mode,
    ]
    if args.net is not None:
        command.extend(("--net", args.net, "--network-profile", args.network_profile))
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


def benchmark_e2e_kvm(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
) -> KvmE2EResult:
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
        "e2e",
        "--warmups",
        str(args.warmups),
        "--runs",
        str(args.runs),
        "--memory-mib",
        str(args.memory_mib),
        "--processors",
        str(args.processors),
        "--host-cpu-reserve",
        str(args.host_cpu_reserve),
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
            if line.startswith(KVM_E2E_RESULT_PREFIX):
                return cast(
                    KvmE2EResult,
                    json.loads(line.removeprefix(KVM_E2E_RESULT_PREFIX)),
                )
        raise RuntimeError("KVM e2e worker did not emit a result")
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
        "--processors",
        str(args.processors),
        "--host-cpu-reserve",
        str(args.host_cpu_reserve),
        "--cpus",
        args.cpus,
        "--timeout",
        str(args.timeout),
        "--teardown-mode",
        args.teardown_mode,
    ]
    if args.net is not None:
        command.extend(("--net", args.net, "--network-profile", args.network_profile))
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
        "--processors",
        str(args.processors),
        "--host-cpu-reserve",
        str(args.host_cpu_reserve),
        "--cpus",
        args.cpus,
        "--timeout",
        str(args.timeout),
    ]
    if args.net is not None:
        command.extend(("--net", args.net, "--network-profile", args.network_profile))
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
    apply_benchmark_suite_defaults(args)
    if (args.net is None) != (args.network_profile is None):
        raise ValueError("--net and --network-profile must be specified together")
    if args._kvm_worker:
        return run_kvm_worker(args)
    if os.name != "nt":
        if sys.platform.startswith("linux"):
            return run_native_linux(args)
        raise RuntimeError(f"benchmark coordinator is unsupported on {sys.platform}")

    openvmm_dir = args.openvmm_dir.resolve()
    require_file(openvmm_dir / "Cargo.toml", "OpenVMM Cargo.toml")
    run_boot = args.suite in ("boot", "e2e", "all")
    run_snapshot = args.suite in ("snapshot", "e2e", "all")
    run_restore = args.suite in ("restore", "e2e", "all")
    run_phase2 = args.suite in ("phase2", "all")
    run_profile = args.suite == "snapshot-profile"
    run_workloads = args.suite in WORKLOAD_SUITES
    if run_workloads and args.backend != "whp":
        raise ValueError(
            "Windows workload suites require --backend whp; run KVM/MSHV "
            "workloads on native Linux"
        )
    if run_profile and args.backend != "whp":
        raise ValueError(
            "Windows snapshot-profile runs require --backend whp; run KVM or "
            "MSHV profiles on a native Linux host"
        )
    run_guest = run_boot or run_snapshot or run_restore or run_profile or run_workloads
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
    if run_guest:
        validate_benchmark_cpu_set(cpus, args.processors, args.host_cpu_reserve)

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
    if run_profile:
        assert kernel is not None and initrd is not None
        executable = boot_binaries["whp"]

        def make_whp_profile_command(memory_mib: int) -> list[str]:
            return whp_command(
                executable,
                kernel,
                initrd,
                memory_mib,
                args.net,
                processors=args.processors,
            )

        results["snapshot_profile_matrix"]["whp"] = benchmark_snapshot_profile_matrix(
            args,
            executable,
            "whp",
            make_whp_profile_command,
            windows_cpus=cpus,
        )
        if args.output:
            output = args.output.resolve()
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
            print(f"Wrote {output}")
        return 0
    kvm_e2e = None
    if args.suite == "e2e" and "kvm" in selected:
        assert kernel is not None and initrd is not None
        kvm_e2e = benchmark_e2e_kvm(
            args,
            boot_binaries["kvm"],
            kernel,
            initrd,
        )

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
                processors=args.processors,
            ),
            warmups=args.warmups,
            runs=args.runs,
            timeout=args.timeout,
            windows_cpus=cpus,
            teardown_mode=args.teardown_mode,
            snapshot_profile=args.snapshot_profile,
        )
        backends["whp"] = whp_result
        print_summary("whp", whp_result)

    if run_boot and "kvm" in selected:
        assert kernel is not None and initrd is not None
        backends["kvm"] = (
            kvm_e2e["cold_start"]
            if kvm_e2e is not None
            else benchmark_kvm(
                args,
                boot_binaries["kvm"],
                kernel,
                initrd,
            )
        )

    snapshot_capture_results = results["snapshot_capture"]
    assert isinstance(snapshot_capture_results, dict)
    snapshot_restore_results = results["snapshot_restore"]
    assert isinstance(snapshot_restore_results, dict)
    with contextlib.ExitStack() as snapshots:
        retained_whp_snapshot = (
            Path(
                snapshots.enter_context(
                    tempfile.TemporaryDirectory(prefix="openvmm-e2e-")
                )
            )
            / "snapshot"
            if args.suite == "e2e" and "whp" in selected
            else None
        )
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
                    processors=args.processors,
                ),
                windows_cpus=cpus,
                retained_snapshot_path=retained_whp_snapshot,
            )
            snapshot_capture_results["whp"] = whp_snapshot
            print_snapshot_summary("whp", whp_snapshot)

        if run_snapshot and "kvm" in selected:
            assert kernel is not None and initrd is not None
            snapshot_capture_results["kvm"] = (
                kvm_e2e["snapshot_capture"]
                if kvm_e2e is not None
                else benchmark_snapshot_kvm(
                    args,
                    boot_binaries["kvm"],
                    kernel,
                    initrd,
                )
            )

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
                    processors=args.processors,
                ),
                windows_cpus=cpus,
                snapshot_path=retained_whp_snapshot,
            )
            snapshot_restore_results["whp"] = whp_restore
            print_summary("snapshot-restore/whp", whp_restore)

        if run_restore and "kvm" in selected:
            assert kernel is not None and initrd is not None
            snapshot_restore_results["kvm"] = (
                kvm_e2e["snapshot_restore"]
                if kvm_e2e is not None
                else benchmark_snapshot_restore_kvm(
                    args,
                    boot_binaries["kvm"],
                    kernel,
                    initrd,
                )
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
