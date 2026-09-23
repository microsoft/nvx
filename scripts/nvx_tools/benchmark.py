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
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Callable, Generator, Sequence
from pathlib import Path
from string import Template
from typing import TextIO, TypedDict, cast

from . import common
from .build_constants import (
    AlpineBuildConstants,
    BuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
)
from .common import sha256_file

BOOT_MARKER = b"ALPINE-MICROVM-BOOT-OK"
RESTORE_MARKER = b"OPENVMM-SNAPSHOT-RESTORE-OK"
TEARDOWN_TIMEOUT_SECONDS = 15.0
PEAK_RSS_SAMPLE_ATTEMPTS = 3
BASE_TUNING = (
    "tsc=reliable no_timer_check random.trust_cpu=on "
    "rcupdate.rcu_expedited=1 nokaslr mitigations=off "
    "cryptomgr.notests quiet loglevel=0"
)
CLOCKSOURCE_CURRENT_PATH = (
    "/sys/devices/system/clocksource/clocksource0/current_clocksource"
)
KVM_RESULT_PREFIX = "OPENVMM_KVM_RESULT="
KVM_E2E_RESULT_PREFIX = "OPENVMM_KVM_E2E_RESULT="
KVM_RESTORE_RESULT_PREFIX = "OPENVMM_KVM_RESTORE_RESULT="
KVM_SNAPSHOT_RESULT_PREFIX = "OPENVMM_KVM_SNAPSHOT_RESULT="
PHASE2_RESULT_PREFIX = "OPENVMM_PHASE2_RESULT="
SNAPSHOT_PROFILE_ENV = "OPENVMM_STARTUP_PROFILE"
SNAPSHOT_PROFILE_PREFIX = b"OPENVMM_SNAPSHOT_PROFILE_V1 "
NVX_SCRIPT = Path(__file__).resolve().parents[1] / "nvx.py"
BENCHMARK_SCRIPTS_DIR = Path(__file__).with_name("benchmark_scripts")
WORKLOAD_SUITES = frozenset(
    {
        "cold-start",
        "device-io",
        "virtfs",
        "shell-snapshot",
        "shell-snapshot-restore",
        "snapshot-restore-memory",
        "snapshot-restore-vcpu",
        "network-snapshot",
        "device-restore-profile",
        "performance",
    }
)
NETWORK_SNAPSHOT_BACKENDS = frozenset({"kvm", "mshv", "whp"})
DEVICE_RESTORE_DEVICES = ("console", "net", "virtiofs")
DEVICE_RESTORE_MODES = ("active", "deferred")
DEVICE_RESTORE_MARKER_PREFIX = "NVX-VIRTIO-RESTORE-PROBE "
VIRTIO_RESTORE_TRACE_MESSAGE = "virtio restore lifecycle"
ANSI_ESCAPE_PATTERN = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
DEVICE_IO_BACKENDS = frozenset({"kvm", "mshv", "whp"})
DEVICE_IO_DEVICES = ("virtio-blk", "virtio-fs", "virtio-net")
DEVICE_IO_OPERATIONS = {
    "virtio-blk": ("read", "write"),
    "virtio-fs": ("read", "write"),
    "virtio-net": ("roundtrip",),
}
DEVICE_IO_GUEST_RESULT_PREFIX = "NVX_DEVICE_IO_GUEST_RESULT="
DEVICE_IO_RESULT_PREFIX = "NVX_DEVICE_IO_RESULT="
DEVICE_IO_COMPLETION_MARKER = b"NVX-DEVICE-IO-COMPLETE"
DEVICE_IO_LOG_FILENAME = "device-io.log"
DEVICE_IO_MEMORY_MIB = 256
DEVICE_IO_SIZE_MIB = 512
DEVICE_IO_DURATION_SECONDS = 10.0
DEVICE_IO_WARMUPS = 5
DEVICE_IO_RUNS = 30
DEVICE_IO_PORT = 5201
DD_RATE_PATTERN = re.compile(r"([0-9.]+)\s*([KMG]?)B/s")
VIRTFS_COMPLETION_MARKER = b"NVX-VIRTFS-WORKLOAD-COMPLETE"
VIRTFS_ROUNDTRIP_MARKER = b"VIRTFS-LIVE-ROUNDTRIP-OK"
VIRTFS_GUEST_TO_HOST = b"guest-to-host\n"
VIRTFS_HOST_WAITING = b"waiting\n"
VIRTFS_HOST_TO_GUEST = b"host-to-guest\n"
SMP_PROBE_COMPLETION_MARKER = b"NVX-SMP-PROBE-OK"
SMP_PROBE_PATH = "/tmp/nvx-smp-probe"
SNAPSHOT_CAPTURE_PATH = "/tmp/nvx-c"
SNAPSHOT_POST_RESTORE_PATH = "/tmp/nvx-post-restore"
SNAPSHOT_GUEST_DISPATCH_MARKER = b"NVX-SNAPSHOT-DISPATCHED"
SNAPSHOT_CAPTURE_TIMING = "openvmm-input-gate-to-publication"
SNAPSHOT_FILENAMES = ("manifest.bin", "state.bin", "memory.bin")
SHELL_SNAPSHOT_MEMORY_MIB = (128, 256, 512)
SNAPSHOT_PROFILE_MEMORY_MIB = (*SHELL_SNAPSHOT_MEMORY_MIB, 1024)
RESTORE_VCPU_TARGETS = (1, 2, 4, 8)
RESTORE_MEMORY_BASE_MIB = 512
RESTORE_MEMORY_TARGETS_MIB = (512, 1024, 2048)
RESTORE_MEMORY_MARKER_PATTERN = re.compile(
    rb"NVX-MEMORY-ONLINE-OK: added_bytes=(\d+) memtotal_kib=(\d+) elapsed_us=(\d+)"
)
PERFORMANCE_LOG_FILENAMES = (
    "cold-start.log",
    "virtfs.log",
    "shell-snapshot.log",
    "shell-snapshot-restore.log",
    "network.log",
)
LEGACY_PYTHON_LOG_FILENAMES = ("snapshot.log", "snapshot-hello.log")
BENCHMARK_METADATA_FILENAME = "benchmark-metadata.json"


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
    peak_rss_remeasured_count: int
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


class DeviceRestoreSample(TypedDict):
    process_launch_to_ready_ms: float
    trigger_to_first_successful_io_ms: float
    peak_rss_bytes: int
    guest_markers: list[dict[str, str]]
    events: list[dict[str, object]]
    queue_start_count: int
    staged_kick_dispatch_count: int
    stale_premature_callback_count: int
    log: str


class DeviceRestoreMetric(TypedDict):
    samples: list[float]
    p50: float
    p95: float


DEVICE_RESTORE_TYPE_IDS = {"console": 3, "net": 1, "virtiofs": 26}
DEVICE_RESTORE_QUEUE_COUNTS = {"console": 2, "net": 2, "virtiofs": 2}


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
    parser.add_argument(
        "--warmups",
        type=nonnegative_int,
        default=None,
        help=("excluded warmup attempts (default: 5 for device-io, 3 otherwise)"),
    )
    parser.add_argument(
        "--runs",
        type=positive_int,
        default=None,
        help=("retained attempts (default: 30 for device-io, 11 otherwise)"),
    )
    parser.add_argument("--memory-mib", type=positive_int, default=128)
    parser.add_argument(
        "--processors",
        type=int,
        choices=(1, 2, 4, 8),
        default=1,
        help="microVM processor count (default: 1)",
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
            "snapshot memory sizes (default: 128 256 512, or "
            "128 256 512 1024 for --suite snapshot-profile)"
        ),
    )
    parser.add_argument(
        "--network-memory-mib",
        type=positive_int,
        default=256,
        help="guest memory for the network snapshot workload (default: 256)",
    )
    parser.add_argument(
        "--restore-devices",
        choices=DEVICE_RESTORE_DEVICES,
        nargs="+",
        default=list(DEVICE_RESTORE_DEVICES),
        metavar="DEVICE",
        help="devices for --suite device-restore-profile",
    )
    parser.add_argument(
        "--restore-modes",
        choices=DEVICE_RESTORE_MODES,
        nargs="+",
        default=list(DEVICE_RESTORE_MODES),
        metavar="MODE",
        help="activation modes for --suite device-restore-profile",
    )
    parser.add_argument(
        "--device-io-duration-seconds",
        type=positive_float,
        default=DEVICE_IO_DURATION_SECONDS,
        help="measurement window for each device operation (default: 10)",
    )
    parser.add_argument(
        "--device-io-size-mib",
        type=positive_int,
        default=DEVICE_IO_SIZE_MIB,
        help="virtio-blk and virtio-fs backing-object size (default: 512)",
    )
    parser.add_argument(
        "--device-io-port",
        type=network_port,
        default=DEVICE_IO_PORT,
        help="same-host UDP echo port (default: 5201)",
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
        "--scratch-dir",
        type=Path,
        help=(
            "existing directory for temporary snapshots, guest RAM backing, "
            "and workload files (default: the system temporary directory)"
        ),
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
    if args.warmups is None:
        if args.suite == "device-restore-profile":
            args.warmups = 1
        elif args.suite == "device-io":
            args.warmups = DEVICE_IO_WARMUPS
        else:
            args.warmups = 3
    if args.runs is None:
        if args.suite == "device-restore-profile":
            args.runs = 5
        elif args.suite == "device-io":
            args.runs = DEVICE_IO_RUNS
        else:
            args.runs = 11
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


def network_port(value: str) -> int:
    parsed = int(value)
    if not 1 <= parsed <= 65535:
        raise argparse.ArgumentTypeError("must be in 1..65535")
    return parsed


def run_checked(command: Sequence[str], *, cwd: Path | None = None) -> None:
    print("+", subprocess.list2cmdline(list(command)), flush=True)
    subprocess.run(command, cwd=cwd, check=True)


def _read_benchmark_script(name: str) -> str:
    script = (BENCHMARK_SCRIPTS_DIR / name).read_text(encoding="utf-8")
    return script if script.endswith("\n") else script + "\n"


class _BenchmarkScriptTemplate(Template):
    delimiter = "@"


def _render_benchmark_script(name: str, **values: str) -> str:
    return _BenchmarkScriptTemplate(_read_benchmark_script(name)).substitute(values)


def append_network_arguments(
    command: list[str],
    network: str,
    profile: str = "portable",
) -> None:
    command.extend(("--net", network, "--network-profile", profile))


def require_file(path: Path, description: str) -> Path:
    path = path.resolve()
    try:
        return common.require_file(path, description)
    except common.ScriptError as error:
        raise FileNotFoundError(str(error)) from error


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


def _linux_live_peak_rss_bytes(pid: int) -> int | None:
    # The unreaped child keeps its PID, so this entry cannot be reused.
    try:
        status = Path(f"/proc/{pid}/status").read_text(encoding="ascii")
    except (FileNotFoundError, ProcessLookupError):
        return None
    return _linux_status_bytes(status, "VmHWM")


def live_peak_rss_bytes(process: subprocess.Popen[bytes]) -> int | None:
    """Return the peak RSS of a still-running process, or None after it exits.

    Exit accounting is not an equivalent sample. Linux stops publishing VmHWM
    once the process releases its address space, and the wait4() maximum RSS
    also covers the coordinator's pre-exec image. Windows retains a terminated
    process's counters, but its peak working set then includes teardown.
    """
    if os.name == "nt":
        try:
            peak = windows_peak_rss_bytes(process.pid)
        except OSError:
            if process.poll() is not None:
                return None
            raise
        # The retained Popen handle keeps a terminated process's counters
        # readable, so accept only a sample completed while it was running.
        if process.poll() is not None:
            return None
    elif sys.platform.startswith("linux"):
        linux_peak = _linux_live_peak_rss_bytes(process.pid)
        if linux_peak is None:
            return None
        peak = linux_peak
    else:
        raise RuntimeError(f"peak RSS measurement is unsupported on {sys.platform}")
    if peak <= 0:
        raise RuntimeError(f"process {process.pid} reported peak RSS {peak} bytes")
    return peak


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


def snapshot_generation_duration_ns(
    records: Sequence[dict[str, object]], pid: int
) -> int:
    phases: dict[str, dict[str, object]] = {}
    for phase in ("input_gate", "publication_commit"):
        matches = [
            record
            for record in records
            if record.get("operation") == "capture" and record.get("phase") == phase
        ]
        if len(matches) != 1:
            raise ValueError(
                f"snapshot capture requires exactly one {phase!r} profile record; "
                f"observed {len(matches)}"
            )
        record = matches[0]
        if _profile_int(record, "pid") != pid:
            raise ValueError("snapshot capture profile came from a different process")
        phases[phase] = record

    input_gate = phases["input_gate"]
    publication = phases["publication_commit"]
    gate_end = _profile_int(input_gate, "process_elapsed_ns")
    gate_duration = _profile_int(input_gate, "duration_ns")
    publication_end = _profile_int(publication, "process_elapsed_ns")
    publication_duration = _profile_int(publication, "duration_ns")
    started = gate_end - gate_duration
    if (
        min(started, gate_duration, publication_duration) < 0
        or publication_end - publication_duration < gate_end
        or publication_end <= started
    ):
        raise ValueError("snapshot capture profile clock is not monotonic")
    return publication_end - started


class SnapshotProfileCollector:
    def __init__(
        self,
        pid: int,
        process_started_ns: int,
        *,
        collect_host_counters: bool = True,
    ) -> None:
        self.pid = pid
        self.process_started_ns = process_started_ns
        self.collect_host_counters = collect_host_counters
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
        if self.collect_host_counters:
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
        logical_bytes: int | None = None,
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
        if logical_bytes is not None:
            record["logical_bytes"] = logical_bytes
        if self.collect_host_counters:
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
        snapshot_dispatched_ns: int,
        snapshot_guest_dispatched_ns: int,
        snapshot_published_ns: int,
        source_exited_ns: int,
        snapshot_request_bytes: int,
    ) -> dict[str, object]:
        self._finish_pending()
        generation_ns = snapshot_generation_duration_ns(self.records, self.pid)
        publication = next(
            record
            for record in self.records
            if record.get("operation") == "capture"
            and record.get("phase") == "publication_commit"
        )
        self.records.append(
            {
                "operation": "capture",
                "phase": "snapshot_generation",
                "exclusive": False,
                "duration_ns": generation_ns,
                "process_elapsed_ns": _profile_int(publication, "process_elapsed_ns"),
                "pid": self.pid,
                "source": "openvmm_clock",
            }
        )
        self.records.append(
            self._external_record(
                "capture",
                "console_input_dispatch",
                snapshot_dispatched_ns - snapshot_started_ns,
                snapshot_dispatched_ns,
                exclusive=True,
                logical_bytes=snapshot_request_bytes,
            )
        )
        self.records.extend(
            (
                self._external_record(
                    "capture",
                    "console_command_round_trip",
                    snapshot_guest_dispatched_ns - snapshot_started_ns,
                    snapshot_guest_dispatched_ns,
                    exclusive=False,
                ),
                self._external_record(
                    "capture",
                    "guest_dispatch_to_publication",
                    snapshot_published_ns - snapshot_guest_dispatched_ns,
                    snapshot_published_ns,
                    exclusive=False,
                ),
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
        return {"records": self.records, "generation_duration_ns": generation_ns}


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
    if process.returncode is not None:
        return process.returncode
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


def parse_device_restore_marker(line: str) -> dict[str, str] | None:
    line = ANSI_ESCAPE_PATTERN.sub("", line).removesuffix("\r")
    if not line.startswith(DEVICE_RESTORE_MARKER_PREFIX):
        return None
    fields: dict[str, str] = {}
    for token in line.removeprefix(DEVICE_RESTORE_MARKER_PREFIX).split():
        name, separator, value = token.partition("=")
        if not separator or not name or not value:
            raise ValueError(f"invalid device restore marker field {token!r}")
        fields[name] = value
    missing = {"phase", "device", "mode"}.difference(fields)
    if missing:
        raise ValueError(f"device restore marker is missing {sorted(missing)}")
    return fields


def _restore_trace_field(line: str, name: str) -> str:
    match = re.search(rf'\b{re.escape(name)}(?:=|:\s*)(?:"([^"]*)"|([^,\s]+))', line)
    if match is None:
        raise ValueError(f"virtio restore event is missing {name!r}")
    return match.group(1) if match.group(1) is not None else match.group(2)


def parse_virtio_restore_event(line: str) -> dict[str, object] | None:
    line = ANSI_ESCAPE_PATTERN.sub("", line)
    if VIRTIO_RESTORE_TRACE_MESSAGE not in line:
        return None
    event: dict[str, object] = {
        "event": _restore_trace_field(line, "event"),
        "device_type": int(_restore_trace_field(line, "device_type"), 0),
        "trigger": _restore_trace_field(line, "trigger"),
        "queue_index": int(_restore_trace_field(line, "queue_index")),
        "restored_progress": _restore_trace_field(line, "restored_progress") == "true",
        "success": _restore_trace_field(line, "success") == "true",
    }
    return event


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
        try:
            record_adversarial_openvmm_pid(self.process.pid, environment)
        except BaseException:
            terminate(self.process)
            self.close()
            raise

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


def record_adversarial_openvmm_pid(
    pid: int,
    environment: dict[str, str],
) -> None:
    pid_journal = environment.get("NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL")
    if pid_journal is None:
        return
    with Path(pid_journal).open("a", encoding="utf-8", newline="\n") as stream:
        record = json.dumps(
            {"pid": pid, "recorded_at_ns": time.time_ns()},
            sort_keys=True,
            separators=(",", ":"),
        )
        stream.write(f"{record}\n")
        stream.flush()
        os.fsync(stream.fileno())


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
    log_path: Path | None = None,
) -> tuple[float, int | None, float | None, float]:
    """Measure one OpenVMM launch through ``marker`` and its teardown.

    Peak RSS is None when OpenVMM exited before it could be sampled at the
    marker, which a prequeued guest exit makes possible.
    """
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
                # opt-in profile counters.
                peak_bytes = live_peak_rss_bytes(process)
                if profile is not None and profile_sink is not None:
                    profile_sink.append(profile.finish_restore(marker_reached))
                elapsed_ms = (marker_reached - started) / 1_000_000
                teardown_started = (
                    marker_reached
                    if teardown_mode == "guest-exit" and guest_exit_prequeued
                    else time.perf_counter_ns()
                )
                if teardown_mode != "guest-exit":
                    process.terminate()
                elif not guest_exit_prequeued:
                    interaction.write_input(
                        _read_benchmark_script("guest-exit.sh").encode("utf-8")
                    )
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
        if log_path is not None:
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_bytes(output)
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

    def measure(
        profile_sink: list[dict[str, object]] | None = None,
    ) -> tuple[float, int | None, float | None, float]:
        if before_each is not None:
            before_each()
        return measure_once(
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
            profile_sink=profile_sink,
        )

    for index in range(warmups):
        value, peak_bytes, teardown_ms, _wall_ms = measure()
        peak_rss = (
            "unavailable"
            if peak_bytes is None
            else f"{bytes_to_mib(peak_bytes):.3f} MiB"
        )
        teardown = (
            f"{teardown_ms:.3f} ms"
            if teardown_ms is not None
            else f"TIMEOUT (>{TEARDOWN_TIMEOUT_SECONDS:g}s)"
        )
        print(
            f"  warmup {index + 1}/{warmups}: {value:.3f} ms, "
            f"peak RSS={peak_rss}, teardown={teardown}",
            flush=True,
        )

    samples: list[float] = []
    wall_samples: list[float] = []
    peak_rss_samples: list[int] = []
    teardown_samples: list[float | None] = []
    profile_samples: list[dict[str, object]] = []
    remeasured = 0

    def measure_sample(index: int) -> tuple[float, int, float | None, float]:
        # An attempt without its marker-time RSS is incomplete. Remeasure it
        # instead of substituting a value from after the marker.
        nonlocal remeasured
        for attempt in range(1, PEAK_RSS_SAMPLE_ATTEMPTS + 1):
            attempt_profiles: list[dict[str, object]] = []
            value, peak_bytes, teardown_ms, wall_ms = measure(attempt_profiles)
            if peak_bytes is not None:
                profile_samples.extend(attempt_profiles)
                remeasured += attempt - 1
                return value, peak_bytes, teardown_ms, wall_ms
            print(
                f"  sample {index + 1}/{runs}: discarded attempt "
                f"{attempt}/{PEAK_RSS_SAMPLE_ATTEMPTS} ({value:.3f} ms); "
                "OpenVMM exited before its peak RSS was sampled at the marker",
                flush=True,
            )
        raise RuntimeError(
            f"OpenVMM exited before its peak RSS was sampled at the marker in "
            f"{PEAK_RSS_SAMPLE_ATTEMPTS} consecutive attempts for sample "
            f"{index + 1}/{runs}"
        )

    for index in range(runs):
        value, peak_bytes, teardown_ms, wall_ms = measure_sample(index)
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
        "peak_rss_remeasured_count": remeasured,
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


def whp_stable_clocksource_wait_script() -> str:
    return "\n".join(
        (
            f"clock_path={CLOCKSOURCE_CURRENT_PATH}",
            "clock_tries=0",
            'while [ "$(cat "$clock_path")" = tsc-early ] '
            '&& [ "$clock_tries" -lt 100 ]; do',
            "    sleep 0.05",
            "    clock_tries=$((clock_tries + 1))",
            "done",
        )
    )


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
    virtio_blk: Path | None = None,
    microvm_sandbox_block: Path | None = None,
) -> list[str]:
    command = [
        *command_prefix,
        str(executable),
        "--single-process",
        "--machine",
        "microvm",
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
    if virtio_blk is not None:
        command.extend(("--virtio-blk", f"file:{virtio_blk}"))
    if microvm_sandbox_block is not None:
        command.extend(
            (
                "--microvm-sandbox-block",
                f"scratch:file:{microvm_sandbox_block}",
            )
        )
    return command


def device_restore_commands(
    executable: Path,
    backend: str,
    kernel: Path,
    initrd: Path,
    memory_mib: int,
    device: str,
    mode: str,
    *,
    command_prefix: Sequence[str] = (),
    network: str = "10.0.0.2/24",
    host_directory: Path | None = None,
    snapshot_path: Path = Path("SNAPSHOT_PATH"),
) -> tuple[list[str], list[str]]:
    if device not in DEVICE_RESTORE_DEVICES:
        raise ValueError(f"unsupported device restore profile device {device!r}")
    if mode not in DEVICE_RESTORE_MODES:
        raise ValueError(f"unsupported device restore profile mode {mode!r}")
    mount = None
    if device == "virtiofs":
        if host_directory is None:
            raise ValueError("virtiofs restore profiling requires a host directory")
        if "," in str(host_directory):
            raise ValueError(
                "virtiofs restore profile directory must not contain a comma"
            )
        mount = f"/mnt/host,{host_directory},rw"
    boot = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        f"quiet loglevel=0 nvx_virtio_restore_probe={device},{mode}",
        processors=1,
        command_prefix=command_prefix,
        network=network if device == "net" else None,
        mount=mount,
    )
    restore = [
        *command_prefix,
        *snapshot_restore_command(
            executable,
            backend,
            snapshot_path,
            processors=1,
            network_profile="portable" if device == "net" else None,
        ),
    ]
    if device == "console":
        boot.extend(("--virtio-console", "console"))
        restore.extend(("--virtio-console", "console"))
    elif mount is not None:
        restore.extend(("--mount", mount))
    return boot, restore


def _device_restore_metric(samples: Sequence[float]) -> DeviceRestoreMetric:
    if not samples:
        raise ValueError("device restore metric requires at least one sample")
    values = list(samples)
    return {
        "samples": values,
        "p50": statistics.median(values),
        "p95": nearest_rank_percentile(values, 95),
    }


def validate_device_restore_sample(
    sample: DeviceRestoreSample,
    device: str,
    mode: str,
) -> None:
    events = [
        event
        for event in sample["events"]
        if event.get("device_type") == DEVICE_RESTORE_TYPE_IDS[device]
    ]
    stages = [event for event in events if event.get("event") == "restore_staged"]
    applications = [
        event
        for event in events
        if event.get("event") == "private_state_apply" and event.get("success") is True
    ]
    queue_starts = [
        event
        for event in events
        if event.get("event") == "queue_start" and event.get("success") is True
    ]
    staged_kick_dispatches = [
        event
        for event in events
        if event.get("event") == "kick_dispatch"
        and event.get("trigger") == "driver-ok"
        and event.get("success") is True
    ]
    staged_kicks = [
        event
        for event in events
        if event.get("event") == "kick_staged" and event.get("success") is True
    ]
    if len(stages) != 1:
        raise RuntimeError(
            f"{device}/{mode} emitted {len(stages)} restore-stage events"
        )
    if len(applications) != 1:
        raise RuntimeError(
            f"{device}/{mode} applied private state {len(applications)} times"
        )
    expected_queues = DEVICE_RESTORE_QUEUE_COUNTS[device]
    if len(queue_starts) != expected_queues:
        raise RuntimeError(
            f"{device}/{mode} started {len(queue_starts)} queues, expected {expected_queues}"
        )
    failed_events = sum(event.get("success") is False for event in events)
    premature = 0
    trigger_markers = [
        marker for marker in sample["guest_markers"] if marker.get("phase") == "trigger"
    ]
    ready_markers = [
        marker
        for marker in sample["guest_markers"]
        if marker.get("phase") == "restore-ready"
    ]
    if len(trigger_markers) != 1:
        raise RuntimeError(f"{device}/{mode} did not emit exactly one trigger marker")
    if len(ready_markers) != 1:
        raise RuntimeError(f"{device}/{mode} did not emit exactly one ready marker")
    trigger_ms = float(trigger_markers[0]["observer_elapsed_ms"])
    trigger_sequence = trigger_markers[0].get("observer_sequence")
    ready_sequence = ready_markers[0].get("observer_sequence")
    stage_sequence = stages[0].get("observer_sequence")
    if (
        ready_sequence is not None
        and isinstance(stage_sequence, int)
        and stage_sequence >= int(ready_sequence)
    ):
        raise RuntimeError(f"{device}/{mode} was staged after vCPU restore readiness")
    if mode == "deferred":
        for event in (
            *applications,
            *staged_kicks,
            *queue_starts,
            *staged_kick_dispatches,
        ):
            event_sequence = event.get("observer_sequence")
            observed = event.get("observer_elapsed_ms")
            if trigger_sequence is not None and isinstance(event_sequence, int):
                is_premature = event_sequence < int(trigger_sequence)
            else:
                is_premature = (
                    isinstance(observed, (int, float)) and observed < trigger_ms
                )
            if is_premature:
                premature += 1
        if stages[0].get("trigger") != "inactive-start":
            raise RuntimeError(f"{device}/deferred was not staged as inactive")
        if applications[0].get("trigger") != "kick":
            raise RuntimeError(f"{device}/deferred was not applied by its staged kick")
        if len(staged_kicks) != 1 or len(staged_kick_dispatches) != 1:
            raise RuntimeError(
                f"{device}/deferred staged {len(staged_kicks)} kicks and "
                f"dispatched {len(staged_kick_dispatches)}"
            )
        dispatch_sequence = staged_kick_dispatches[0].get("observer_sequence")
        last_queue_sequence = queue_starts[-1].get("observer_sequence")
        if (
            isinstance(dispatch_sequence, int)
            and isinstance(last_queue_sequence, int)
            and dispatch_sequence <= last_queue_sequence
        ):
            raise RuntimeError(
                f"{device}/deferred dispatched its kick before queue start"
            )
    else:
        if applications[0].get("trigger") != "active-start":
            raise RuntimeError(f"{device}/active did not restore during active start")
        if any(event.get("restored_progress") is not True for event in queue_starts):
            raise RuntimeError(f"{device}/active queue progress was not restored")
        if ready_sequence is not None and any(
            not isinstance(event.get("observer_sequence"), int)
            or cast(int, event["observer_sequence"]) >= int(ready_sequence)
            for event in (*applications, *queue_starts)
        ):
            raise RuntimeError(
                f"{device}/active activation completed after vCPU readiness"
            )
        if staged_kicks or staged_kick_dispatches:
            raise RuntimeError(f"{device}/active unexpectedly staged a queue kick")
    stale_premature = failed_events + premature
    sample["queue_start_count"] = len(queue_starts)
    sample["staged_kick_dispatch_count"] = len(staged_kick_dispatches)
    sample["stale_premature_callback_count"] = stale_premature
    if stale_premature != 0:
        raise RuntimeError(
            f"{device}/{mode} observed {stale_premature} failed or premature events"
        )


def summarize_device_restore_samples(
    device: str,
    mode: str,
    samples: list[DeviceRestoreSample],
) -> dict[str, object]:
    if not samples:
        raise ValueError("device restore scenario requires measured samples")
    for sample in samples:
        validate_device_restore_sample(sample, device, mode)
    return {
        "device": device,
        "mode": mode,
        "process_launch_to_ready_ms": _device_restore_metric(
            [sample["process_launch_to_ready_ms"] for sample in samples]
        ),
        "trigger_to_first_successful_io_ms": _device_restore_metric(
            [sample["trigger_to_first_successful_io_ms"] for sample in samples]
        ),
        "peak_rss_bytes": _device_restore_metric(
            [float(sample["peak_rss_bytes"]) for sample in samples]
        ),
        "queue_start_counts": [sample["queue_start_count"] for sample in samples],
        "staged_kick_dispatch_counts": [
            sample["staged_kick_dispatch_count"] for sample in samples
        ],
        "stale_premature_callback_count": sum(
            sample["stale_premature_callback_count"] for sample in samples
        ),
        "samples": samples,
        "passed": True,
    }


def _try_peak_rss(process: subprocess.Popen[bytes], current: int) -> int:
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
    log_path: Path | None = None,
    boot_marker: bytes = BOOT_MARKER,
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
            if not input_sent and boot_marker in output:
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
    except BaseException as error:
        terminate(process)
        if not isinstance(error, Exception):
            raise
        tail = output[-4096:].decode("utf-8", "replace")
        if tail:
            raise RuntimeError(f"{error}\n--- OpenVMM output ---\n{tail}") from error
        raise
    finally:
        if log_path is not None:
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_bytes(output)
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


def _device_restore_markers(
    text: str,
    device: str,
    mode: str,
) -> list[dict[str, str]]:
    markers: list[dict[str, str]] = []
    for line in text.splitlines():
        marker = parse_device_restore_marker(line)
        if marker is None:
            continue
        if marker["device"] != device or marker["mode"] != mode:
            raise RuntimeError(
                f"unexpected device restore marker for "
                f"{marker['device']}/{marker['mode']}"
            )
        markers.append(marker)
    return markers


def capture_device_restore_snapshot(
    command: Sequence[str],
    snapshot_path: Path,
    device: str,
    mode: str,
    *,
    timeout: float,
    log_path: Path,
    windows_cpus: set[int] | None = None,
) -> list[dict[str, str]]:
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
                raise TimeoutError(
                    f"{device}/{mode} source snapshot did not finish within {timeout:g}s"
                )
            try:
                chunk = chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                if process.poll() is not None:
                    continue
                continue
            if chunk is None:
                break
            output.extend(chunk)
        returncode = process.wait()
        if returncode != 0:
            raise RuntimeError(
                f"{device}/{mode} snapshot source exited with status {returncode}"
            )
        markers = _device_restore_markers(
            output.decode("utf-8", "replace"), device, mode
        )
        phases = {marker["phase"] for marker in markers}
        required = {"located", "pre-io-success", "capture-ready"}
        if missing := required.difference(phases):
            raise RuntimeError(
                f"{device}/{mode} snapshot source is missing markers {sorted(missing)}"
            )
        for filename in SNAPSHOT_FILENAMES:
            if not (snapshot_path / filename).is_file():
                raise RuntimeError(
                    f"{device}/{mode} snapshot did not publish "
                    f"{snapshot_path / filename}"
                )
        return markers
    except Exception as error:
        terminate(process)
        tail = output[-4096:].decode("utf-8", "replace")
        if tail:
            raise RuntimeError(f"{error}\n--- OpenVMM output ---\n{tail}") from error
        raise
    finally:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.write_bytes(output)
        interaction.close()


def run_device_restore_sample(
    command: Sequence[str],
    device: str,
    mode: str,
    *,
    timeout: float,
    log_path: Path,
    windows_cpus: set[int] | None = None,
) -> DeviceRestoreSample:
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off,virtio_restore=debug"
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
    pending = bytearray()
    markers: list[dict[str, str]] = []
    events: list[dict[str, object]] = []
    phase_times: dict[str, int] = {}
    sequence = 0
    peak_bytes = 0

    def observe_line(raw_line: bytes, observed_ns: int) -> None:
        nonlocal sequence
        sequence += 1
        line = raw_line.decode("utf-8", "replace").removesuffix("\r")
        elapsed_ms = (observed_ns - started_ns) / 1_000_000
        marker = parse_device_restore_marker(line)
        if marker is not None:
            if marker["device"] != device or marker["mode"] != mode:
                raise RuntimeError(
                    f"unexpected device restore marker for "
                    f"{marker['device']}/{marker['mode']}"
                )
            marker["observer_elapsed_ms"] = f"{elapsed_ms:.6f}"
            marker["observer_sequence"] = str(sequence)
            markers.append(marker)
            phase_times.setdefault(marker["phase"], observed_ns)
            if marker["phase"] == "failure":
                raise RuntimeError(
                    f"{device}/{mode} guest probe failed: "
                    f"{marker.get('reason', 'unknown')}"
                )
        event = parse_virtio_restore_event(line)
        if event is not None:
            event["observer_elapsed_ms"] = elapsed_ms
            event["observer_sequence"] = sequence
            events.append(event)

    def observe_chunk(chunk: bytes, observed_ns: int) -> None:
        pending.extend(chunk)
        while b"\n" in pending:
            raw_line, _, remaining = pending.partition(b"\n")
            pending.clear()
            pending.extend(remaining)
            observe_line(bytes(raw_line), observed_ns)

    try:
        while True:
            peak_bytes = _try_peak_rss(process, peak_bytes)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"{device}/{mode} restore did not finish within {timeout:g}s"
                )
            try:
                chunk = chunks.get(timeout=min(remaining, 0.05))
            except queue.Empty:
                if process.poll() is not None:
                    continue
                continue
            if chunk is None:
                break
            observed_ns = time.perf_counter_ns()
            output.extend(chunk)
            observe_chunk(chunk, observed_ns)
        if pending:
            observe_line(bytes(pending), time.perf_counter_ns())
            pending.clear()
        returncode = process.wait()
        if returncode != 0:
            raise RuntimeError(
                f"{device}/{mode} restore exited with status {returncode}"
            )
        for phase in ("restore-ready", "trigger", "io-success"):
            if phase not in phase_times:
                raise RuntimeError(f"{device}/{mode} restore is missing {phase!r}")
        if device == "console" and not contains_output_line(
            output, b"NVX-VIRTIO-CONSOLE-IO-OK"
        ):
            raise RuntimeError(
                "console restore did not emit its virtio-console I/O marker"
            )
        ready_ns = phase_times["restore-ready"]
        trigger_ns = phase_times["trigger"]
        io_ns = phase_times["io-success"]
        if io_ns < trigger_ns:
            raise RuntimeError(f"{device}/{mode} I/O completed before its trigger")
        sample: DeviceRestoreSample = {
            "process_launch_to_ready_ms": (ready_ns - started_ns) / 1_000_000,
            "trigger_to_first_successful_io_ms": (io_ns - trigger_ns) / 1_000_000,
            "peak_rss_bytes": peak_bytes,
            "guest_markers": markers,
            "events": events,
            "queue_start_count": 0,
            "staged_kick_dispatch_count": 0,
            "stale_premature_callback_count": 0,
            "log": str(log_path),
        }
        validate_device_restore_sample(sample, device, mode)
        return sample
    except Exception as error:
        terminate(process)
        tail = output[-4096:].decode("utf-8", "replace")
        if tail:
            raise RuntimeError(f"{error}\n--- OpenVMM output ---\n{tail}") from error
        raise
    finally:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.write_bytes(output)
        interaction.close()


def benchmark_device_restore_profile(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    output_dir: Path,
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> Path:
    if args.processors != 1:
        raise ValueError("device-restore-profile requires exactly one vCPU")
    logs_root = output_dir / "device-restore-profile-logs"
    if logs_root.exists():
        raise FileExistsError(
            f"device restore profile output already exists: {logs_root}"
        )
    logs_root.mkdir(parents=True)
    scenarios: dict[str, object] = {}
    network = args.net or "10.0.0.2/24"
    timeout = max(args.timeout, 60.0)
    for device in args.restore_devices:
        for mode in args.restore_modes:
            scenario_name = f"{device}-{mode}"
            scenario_logs = logs_root / scenario_name
            scenario_logs.mkdir()
            with tempfile.TemporaryDirectory(
                prefix=f"openvmm-device-restore-{scenario_name}-"
            ) as temporary:
                scenario_root = Path(temporary)
                host_directory = scenario_root / "host"
                host_directory.mkdir()
                if device == "virtiofs":
                    (host_directory / ".nvx-virtio-restore-host-seed").write_bytes(
                        VIRTFS_HOST_TO_GUEST
                    )
                snapshot_path = scenario_root / "snapshot"
                boot_command, restore_command = device_restore_commands(
                    executable,
                    backend,
                    kernel,
                    initrd,
                    args.memory_mib,
                    device,
                    mode,
                    command_prefix=command_prefix,
                    network=network,
                    host_directory=host_directory,
                    snapshot_path=snapshot_path,
                )
                capture_markers = capture_device_restore_snapshot(
                    [
                        *boot_command,
                        "--snapshot-destination",
                        str(snapshot_path),
                    ],
                    snapshot_path,
                    device,
                    mode,
                    timeout=timeout,
                    log_path=scenario_logs / "capture.log",
                    windows_cpus=windows_cpus,
                )
                measured: list[DeviceRestoreSample] = []
                total_runs = args.warmups + args.runs
                for index in range(total_runs):
                    result_path = host_directory / ".nvx-virtio-restore-guest-result"
                    result_path.unlink(missing_ok=True)
                    label = (
                        f"warmup-{index + 1}"
                        if index < args.warmups
                        else f"sample-{index - args.warmups + 1}"
                    )
                    log_path = scenario_logs / f"{label}.log"
                    sample = run_device_restore_sample(
                        restore_command,
                        device,
                        mode,
                        timeout=timeout,
                        log_path=log_path,
                        windows_cpus=windows_cpus,
                    )
                    sample["log"] = str(log_path.relative_to(output_dir))
                    if device == "virtiofs":
                        if (
                            not result_path.is_file()
                            or result_path.read_text(encoding="ascii")
                            != "guest-to-host\n"
                        ):
                            raise RuntimeError(
                                f"{scenario_name} did not publish its guest result"
                            )
                    if index < args.warmups:
                        print(
                            f"  {scenario_name} warmup {index + 1}/{args.warmups}: "
                            f"ready={sample['process_launch_to_ready_ms']:.3f} ms, "
                            f"I/O={sample['trigger_to_first_successful_io_ms']:.3f} ms",
                            flush=True,
                        )
                    else:
                        measured.append(sample)
                        print(
                            f"  {scenario_name} sample "
                            f"{index - args.warmups + 1}/{args.runs}: "
                            f"ready={sample['process_launch_to_ready_ms']:.3f} ms, "
                            f"I/O={sample['trigger_to_first_successful_io_ms']:.3f} ms, "
                            f"peak RSS={bytes_to_mib(sample['peak_rss_bytes']):.3f} MiB",
                            flush=True,
                        )
                result = summarize_device_restore_samples(device, mode, measured)
                result["capture_markers"] = capture_markers
                result["capture_log"] = str(
                    (scenario_logs / "capture.log").relative_to(output_dir)
                )
                scenarios[scenario_name] = result
    stale_count = 0
    for result in scenarios.values():
        value = cast(dict[str, object], result)["stale_premature_callback_count"]
        if not isinstance(value, int):
            raise TypeError("device restore stale callback count is not an integer")
        stale_count += value
    document = {
        "schema_version": 1,
        "non_canonical": True,
        "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "platform": args.platform,
        "backend": backend,
        "processors": 1,
        "memory_mib": args.memory_mib,
        "warmups": args.warmups,
        "runs": args.runs,
        "devices": list(args.restore_devices),
        "modes": list(args.restore_modes),
        "scenarios": scenarios,
        "assertions": {
            "zero_stale_premature_callbacks": stale_count == 0,
            "stale_premature_callback_count": stale_count,
        },
    }
    result_path = output_dir / "device-restore-profile.json"
    result_path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    print(f"Wrote {result_path}", flush=True)
    return result_path


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
    return (
        _read_benchmark_script("guest-exit.sh") if teardown_mode == "guest-exit" else ""
    )


def _device_io_script(
    device: str,
    duration_seconds: float,
    size_mib: int,
    teardown_mode: str,
    *,
    network_target: str | None = None,
    network_port: int = DEVICE_IO_PORT,
) -> str:
    if device not in DEVICE_IO_OPERATIONS:
        raise ValueError(f"unsupported device I/O device: {device}")
    if device == "virtio-net" and network_target is None:
        raise ValueError("virtio-net device I/O requires a network target")
    duration_ms = max(1, round(duration_seconds * 1000))
    size_bytes = size_mib * 1024 * 1024
    rendered = _render_benchmark_script(
        "device-io.sh.in",
        DEVICE=device,
        DURATION_MS=str(duration_ms),
        SIZE_BYTES=str(size_bytes),
        NETWORK_TARGET=(
            str(ipaddress.IPv4Address(network_target)) if network_target else ""
        ),
        NETWORK_PORT=str(network_port),
        RESULT_PREFIX=DEVICE_IO_GUEST_RESULT_PREFIX.removesuffix("="),
        COMPLETION_MARKER=DEVICE_IO_COMPLETION_MARKER.decode(),
    )
    return rendered + _guest_exit_script(teardown_mode)


def _device_io_positive_int(value: object, field: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ValueError(f"device I/O result {field} must be a positive integer")
    return value


def parse_device_io_guest_results(text: str, device: str) -> list[dict[str, object]]:
    if device not in DEVICE_IO_OPERATIONS:
        raise ValueError(f"unsupported device I/O device: {device}")
    parsed: dict[str, dict[str, object]] = {}
    decoder = json.JSONDecoder()
    for line in text.replace("\r", "\n").splitlines():
        prefix = line.find(DEVICE_IO_GUEST_RESULT_PREFIX)
        if prefix < 0:
            continue
        payload = line[prefix + len(DEVICE_IO_GUEST_RESULT_PREFIX) :].lstrip()
        try:
            decoded: object
            decoded, _ = decoder.raw_decode(payload)
        except json.JSONDecodeError as error:
            raise ValueError(f"invalid device I/O result JSON: {error.msg}") from error
        if not isinstance(decoded, dict):
            raise ValueError("device I/O result must be an object")
        value = cast(dict[str, object], decoded)
        required = {
            "device",
            "operation",
            "operations",
            "bytes_per_operation",
            "elapsed_ns",
        }
        if set(value) != required:
            raise ValueError(f"device I/O result keys must be {sorted(required)}")
        expected_helper = "network" if device == "virtio-net" else "file"
        if value["device"] != expected_helper:
            raise ValueError(
                f"{device} result reports helper device {value['device']!r}"
            )
        operation_value = value["operation"]
        if (
            not isinstance(operation_value, str)
            or operation_value not in DEVICE_IO_OPERATIONS[device]
        ):
            raise ValueError(f"unexpected {device} operation {operation_value!r}")
        operation = operation_value
        if operation in parsed:
            raise ValueError(f"duplicate {device} {operation} result")
        operations = _device_io_positive_int(value["operations"], "operations")
        bytes_per_operation = _device_io_positive_int(
            value["bytes_per_operation"], "bytes_per_operation"
        )
        elapsed_ns = _device_io_positive_int(value["elapsed_ns"], "elapsed_ns")
        expected_bytes = 64 if device == "virtio-net" else 4096
        if bytes_per_operation != expected_bytes:
            raise ValueError(
                f"{device} {operation} bytes_per_operation must be {expected_bytes}"
            )
        parsed[operation] = {
            "device": device,
            "operation": operation,
            "operations": operations,
            "bytes_per_operation": bytes_per_operation,
            "elapsed_ns": elapsed_ns,
        }
    expected = set(DEVICE_IO_OPERATIONS[device])
    missing = sorted(expected - set(parsed))
    if missing:
        raise ValueError(f"{device} results are missing {missing}")
    return [parsed[operation] for operation in DEVICE_IO_OPERATIONS[device]]


class UdpEchoServer:
    def __init__(self, port: int) -> None:
        self.socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.socket.bind(("0.0.0.0", port))
        self.socket.settimeout(0.2)
        self.stop = threading.Event()
        self.error: OSError | None = None
        self.thread = threading.Thread(
            target=self._serve,
            name="nvx-device-io-udp-echo",
            daemon=True,
        )

    def __enter__(self) -> UdpEchoServer:
        self.thread.start()
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.stop.set()
        self.thread.join(timeout=2)
        self.socket.close()
        if self.thread.is_alive() and exc_info[0] is None:
            raise RuntimeError("UDP echo server did not stop")
        if self.error is not None and exc_info[0] is None:
            raise RuntimeError(f"UDP echo server failed: {self.error}")

    def check(self) -> None:
        if self.error is not None:
            raise RuntimeError(f"UDP echo server failed: {self.error}")

    def _serve(self) -> None:
        try:
            while not self.stop.is_set():
                try:
                    payload, address = self.socket.recvfrom(65535)
                except TimeoutError:
                    continue
                if payload:
                    self.socket.sendto(payload, address)
        except OSError as error:
            if not self.stop.is_set():
                self.error = error


def host_ipv4_address() -> str:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.connect(("1.1.1.1", 53))
        address = probe.getsockname()[0]
    try:
        return str(ipaddress.IPv4Address(address))
    except ipaddress.AddressValueError as error:
        raise RuntimeError(
            f"cannot determine host IPv4 address: {address!r}"
        ) from error


def _device_io_attempt_record(
    device: str,
    attempt_index: int,
    warmups: int,
    result: GuestCommandResult | None,
    error: Exception | None,
) -> dict[str, object]:
    warmup = attempt_index < warmups
    record: dict[str, object] = {
        "schema_version": 1,
        "attempt_index": attempt_index,
        "sample_index": None if warmup else attempt_index - warmups,
        "warmup": warmup,
        "device": device,
    }
    if result is None:
        assert error is not None
        record.update(
            {
                "status": "failure",
                "results": [],
                "error": f"{type(error).__name__}: {error}",
            }
        )
        return record
    try:
        measurements = parse_device_io_guest_results(result["text"], device)
    except ValueError as parse_error:
        record.update(
            {
                "status": "failure",
                "results": [],
                "error": f"{type(parse_error).__name__}: {parse_error}",
            }
        )
        return record
    record.update(
        {
            "status": "success",
            "results": measurements,
            "wall_ms": result["wall_ms"],
            "peak_rss_bytes": result["peak_rss_bytes"],
        }
    )
    return record


def _decode_device_io_line(line: str) -> tuple[bool, object]:
    if not line.startswith(DEVICE_IO_RESULT_PREFIX):
        return False, None
    return True, json.loads(line.removeprefix(DEVICE_IO_RESULT_PREFIX))


def _device_io_completed_attempts(path: Path | None) -> set[tuple[str, int]]:
    if path is None or not path.is_file():
        return set()
    completed: set[tuple[str, int]] = set()
    for line_number, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), 1
    ):
        try:
            is_record, decoded = _decode_device_io_line(line)
        except json.JSONDecodeError as error:
            raise ValueError(
                f"invalid resumable device I/O record at {path}:{line_number}: {error}"
            ) from error
        if not is_record:
            continue
        if not isinstance(decoded, dict):
            raise ValueError(
                f"resumable device I/O record at {path}:{line_number} must be an object"
            )
        record = cast(dict[str, object], decoded)
        device_value = record.get("device")
        attempt_index = record.get("attempt_index")
        if (
            not isinstance(device_value, str)
            or device_value not in DEVICE_IO_OPERATIONS
            or (isinstance(attempt_index, bool) or not isinstance(attempt_index, int))
        ):
            raise ValueError(
                f"invalid resumable device I/O identity at {path}:{line_number}"
            )
        device = device_value
        identity = (device, attempt_index)
        if identity in completed:
            raise ValueError(
                f"duplicate resumable device I/O attempt {device}/{attempt_index}"
            )
        completed.add(identity)
    return completed


def _append_device_io_record(path: Path | None, record: dict[str, object]) -> None:
    line = DEVICE_IO_RESULT_PREFIX + json.dumps(
        record, sort_keys=True, separators=(",", ":")
    )
    print(line, flush=True)
    if path is None:
        return
    with path.open("a", encoding="utf-8", newline="\n") as output:
        output.write(line + "\n")
        output.flush()
        os.fsync(output.fileno())


def _read_device_io_records(path: Path) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        is_record, decoded = _decode_device_io_line(line)
        if is_record and isinstance(decoded, dict):
            records.append(cast(dict[str, object], decoded))
    return records


def _print_device_io_summary(records: Sequence[dict[str, object]]) -> int:
    rates: dict[tuple[str, str], list[float]] = {}
    failures: dict[str, int] = {device: 0 for device in DEVICE_IO_DEVICES}
    for record in records:
        if record.get("warmup") is True:
            continue
        device = cast(str, record["device"])
        if record.get("status") != "success":
            failures[device] += 1
            continue
        for result in cast(list[dict[str, object]], record["results"]):
            operation = cast(str, result["operation"])
            rate = (
                cast(int, result["operations"])
                * 1_000_000_000
                / cast(int, result["elapsed_ns"])
            )
            rates.setdefault((device, operation), []).append(rate)
    print("device I/O operation rates:")
    for device in DEVICE_IO_DEVICES:
        for operation in DEVICE_IO_OPERATIONS[device]:
            samples = rates.get((device, operation), [])
            if samples:
                print(
                    f"  {device:<10} {operation:<9}: "
                    f"{statistics.median(samples):.1f} ops/s  "
                    f"(p95 {nearest_rank_percentile(samples, 95):.1f}, "
                    f"min {min(samples):.1f}, max {max(samples):.1f}, "
                    f"n={len(samples)}, failures={failures[device]})"
                )
            else:
                print(
                    f"  {device:<10} {operation:<9}: no successful samples "
                    f"(failures={failures[device]})"
                )
    return sum(failures.values())


def benchmark_device_io_workload(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    output_path: Path | None,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> int:
    if backend not in DEVICE_IO_BACKENDS:
        raise ValueError(f"device-io is unsupported on OpenVMM/{backend}")
    if args.processors != 1:
        raise ValueError("device-io requires exactly one processor")
    if args.device_io_size_mib < 64:
        raise ValueError("device-io backing objects must be at least 64 MiB")
    completed = _device_io_completed_attempts(output_path)
    total_attempts = args.warmups + args.runs
    expected_attempts = {
        (device, attempt_index)
        for device in DEVICE_IO_DEVICES
        for attempt_index in range(total_attempts)
    }
    unexpected_attempts = sorted(completed - expected_attempts)
    if unexpected_attempts:
        raise ValueError(
            f"device-io log contains unexpected attempts: {unexpected_attempts}"
        )
    with tempfile.TemporaryDirectory(prefix="openvmm-device-io-") as temporary:
        root = Path(temporary)
        if "," in str(root):
            raise ValueError("device-io temporary directory must not contain a comma")
        block = root / "block.raw"
        with block.open("wb") as backing:
            backing.truncate(args.device_io_size_mib * 1024 * 1024)
        shared = root / "shared"
        shared.mkdir()
        network_target = None
        for device in DEVICE_IO_DEVICES:
            pending = any(
                (device, attempt) not in completed for attempt in range(total_attempts)
            )
            if not pending:
                print(
                    f"device-io {device}: all {total_attempts} attempts already recorded"
                )
                continue
            if device == "virtio-net":
                network_target = host_ipv4_address()
            network = (
                getattr(args, "net", None) or "10.0.0.2/24"
                if device == "virtio-net"
                else None
            )
            mount = f"/mnt/host,{shared},rw" if device == "virtio-fs" else None
            command = workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                DEVICE_IO_MEMORY_MIB,
                "quiet loglevel=0",
                processors=1,
                command_prefix=command_prefix,
                network=network,
                mount=mount,
                microvm_sandbox_block=block if device == "virtio-blk" else None,
            )
            context = (
                UdpEchoServer(args.device_io_port)
                if device == "virtio-net"
                else contextlib.nullcontext()
            )
            with context as echo_server:
                for attempt_index in range(total_attempts):
                    identity = (device, attempt_index)
                    if identity in completed:
                        continue
                    warmup = attempt_index < args.warmups
                    label = (
                        f"warmup {attempt_index + 1}/{args.warmups}"
                        if warmup
                        else f"sample {attempt_index - args.warmups + 1}/{args.runs}"
                    )
                    print(f"device-io {device} {label}", flush=True)
                    result = None
                    error = None
                    try:
                        result = run_guest_script(
                            command,
                            _device_io_script(
                                device,
                                args.device_io_duration_seconds,
                                args.device_io_size_mib,
                                args.teardown_mode,
                                network_target=network_target,
                                network_port=args.device_io_port,
                            ),
                            DEVICE_IO_COMPLETION_MARKER,
                            timeout=max(
                                args.timeout,
                                args.device_io_duration_seconds
                                * len(DEVICE_IO_OPERATIONS[device])
                                + 30,
                            ),
                            windows_cpus=windows_cpus,
                            teardown_mode=args.teardown_mode,
                        )
                        if isinstance(echo_server, UdpEchoServer):
                            echo_server.check()
                    except Exception as run_error:
                        error = run_error
                    record = _device_io_attempt_record(
                        device,
                        attempt_index,
                        args.warmups,
                        result,
                        error,
                    )
                    _append_device_io_record(output_path, record)
                    completed.add(identity)
    if completed != expected_attempts:
        raise RuntimeError(
            "device-io attempt set is incomplete after execution: "
            f"{sorted(expected_attempts - completed)}"
        )
    if output_path is None:
        return 0
    failures = _print_device_io_summary(_read_device_io_records(output_path))
    if failures:
        raise RuntimeError(
            f"device-io retained {failures} failed attempt(s); see {output_path}"
        )
    return 0


def smp_probe_script(
    processors: int,
    *,
    exit_guest: bool = True,
    network_gateway: str | None = None,
    ioapic_irq: int | None = None,
) -> str:
    if processors not in (1, 2, 4, 8):
        raise ValueError("microVM SMP probe supports 1, 2, 4, or 8 vCPUs")
    if (network_gateway is None) != (ioapic_irq is None):
        raise ValueError("network gateway and IOAPIC IRQ must be specified together")
    apic_ids = ",".join(str(cpu) for cpu in range(processors))
    network_probe = ""
    if network_gateway is not None and ioapic_irq is not None:
        network_probe = _render_benchmark_script(
            "smp-network-probe.sh.in",
            IOAPIC_IRQ=str(ioapic_irq),
            NETWORK_GATEWAY=str(ipaddress.IPv4Address(network_gateway)),
        )
    rendered = _render_benchmark_script(
        "smp-probe.sh.in",
        PROCESSORS=str(processors),
        APIC_IDS=apic_ids,
        NETWORK_PROBE=network_probe,
        COMPLETION_MARKER=SMP_PROBE_COMPLETION_MARKER.decode(),
    )
    return rendered + (_guest_exit_script("guest-exit") if exit_guest else "")


def prepare_snapshot_capture_script(
    processors: int,
    *,
    backend: str,
    teardown_mode: str,
    network_gateway: str | None = None,
    ioapic_irq: int | None = None,
    post_restore_script: str | None = None,
) -> str:
    clocksource_ready = ""
    if backend == "whp":
        clocksource_ready = (
            whp_stable_clocksource_wait_script()
            + "\n"
            + 'current_clocksource="$(cat "$clock_path")"\n'
            + '[ "$current_clocksource" != tsc-early ] || { '
            + 'echo "SMP-CLOCKSOURCE-FAIL expected=stable '
            + 'actual=$current_clocksource"; exit 80; }\n'
        )
    probe = smp_probe_script(
        processors,
        exit_guest=False,
        network_gateway=network_gateway,
        ioapic_irq=ioapic_irq,
    )
    probe = clocksource_ready + probe
    capture = _render_benchmark_script(
        "snapshot-capture-controller.sh.in",
        SMP_PROBE_PATH=SMP_PROBE_PATH,
        DISPATCH_MARKER=SNAPSHOT_GUEST_DISPATCH_MARKER.decode(),
        POST_RESTORE_ACTION=(
            f"{SNAPSHOT_POST_RESTORE_PATH}\n" if post_restore_script is not None else ""
        ),
        RESTORE_MARKER=RESTORE_MARKER.decode(),
    ) + _guest_exit_script(teardown_mode)
    post_restore_setup = ""
    if post_restore_script is not None:
        post_restore_setup = (
            f"cat >{SNAPSHOT_POST_RESTORE_PATH} <<'NVX_POST_RESTORE_SCRIPT'\n"
            f"{post_restore_script}"
            "NVX_POST_RESTORE_SCRIPT\n"
            f"chmod +x {SNAPSHOT_POST_RESTORE_PATH}\n"
        )
    return _render_benchmark_script(
        "prepare-snapshot-capture.sh.in",
        SMP_PROBE_PATH=SMP_PROBE_PATH,
        SMP_PROBE=probe,
        SNAPSHOT_CAPTURE_PATH=SNAPSHOT_CAPTURE_PATH,
        SNAPSHOT_CAPTURE=capture,
        POST_RESTORE_SETUP=post_restore_setup,
    )


def _virtfs_script(payload_mib: int, teardown_mode: str) -> str:
    return _render_benchmark_script(
        "virtfs.sh.in",
        PAYLOAD_MIB=str(payload_mib),
        COMPLETION_MARKER=VIRTFS_COMPLETION_MARKER.decode(),
    ) + _guest_exit_script(teardown_mode)


def _virtfs_roundtrip_script(teardown_mode: str) -> str:
    return _render_benchmark_script(
        "virtfs-roundtrip.sh.in",
        ROUNDTRIP_MARKER=VIRTFS_ROUNDTRIP_MARKER.decode(),
    ) + _guest_exit_script(teardown_mode)


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


def _memory_online_elapsed_ms(log_path: Path, expected_added_bytes: int) -> float:
    matches = RESTORE_MEMORY_MARKER_PATTERN.findall(log_path.read_bytes())
    if len(matches) != 1:
        raise RuntimeError(
            f"expected exactly one memory-online marker in {log_path}, "
            f"found {len(matches)}"
        )
    added_bytes, _memtotal_kib, elapsed_us = (int(value) for value in matches[0])
    if added_bytes != expected_added_bytes:
        raise RuntimeError(
            f"memory-online marker reported {added_bytes} added bytes, "
            f"expected {expected_added_bytes}"
        )
    return elapsed_us / 1000.0


def benchmark_snapshot_restore_memory_workload(
    args: argparse.Namespace,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    command_prefix: Sequence[str] = (),
    windows_cpus: set[int] | None = None,
) -> None:
    snapshot_profile = bool(getattr(args, "snapshot_profile", False))
    capacity_mib = RESTORE_MEMORY_TARGETS_MIB[-1]
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    environment.pop(SNAPSHOT_PROFILE_ENV, None)
    if snapshot_profile:
        environment[SNAPSHOT_PROFILE_ENV] = "1"

    print(
        "snapshot restore memory activation, "
        f"median of {args.runs} runs, base {RESTORE_MEMORY_BASE_MIB} MiB, "
        f"capacity {capacity_mib} MiB"
    )
    print()
    with tempfile.TemporaryDirectory(prefix="openvmm-memory-restore-") as temporary:
        root = Path(temporary)
        snapshot_path = root / "snapshot"
        capture_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            RESTORE_MEMORY_BASE_MIB,
            "quiet loglevel=0 shellsnap",
            processors=args.processors,
            command_prefix=command_prefix,
        )
        capture_automatic_snapshot(
            [
                *capture_command,
                "--memory-capacity",
                f"{capacity_mib}M",
                "--snapshot-destination",
                str(snapshot_path),
            ],
            snapshot_path,
            timeout=args.timeout,
            windows_cpus=windows_cpus,
        )

        for target_mib in RESTORE_MEMORY_TARGETS_MIB:
            launch_samples: list[float] = []
            activation_samples: list[float] = []
            peak_rss_samples: list[int] = []
            profile_samples: list[dict[str, object]] = []
            total = args.warmups + args.runs
            for index in range(total):
                log_path = root / f"restore-{target_mib}-{index}.log"
                launch_ms, peak_bytes, _teardown_ms, _wall_ms = measure_once(
                    [
                        *command_prefix,
                        *snapshot_restore_command(
                            executable,
                            backend,
                            snapshot_path,
                            processors=args.processors,
                            restore_memory_mib=target_mib,
                        ),
                    ],
                    environment=environment,
                    timeout=args.timeout,
                    marker=BOOT_MARKER,
                    windows_cpus=windows_cpus,
                    teardown_mode=args.teardown_mode,
                    snapshot_profile=snapshot_profile,
                    profile_sink=profile_samples,
                    log_path=log_path,
                )
                activation_ms = _memory_online_elapsed_ms(
                    log_path,
                    (target_mib - RESTORE_MEMORY_BASE_MIB) * 1024 * 1024,
                )
                if index < args.warmups:
                    print(
                        f"  target {target_mib} MiB warmup "
                        f"{index + 1}/{args.warmups}: "
                        f"activation={activation_ms:.3f} ms, "
                        f"ready={launch_ms:.3f} ms",
                        flush=True,
                    )
                    continue
                if peak_bytes is None:
                    raise RuntimeError(
                        f"OpenVMM exited before its peak RSS was sampled at "
                        f"the target {target_mib} MiB marker"
                    )
                launch_samples.append(launch_ms)
                activation_samples.append(activation_ms)
                peak_rss_samples.append(peak_bytes)
                print(
                    f"  target {target_mib} MiB sample "
                    f"{index - args.warmups + 1}/{args.runs}: "
                    f"activation={activation_ms:.3f} ms, "
                    f"ready={launch_ms:.3f} ms, "
                    f"peak RSS={bytes_to_mib(peak_bytes):.3f} MiB",
                    flush=True,
                )

            print(f"== restore target {target_mib} MiB ==")
            print(
                "  guest add+online      : " + format_sample_summary(activation_samples)
            )
            print("  process launch->ready : " + format_sample_summary(launch_samples))
            print(f"  OpenVMM peak RSS     : {format_rss_summary(peak_rss_samples)}")
            if snapshot_profile:
                print_lifecycle_profile_summary(
                    f"snapshot-restore-memory/{backend}/"
                    f"base-{RESTORE_MEMORY_BASE_MIB}/target-{target_mib}",
                    summarize_lifecycle_profiles(profile_samples),
                )
            print()


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
        "quiet loglevel=0",
        processors=args.processors,
        command_prefix=command_prefix,
    )
    capture_snapshot(
        [
            *capture_command,
            "--snapshot-destination",
            str(snapshot_path),
        ],
        snapshot_path,
        backend=backend,
        timeout=args.timeout,
        windows_cpus=windows_cpus,
        processors=args.processors,
        teardown_mode=args.teardown_mode,
    )
    return benchmark_snapshot_restore(
        args,
        executable,
        backend,
        capture_command,
        command_prefix=command_prefix,
        windows_cpus=windows_cpus,
        snapshot_path=snapshot_path,
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
        "cold boot vs lifecycle-aligned snapshot restore, "
        f"median of {args.runs} runs, {args.processors} vCPU"
    )
    print(f'cold marker    : "{BOOT_MARKER.decode()}"')
    print(f'restore marker : "{RESTORE_MARKER.decode()}"')
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
        "lifecycle-aligned shell snapshot restore, "
        f"median of {args.runs} runs, {args.processors} vCPU"
    )
    print(f'marker : "{RESTORE_MARKER.decode()}"')
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
                _format_shell_snapshot_line("snapshot restore", restored["samples_ms"])
            )
            print(
                f"  OpenVMM peak RSS    : {format_rss_summary(restored['peak_rss_samples_bytes'])}"
            )
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


def _git_status(repository: Path) -> list[str] | None:
    try:
        return subprocess.run(
            ["git", "-C", str(repository), "status", "--short"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.splitlines()
    except (OSError, subprocess.CalledProcessError):
        return None


def _device_io_helper_provenance(
    args: argparse.Namespace, initrd: Path
) -> dict[str, str]:
    source = require_file(
        args.nvx_dir.resolve() / "guest" / "common" / "nvx-device-io.c",
        "device I/O helper source",
    )
    manifest_path = require_file(
        initrd.with_name(f"{initrd.name}{BuildConstants.PACKAGE_MANIFEST_SUFFIX}"),
        "initramfs package manifest",
    )
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        helper = manifest["helpers"]["nvx-device-io"]
        source_sha256 = helper["source_sha256"]
        binary_sha256 = helper["binary_sha256"]
    except (KeyError, TypeError, json.JSONDecodeError) as error:
        raise ValueError(
            f"invalid nvx-device-io provenance in {manifest_path}"
        ) from error
    actual_source_sha256 = sha256_file(source)
    if source_sha256 != actual_source_sha256:
        raise ValueError(
            "initramfs device I/O helper source does not match the current checkout"
        )
    if not isinstance(binary_sha256, str) or not re.fullmatch(
        r"[0-9a-f]{64}", binary_sha256
    ):
        raise ValueError(f"invalid nvx-device-io binary hash in {manifest_path}")
    return {
        "source": str(source),
        "source_sha256": actual_source_sha256,
        "binary_sha256": binary_sha256,
    }


def write_benchmark_metadata(
    args: argparse.Namespace,
    output_dir: Path,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
) -> Path:
    platform = args.platform or f"{'windows' if os.name == 'nt' else 'linux'}-{backend}"
    device_io = args.suite == "device-io"
    microvm_abi_version = OpenVMMBuildConstants.MICROVM_ABI_VERSION
    processors = 1 if device_io else args.processors
    effective_network = (
        args.net or "10.0.0.2/24"
        if args.suite in {"network-snapshot", "performance", "device-io"}
        else args.net
    )
    document = {
        "schema_version": 1,
        "suite": args.suite,
        "platform": platform,
        "backend": backend,
        "microvm_abi_version": microvm_abi_version,
        "processors": processors,
        "restore_processor_targets": (
            list(RESTORE_VCPU_TARGETS)
            if args.suite == "snapshot-restore-vcpu"
            else None
        ),
        "restore_memory_base_mib": (
            RESTORE_MEMORY_BASE_MIB if args.suite == "snapshot-restore-memory" else None
        ),
        "restore_memory_targets_mib": (
            list(RESTORE_MEMORY_TARGETS_MIB)
            if args.suite == "snapshot-restore-memory"
            else None
        ),
        "network": effective_network,
        "lifecycle_network": args.net,
        "host_affinity_set": args.cpus,
        "host_cpu_reserve": args.host_cpu_reserve,
        "scratch_directory": scratch_directory_control(args),
        "memory_mib": {
            "lifecycle": args.memory_mib,
            "virtfs": args.virtfs_memory_mib,
            "shell": args.shell_memories,
            "network": args.network_memory_mib,
            "device_io": DEVICE_IO_MEMORY_MIB,
        },
        "warmups": args.warmups,
        "measured_runs": args.runs,
        "virtfs_measured_runs": args.virtfs_runs,
        "payload_mib": args.payload_mib,
        "virtfs_memory_mib": args.virtfs_memory_mib,
        "shell_memories_mib": args.shell_memories,
        "network_memory_mib": args.network_memory_mib,
        "device_io_duration_seconds": args.device_io_duration_seconds,
        "device_io_size_mib": args.device_io_size_mib,
        "device_io_port": args.device_io_port,
        "artifacts": {
            "openvmm": str(executable),
            "kernel": str(kernel),
            "initrd": str(initrd),
        },
        "artifact_revisions": {
            "nvx": _git_revision(args.nvx_dir.resolve()),
            "openvmm": _git_revision(args.openvmm_dir.resolve()),
        },
        "repository_status": {
            "nvx": _git_status(args.nvx_dir.resolve()),
            "openvmm": _git_status(args.openvmm_dir.resolve()),
        },
    }
    if device_io:
        document["artifact_sha256"] = {
            "openvmm": sha256_file(executable),
            "kernel": sha256_file(kernel),
            "initrd": sha256_file(initrd),
            "benchmark_coordinator": sha256_file(Path(__file__)),
        }
        document["device_io_helper"] = _device_io_helper_provenance(args, initrd)
    path = output_dir / BENCHMARK_METADATA_FILENAME
    if device_io and path.is_file():
        try:
            existing = json.loads(path.read_text(encoding="utf-8"))
        except (UnicodeError, json.JSONDecodeError) as error:
            raise ValueError(f"invalid resumable benchmark metadata {path}") from error
        if existing != document:
            raise ValueError(
                f"device-io resume controls or provenance do not match {path}"
            )
        return path
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
    if args.suite == "device-io" and backend not in DEVICE_IO_BACKENDS:
        raise ValueError(f"device-io is unsupported on OpenVMM/{backend}")
    if args.suite == "performance":
        requested = ["cold-start", "virtfs", "shell-snapshot"]
        if backend in NETWORK_SNAPSHOT_BACKENDS:
            requested.append("network-snapshot")
    else:
        requested = [args.suite]
    output_dir = args.output_dir
    if output_dir is None and args.suite in {"performance", "device-io"}:
        platform = (
            args.platform or f"{'windows' if os.name == 'nt' else 'linux'}-{backend}"
        )
        if args.suite == "device-io":
            output_dir = (
                args.nvx_dir.resolve()
                / "data"
                / "runs"
                / platform
                / "microvm-v2"
                / "1vcpu"
                / "device-io"
            )
        else:
            output_dir = (
                args.nvx_dir.resolve()
                / "data"
                / "runs"
                / f"{platform}-microvm-v{OpenVMMBuildConstants.MICROVM_ABI_VERSION}-{args.processors}vcpu"
            )
    if args.suite == "device-restore-profile" and output_dir is None:
        raise ValueError("device-restore-profile requires --output-dir")
    if output_dir is not None:
        output_dir = output_dir.resolve()
        if (
            args.suite == "device-restore-profile"
            and output_dir.is_dir()
            and any(output_dir.iterdir())
        ):
            raise FileExistsError(
                f"device-restore-profile requires an empty output directory: {output_dir}"
            )
        output_dir.mkdir(parents=True, exist_ok=True)
        if args.suite in {"performance", "shell-snapshot-restore"}:
            for filename in (*PERFORMANCE_LOG_FILENAMES, *LEGACY_PYTHON_LOG_FILENAMES):
                (output_dir / filename).unlink(missing_ok=True)
        if args.suite == "shell-snapshot-restore":
            (output_dir / "acceptance.json").unlink(missing_ok=True)
        if args.suite == "snapshot-restore-vcpu":
            (output_dir / "snapshot-restore-vcpu.log").unlink(missing_ok=True)
        if args.suite == "snapshot-restore-memory":
            (output_dir / "snapshot-restore-memory.log").unlink(missing_ok=True)
        metadata_path = write_benchmark_metadata(
            args, output_dir, executable, kernel, initrd, backend
        )
        print(f"Wrote {metadata_path}", flush=True)
    if args.suite == "device-restore-profile":
        assert output_dir is not None
        benchmark_device_restore_profile(
            args,
            executable,
            kernel,
            initrd,
            backend,
            output_dir,
            command_prefix=command_prefix,
            windows_cpus=windows_cpus,
        )
        return 0

    if args.suite == "device-io":
        assert output_dir is not None
        path = output_dir / DEVICE_IO_LOG_FILENAME
        result = benchmark_device_io_workload(
            args,
            executable,
            kernel,
            initrd,
            backend,
            output_path=path,
            command_prefix=command_prefix,
            windows_cpus=windows_cpus,
        )
        print(f"Wrote {path}", flush=True)
        return result

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
        "snapshot-restore-memory": (
            "snapshot-restore-memory.log",
            lambda: benchmark_snapshot_restore_memory_workload(
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


def snapshot_request_script(
    processors: int | None,
    *,
    teardown_mode: str,
) -> str:
    if processors is not None:
        return _read_benchmark_script("snapshot-request.sh")
    return _render_benchmark_script(
        "snapshot-request-direct.sh.in",
        DISPATCH_MARKER=SNAPSHOT_GUEST_DISPATCH_MARKER.decode(),
        RESTORE_MARKER=RESTORE_MARKER.decode(),
    ) + _guest_exit_script(teardown_mode)


def capture_snapshot(
    command: Sequence[str],
    snapshot_path: Path,
    *,
    backend: str,
    timeout: float,
    windows_cpus: set[int] | None = None,
    processors: int | None = None,
    teardown_mode: str = "guest-exit",
    smp_network_gateway: str | None = None,
    smp_ioapic_irq: int | None = None,
    snapshot_profile: bool = False,
    profile_sink: list[dict[str, object]] | None = None,
    post_restore_script: str | None = None,
    log_path: Path | None = None,
    boot_marker: bytes = BOOT_MARKER,
) -> tuple[float, float, float, int]:
    if snapshot_path.exists():
        shutil.rmtree(snapshot_path)
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off,openvmm_entry::vm_controller=info"
    environment[SNAPSHOT_PROFILE_ENV] = "1"
    process_started_ns = time.perf_counter_ns()
    interaction = InteractiveProcess(command, environment)
    process = interaction.process
    profile = SnapshotProfileCollector(
        process.pid,
        process_started_ns,
        collect_host_counters=snapshot_profile,
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
    snapshot_dispatched_ns = None
    snapshot_guest_dispatched_ns = None
    snapshot_request_bytes = None
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
        nonlocal snapshot_requested, snapshot_started_ns
        nonlocal snapshot_dispatched_ns, snapshot_request_bytes, deadline
        payload = snapshot_request_script(
            processors,
            teardown_mode=teardown_mode,
        ).encode("utf-8")
        snapshot_started_ns = time.perf_counter_ns()
        interaction.write_input(payload)
        snapshot_dispatched_ns = time.perf_counter_ns()
        snapshot_request_bytes = len(payload)
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
            profile.feed(chunk)
            output.extend(chunk)
            peak_bytes = _try_peak_rss(process, peak_bytes)
            if (
                snapshot_requested
                and snapshot_guest_dispatched_ns is None
                and contains_output_line(output, SNAPSHOT_GUEST_DISPATCH_MARKER)
            ):
                snapshot_guest_dispatched_ns = time.perf_counter_ns()
            if not boot_seen and boot_marker in output:
                boot_seen = True
                if processors is None:
                    request_snapshot()
                else:
                    interaction.write_input(
                        prepare_snapshot_capture_script(
                            processors,
                            backend=backend,
                            teardown_mode=teardown_mode,
                            network_gateway=smp_network_gateway,
                            ioapic_irq=smp_ioapic_irq,
                            post_restore_script=post_restore_script,
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
        if snapshot_guest_dispatched_ns is None:
            raise RuntimeError("snapshot guest dispatch marker was not observed")
        assert snapshot_started_ns is not None
        assert snapshot_dispatched_ns is not None
        assert snapshot_request_bytes is not None
        assert snapshot_published_ns is not None
        profile_sample = profile.finish_capture(
            snapshot_started_ns,
            snapshot_dispatched_ns,
            snapshot_guest_dispatched_ns,
            snapshot_published_ns,
            source_exited_ns,
            snapshot_request_bytes,
        )
        if snapshot_profile and profile_sink is not None:
            profile_sink.append(profile_sample)
        request_to_publication_ms = (
            snapshot_published_ns - snapshot_started_ns
        ) / 1_000_000
        return (
            _profile_int(profile_sample, "generation_duration_ns") / 1_000_000,
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
        if log_path is not None:
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_bytes(output)
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
    backend: str,
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
                    backend=backend,
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
    restore_memory_mib: int | None = None,
    network_profile: str | None = None,
) -> list[str]:
    command = [
        str(executable),
        "--single-process",
        "--machine",
        "microvm",
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
    if restore_memory_mib is not None:
        command.extend(("--restore-memory", f"{restore_memory_mib}M"))
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
            backend=hypervisor,
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
                backend,
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
        openvmm_dir / "target" / "release" / OpenVMMBuildConstants.WINDOWS_BINARY_NAME,
        "native OpenVMM release binary",
    )


def build_kvm(openvmm_dir: Path) -> Path:
    openvmm_wsl = windows_to_wsl(openvmm_dir)
    script_wsl = windows_to_wsl(BENCHMARK_SCRIPTS_DIR / "build-openvmm-kvm.sh")
    run_checked(
        [
            "wsl.exe",
            "--cd",
            openvmm_wsl,
            "--exec",
            "sh",
            # A login shell is required because rustup exports cargo from the profile.
            "-l",
            script_wsl,
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
    script_wsl = windows_to_wsl(BENCHMARK_SCRIPTS_DIR / "build-phase2-kvm.sh")
    run_checked(
        [
            "wsl.exe",
            "--cd",
            openvmm_wsl,
            "--exec",
            "sh",
            # A login shell is required because rustup exports cargo from the profile.
            "-l",
            script_wsl,
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
        "microvm",
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
    script_wsl = windows_to_wsl(BENCHMARK_SCRIPTS_DIR / "stage-openvmm-kvm.sh")
    run_checked(
        [
            "wsl.exe",
            "--exec",
            "sh",
            "-l",
            script_wsl,
            stage_dir,
            executable_wsl,
            kernel_wsl,
            initrd_wsl,
        ]
    )


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
    script_wsl = windows_to_wsl(BENCHMARK_SCRIPTS_DIR / "stage-phase2-kvm.sh")
    run_checked(
        [
            "wsl.exe",
            "--exec",
            "sh",
            "-l",
            script_wsl,
            stage_dir,
            executable_wsl,
        ]
    )
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
        "--processors",
        str(args.processors),
        "--hypervisor",
        "kvm",
        "--memory",
        f"{args.memory_mib}M",
        "--kernel",
        str(stage / KernelBuildConstants.BINARY_NAME),
        "--initrd",
        str(stage / AlpineBuildConstants.INITRAMFS_NAME),
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
                "kvm",
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
        result = benchmark_snapshot_capture(args, "kvm", boot_command)
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
            "microvm_abi_version": OpenVMMBuildConstants.MICROVM_ABI_VERSION,
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
                "OpenVMM snapshot input-gate start through atomic snapshot "
                "directory publication; console command delivery excluded"
            ),
            "snapshot_capture_source": (
                "OpenVMM process-relative monotonic profile clock from "
                "capture.input_gate start through capture.publication_commit"
            ),
            "snapshot_capture_timing": SNAPSHOT_CAPTURE_TIMING,
            "snapshot_publication_poll_interval_ms": 1.0,
            "snapshot_request_to_publication_scope": (
                "diagnostic host console write through first host observation "
                "of atomic snapshot publication; includes console polling"
            ),
            "snapshot_post_publication_exit_scope": (
                "first observation of atomic snapshot publication through "
                "successful source OpenVMM process exit"
            ),
            "teardown_scope": (
                "host process termination request through OpenVMM process exit; "
                "TerminateProcess on WHP and SIGTERM on Linux"
                if args.teardown_mode != "guest-exit"
                else "cold: host dispatch of nvx-exit 0 through successful "
                "OpenVMM process exit, including console delivery; "
                "restore: host observation of the readiness marker through "
                "successful OpenVMM process exit, with nvx-exit 0 prequeued "
                "in the capture controller and no new console input"
            ),
            "snapshot_restore_guest_exit_prequeued": args.teardown_mode == "guest-exit",
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
            "scratch_directory": scratch_directory_control(args),
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
        kernel = require_file(
            artifact_dir / KernelBuildConstants.BINARY_NAME, "NVX PVH kernel"
        )
        initrd = require_file(
            artifact_dir / AlpineBuildConstants.INITRAMFS_NAME,
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
                "microvm",
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
                    backend,
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


def _run_kvm_worker(command: Sequence[str], result_kind: str) -> object:
    result_prefix = (
        KVM_RESULT_PREFIX,
        KVM_E2E_RESULT_PREFIX,
        KVM_RESTORE_RESULT_PREFIX,
        KVM_SNAPSHOT_RESULT_PREFIX,
    )[("", "e2e", "restore", "snapshot").index(result_kind)]
    worker = f"KVM {result_kind}".rstrip()
    try:
        completed = subprocess.run(command, check=True, capture_output=True, text=True)
        print(completed.stdout, end="")
        for line in completed.stdout.splitlines():
            if line.startswith(result_prefix):
                return json.loads(line.removeprefix(result_prefix))
        raise RuntimeError(f"{worker} worker did not emit a result")
    except subprocess.CalledProcessError as error:
        if error.stdout:
            print(error.stdout, end="", file=sys.stderr)
        if error.stderr:
            print(error.stderr, end="", file=sys.stderr)
        raise


def _resolved_scratch_directory(args: argparse.Namespace) -> Path:
    scratch = getattr(args, "scratch_dir", None)
    if scratch is None:
        scratch = Path(tempfile.gettempdir())
    return Path(scratch).resolve()


def _kvm_worker_scratch_arguments(args: argparse.Namespace) -> list[str]:
    return [
        "--scratch-dir",
        windows_to_wsl(_resolved_scratch_directory(args)),
    ]


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
        *_kvm_worker_scratch_arguments(args),
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
        return cast(BenchmarkResult, _run_kvm_worker(command, ""))
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
        *_kvm_worker_scratch_arguments(args),
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
        return cast(KvmE2EResult, _run_kvm_worker(command, "e2e"))
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
        *_kvm_worker_scratch_arguments(args),
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
        return cast(BenchmarkResult, _run_kvm_worker(command, "restore"))
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
        *_kvm_worker_scratch_arguments(args),
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
        return cast(SnapshotCaptureResult, _run_kvm_worker(command, "snapshot"))
    finally:
        if not args.keep_kvm_stage:
            cleanup_kvm(stage_dir)


@contextlib.contextmanager
def benchmark_scratch_directory(args: argparse.Namespace) -> Generator[None]:
    """Creates benchmark temporary files under the requested scratch directory.

    Snapshot capture writes and flushes guest RAM through these files, so the
    selected volume's write throughput bounds snapshot generation time.
    """
    scratch_dir = getattr(args, "scratch_dir", None)
    if scratch_dir is None:
        yield
        return
    scratch = _resolved_scratch_directory(args)
    if not scratch.is_dir():
        raise ValueError(f"benchmark scratch directory does not exist: {scratch}")
    args.scratch_dir = scratch
    previous = tempfile.tempdir
    tempfile.tempdir = str(scratch)
    try:
        yield
    finally:
        tempfile.tempdir = previous


def scratch_directory_control(args: argparse.Namespace) -> str:
    return str(_resolved_scratch_directory(args))


def run(args: argparse.Namespace) -> int:
    with benchmark_scratch_directory(args):
        return run_benchmark(args)


def run_benchmark(args: argparse.Namespace) -> int:
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
        kernel = require_file(
            nvx_dir / "build" / KernelBuildConstants.BINARY_NAME, "NVX PVH kernel"
        )
        initrd = require_file(
            nvx_dir / "build" / AlpineBuildConstants.INITRAMFS_NAME,
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
                openvmm_dir
                / "target"
                / "release"
                / OpenVMMBuildConstants.WINDOWS_BINARY_NAME,
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
