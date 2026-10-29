"""Collect, persist, and gate CI performance results."""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import math
import os
import re
import statistics
import subprocess
import sys
import tempfile
from collections import Counter, defaultdict, deque
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import cast

from . import benchmark_host, host_telemetry, storage_events
from .build_constants import BuildConstants
from .common import bytes_to_mib, positive_int, sha256_file

ROOT = BuildConstants.REPO_ROOT
LEGACY_CSV_FIELDS = ["commit", "metric", "unit", "direction", "p50"]
CSV_FIELDS = [
    "platform",
    "microvm_abi_version",
    "processors",
    *LEGACY_CSV_FIELDS,
]
DIRECTIONS = {"lower", "higher"}
MICROVM_PROCESSOR_COUNTS = {
    1: frozenset({1}),
    2: frozenset({1, 2, 4, 8}),
}
SHARED_METRICS = frozenset(
    {
        "cold_start_base",
        "cold_start_clocksource",
        "cold_start_tsc_reliable",
        "cold_start_no_timer_check",
        "cold_start_random_trust_cpu",
        "cold_start_rcu_expedited",
        "cold_start_nokaslr",
        "cold_start_mitigations_off",
        "cold_start_cryptomgr_notests",
        "virtfs_live_write",
        "virtfs_live_read",
        "virtfs_live_roundtrip",
        "shell_snapshot_cold_128_mib",
        "shell_snapshot_restore_128_mib",
        "shell_snapshot_cold_256_mib",
        "shell_snapshot_restore_256_mib",
        "shell_snapshot_cold_512_mib",
        "shell_snapshot_restore_512_mib",
        "network_snapshot_cold",
        "network_snapshot_restore",
        "network_snapshot_restore_wall",
    }
)
LIFECYCLE_METRICS = frozenset(
    {
        "openvmm_cold_start",
        "openvmm_snapshot_generation",
        "openvmm_snapshot_restore",
        "openvmm_cold_start_guest_exit_teardown",
        "openvmm_snapshot_restore_guest_exit_teardown",
        "openvmm_cold_start_peak_rss",
        "openvmm_snapshot_generation_peak_rss",
        "openvmm_snapshot_restore_peak_rss",
    }
)
LIFECYCLE_MEMORY_MIB = 128
LIFECYCLE_BOOT_MARKER = "ALPINE-MICROVM-BOOT-OK"
LIFECYCLE_RESTORE_MARKER = "OPENVMM-SNAPSHOT-RESTORE-OK"
LIFECYCLE_CAPTURE_TIMING = "openvmm-input-gate-to-publication"
STABILITY_MINIMUM_SAMPLES = 10
STABILITY_MINIMUM_CLUSTER_SAMPLES = 2
STABILITY_MAX_P50_OVER_P25 = 1.25
STABILITY_MAX_CLUSTER_GAP = 1.25
UNSTABLE_EXIT_CODE = 75
LIFECYCLE_ATTEMPT_LOG_SCHEMA_VERSION = 1
LIFECYCLE_ATTEMPT_STATUSES = {0: "valid", UNSTABLE_EXIT_CODE: "unstable"}
"""Validation statuses by exit status; any other status is ``invalid``."""
SHA256_HEX = re.compile(r"[0-9a-f]{64}")
COMMIT_ID = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
NUMBER = r"[0-9]+(?:,[0-9]{3})*(?:\.[0-9]+)?"
ANSI_ESCAPE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
SHELL_SNAPSHOT_MEMORIES_MIB = (128, 256, 512)
BENCHMARK_METADATA_FILENAME = "benchmark-metadata.json"
DEVICE_IO_RESULT_PREFIX = "NVX_DEVICE_IO_RESULT="
DEVICE_IO_OPERATIONS = {
    "virtio-blk": ("read", "write"),
    "virtio-fs": ("read", "write"),
    "virtio-net": ("roundtrip",),
}
DEVICE_IO_METRICS = {
    ("virtio-blk", "read"): "virtio_blk_random_read_iops",
    ("virtio-blk", "write"): "virtio_blk_random_write_iops",
    ("virtio-fs", "read"): "virtio_fs_random_read_iops",
    ("virtio-fs", "write"): "virtio_fs_random_write_iops",
    ("virtio-net", "roundtrip"): "virtio_net_udp_roundtrip_ops",
}
DEVICE_IO_METRIC_NAMES = frozenset(DEVICE_IO_METRICS.values())
SHELL_SNAPSHOT_SECTION = re.compile(
    r"^==\s*(?P<memory>[0-9]+)\s+MiB\s*==\s*$"
    r"(?P<body>.*?)(?=^==\s*[0-9]+\s+MiB\s*==\s*$|\Z)",
    re.IGNORECASE | re.MULTILINE | re.DOTALL,
)
SHELL_SNAPSHOT_RESTORE_LOGS = ("shell-snapshot.log", "shell-snapshot-restore.log")
SHELL_SNAPSHOT_RESTORE_SUMMARY = re.compile(
    rf"^\s*snapshot restore\s*:\s*median\s+(?P<median>{NUMBER})\s*ms\b"
    r"[^\n]*\bn=(?P<count>[0-9]+)\)",
    re.IGNORECASE | re.MULTILINE,
)
SHELL_SNAPSHOT_RESTORE_SAMPLES = re.compile(
    r"^\s*restore samples\s*:[ \t]*(?P<samples>[^\r\n]*?)[ \t]*ms[ \t]*\r?$",
    re.IGNORECASE | re.MULTILINE,
)
SAMPLE_VALUE = re.compile(r"[0-9]+(?:\.[0-9]+)?")
# The KVM CI runners restore a shell snapshot in two modes about twice apart,
# so about a third of their restore series exceed the stability limits without
# a host stall, against fewer than 0.1% on MSHV and WHP (#345).
RESTORE_STABILITY_BACKENDS = frozenset({"mshv", "whp"})


class PerformanceError(RuntimeError):
    """Raised when benchmark data is missing or malformed."""


class UnstablePerformanceError(PerformanceError):
    """Raised when benchmark samples expose temporary host instability."""


@dataclass(frozen=True)
class Result:
    commit: str
    metric: str
    unit: str
    direction: str
    p50: float
    platform: str = ""
    microvm_abi_version: int = 1
    processors: int = 1


@dataclass(frozen=True)
class LifecycleData:
    document: dict[str, object]
    backend: str
    metrics: dict[str, MetricValue]
    microvm_abi_version: int
    processors: int


@dataclass(frozen=True)
class BenchmarkDimensions:
    platform: str
    microvm_abi_version: int
    processors: int


@dataclass(frozen=True)
class LifecycleAttempt:
    """One bounded lifecycle measurement and its validation outcome."""

    number: int
    file: str
    sha256: str | None
    status: str
    exit_status: int
    message: str | None
    published: bool = False
    document: dict[str, object] | None = None


MetricValue = tuple[str, str, float]
Parser = Callable[[str], dict[str, MetricValue]]


def _number(value: str) -> float:
    return float(value.replace(",", ""))


def _read_log(path: Path) -> str:
    data = path.read_bytes()
    if data.startswith((b"\xff\xfe", b"\xfe\xff")):
        encoding = "utf-16"
    elif b"\x00" in data[:128]:
        encoding = "utf-16-le"
    else:
        encoding = "utf-8-sig"
    try:
        return ANSI_ESCAPE.sub("", data.decode(encoding))
    except UnicodeError as error:
        raise PerformanceError(
            f"cannot decode benchmark log {path}: {error}"
        ) from error


def _parse_fixed(
    text: str, source: str, patterns: Sequence[tuple[str, str, str, str]]
) -> dict[str, MetricValue]:
    metrics: dict[str, MetricValue] = {}
    for metric, unit, direction, pattern in patterns:
        match = re.search(pattern, text, re.IGNORECASE | re.MULTILINE)
        if match is None:
            raise PerformanceError(f"missing metric '{metric}' in {source}")
        metrics[metric] = (unit, direction, _number(match.group("value")))
    return metrics


def _parse_cold_start(text: str) -> dict[str, MetricValue]:
    patterns = [
        (
            "cold_start_base",
            "ms",
            "lower",
            rf"^\s*base\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_clocksource",
            "ms",
            "lower",
            rf"^\s*clocksource=[^\s:]+\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_tsc_reliable",
            "ms",
            "lower",
            rf"^\s*tsc=reliable\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_no_timer_check",
            "ms",
            "lower",
            rf"^\s*no_timer_check\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_random_trust_cpu",
            "ms",
            "lower",
            rf"^\s*random\.trust_cpu=on\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_rcu_expedited",
            "ms",
            "lower",
            rf"^\s*rcupdate\.rcu_expedited=1\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_nokaslr",
            "ms",
            "lower",
            rf"^\s*nokaslr\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_mitigations_off",
            "ms",
            "lower",
            rf"^\s*mitigations=off\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_cryptomgr_notests",
            "ms",
            "lower",
            rf"^\s*cryptomgr\.notests\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
    ]
    return _parse_fixed(text, "cold-start.log", patterns)


def _parse_snapshot(text: str) -> dict[str, MetricValue]:
    patterns = [
        (
            "python_pandas_cold",
            "ms",
            "lower",
            rf"^\s*cold\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "python_pandas_restore",
            "ms",
            "lower",
            rf"^\s*restore\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
    ]
    return _parse_fixed(text, "snapshot.log", patterns)


def _parse_hello_snapshot(text: str) -> dict[str, MetricValue]:
    patterns = [
        (
            "python_hello_cold",
            "ms",
            "lower",
            rf"^\s*cold\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "python_hello_restore",
            "ms",
            "lower",
            rf"^\s*restore\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
    ]
    return _parse_fixed(text, "snapshot-hello.log", patterns)


def _shell_snapshot_sections(text: str, source: str) -> dict[int, str]:
    sections: dict[int, str] = {}
    for match in SHELL_SNAPSHOT_SECTION.finditer(text):
        memory_mib = int(match.group("memory"))
        if memory_mib in sections:
            raise PerformanceError(f"duplicate {memory_mib} MiB section in {source}")
        sections[memory_mib] = match.group("body")
    return sections


def _parse_shell_snapshot(text: str) -> dict[str, MetricValue]:
    sections = _shell_snapshot_sections(text, "shell-snapshot.log")

    missing = [
        memory_mib
        for memory_mib in SHELL_SNAPSHOT_MEMORIES_MIB
        if memory_mib not in sections
    ]
    if missing:
        sizes = ", ".join(f"{memory_mib} MiB" for memory_mib in missing)
        raise PerformanceError(
            f"missing memory section(s) in shell-snapshot.log: {sizes}"
        )

    metrics: dict[str, MetricValue] = {}
    for memory_mib in SHELL_SNAPSHOT_MEMORIES_MIB:
        metrics.update(
            _parse_fixed(
                sections[memory_mib],
                f"shell-snapshot.log ({memory_mib} MiB)",
                [
                    (
                        f"shell_snapshot_cold_{memory_mib}_mib",
                        "ms",
                        "lower",
                        rf"^\s*cold boot\s*:\s*median\s+"
                        rf"(?P<value>{NUMBER})\s*ms\b",
                    ),
                    (
                        f"shell_snapshot_restore_{memory_mib}_mib",
                        "ms",
                        "lower",
                        rf"^\s*snapshot restore\s*:\s*median\s+"
                        rf"(?P<value>{NUMBER})\s*ms\b",
                    ),
                ],
            )
        )
    return metrics


def _parse_shell_snapshot_restore(text: str) -> dict[str, MetricValue]:
    source = "shell-snapshot-restore.log"
    sections = _shell_snapshot_sections(text, source)
    if not sections:
        raise PerformanceError(f"missing memory sections in {source}")

    metrics: dict[str, MetricValue] = {}
    for memory_mib, body in sorted(sections.items()):
        metrics.update(
            _parse_fixed(
                body,
                f"{source} ({memory_mib} MiB)",
                [
                    (
                        f"shell_snapshot_restore_{memory_mib}_mib",
                        "ms",
                        "lower",
                        rf"^\s*snapshot restore\s*:\s*median\s+"
                        rf"(?P<value>{NUMBER})\s*ms\b",
                    )
                ],
            )
        )
    return metrics


def _shell_snapshot_restore_samples(text: str, source: str) -> dict[int, list[float]]:
    """Return the restore samples that each memory section of a shell-snapshot
    log lists, by memory size.

    A section whose summary counts at least ``STABILITY_MINIMUM_SAMPLES``
    restores must list them, so that a log can't evade the stability guard by
    omitting them. The listed samples must be positive and match the summary's
    count and median.
    """
    sections = _shell_snapshot_sections(text, source)
    if not sections:
        raise PerformanceError(f"missing memory sections in {source}")
    series: dict[int, list[float]] = {}
    for memory_mib, body in sorted(sections.items()):
        location = f"{source} ({memory_mib} MiB)"
        summary = SHELL_SNAPSHOT_RESTORE_SUMMARY.search(body)
        if summary is None:
            raise PerformanceError(f"missing snapshot restore summary in {location}")
        count = int(summary.group("count"))
        listed = SHELL_SNAPSHOT_RESTORE_SAMPLES.search(body)
        if listed is None:
            if count >= STABILITY_MINIMUM_SAMPLES:
                raise PerformanceError(f"missing restore samples in {location}")
            continue
        values = [value.strip() for value in listed.group("samples").split(",")]
        if not all(SAMPLE_VALUE.fullmatch(value) for value in values):
            raise PerformanceError(
                f"malformed restore samples in {location}: {listed.group('samples')!r}"
            )
        samples = [float(value) for value in values]
        # The stability guard divides by samples.
        if not all(sample > 0 for sample in samples):
            raise PerformanceError(f"non-positive restore sample in {location}")
        if len(samples) != count:
            raise PerformanceError(
                f"{location} lists {len(samples)} restore samples, but its "
                f"summary counts {count}"
            )
        median = statistics.median(samples)
        reported = _number(summary.group("median"))
        # The summary rounds the median to 0.1 ms, and the list rounds each
        # sample to 0.001 ms.
        if abs(median - reported) > 0.051:
            raise PerformanceError(
                f"{location} lists restore samples with median {median:.3f} ms, "
                f"but its summary reports {reported:g} ms"
            )
        series[memory_mib] = samples
    return series


def _parse_network(text: str) -> dict[str, MetricValue]:
    patterns = [
        (
            "network_snapshot_cold",
            "ms",
            "lower",
            rf"^\s*cold\s+\(guest start\s*->\s*marker\)\s*:\s*"
            rf"(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "network_snapshot_restore",
            "ms",
            "lower",
            rf"^\s*restore\s+\(guest resume\s*->\s*marker\)\s*:\s*"
            rf"(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "network_snapshot_restore_wall",
            "ms",
            "lower",
            rf"^\s*restore wall-clock\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
    ]
    return _parse_fixed(text, "network.log", patterns)


def _parse_virtfs(text: str) -> dict[str, MetricValue]:
    return _parse_fixed(
        text,
        "virtfs.log",
        [
            (
                "virtfs_live_write",
                "MB/s",
                "higher",
                rf"^\s*rw live host directory\s+write\s+"
                rf"(?P<value>{NUMBER})\s+MB/s\b",
            ),
            (
                "virtfs_live_read",
                "MB/s",
                "higher",
                rf"^\s*read\s+(?P<value>{NUMBER})\s+MB/s\b",
            ),
            (
                "virtfs_live_roundtrip",
                "ms",
                "lower",
                rf"^\s*live exchange \(cold each\)\s*:\s*"
                rf"(?P<value>{NUMBER})\s*ms\b",
            ),
        ],
    )


def _device_io_int(value: object, field: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise PerformanceError(f"device I/O {field} must be a nonnegative integer")
    return value


def _parse_device_io(
    text: str,
    *,
    expected_warmups: int | None = None,
    expected_runs: int | None = None,
) -> dict[str, MetricValue]:
    samples: dict[tuple[str, str], list[float]] = defaultdict(list)
    attempts: dict[tuple[str, int], tuple[bool, int | None]] = {}
    records = 0
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.startswith(DEVICE_IO_RESULT_PREFIX):
            continue
        records += 1
        try:
            decoded: object = json.loads(line.removeprefix(DEVICE_IO_RESULT_PREFIX))
        except json.JSONDecodeError as error:
            raise PerformanceError(
                f"malformed device I/O result on line {line_number}: {error}"
            ) from error
        if not isinstance(decoded, dict):
            raise PerformanceError(
                f"device I/O result on line {line_number} must be an object"
            )
        record = cast(dict[str, object], decoded)
        if record.get("schema_version") != 1:
            raise PerformanceError(
                f"unsupported device I/O schema on line {line_number}: "
                f"{record.get('schema_version')!r}"
            )
        device_value = record.get("device")
        if (
            not isinstance(device_value, str)
            or device_value not in DEVICE_IO_OPERATIONS
        ):
            raise PerformanceError(
                f"invalid device I/O device on line {line_number}: {device_value!r}"
            )
        device = device_value
        attempt_index = _device_io_int(record.get("attempt_index"), "attempt_index")
        warmup = record.get("warmup")
        if not isinstance(warmup, bool):
            raise PerformanceError("device I/O warmup must be a boolean")
        sample_value = record.get("sample_index")
        if warmup:
            if sample_value is not None:
                raise PerformanceError("device I/O warmup sample_index must be null")
            sample_index = None
        else:
            sample_index = _device_io_int(sample_value, "sample_index")
        identity = (device, attempt_index)
        if identity in attempts:
            if warmup:
                raise PerformanceError(
                    f"duplicate {device} warmup attempt {attempt_index}"
                )
            raise PerformanceError(f"duplicate retained {device} sample {sample_index}")
        attempts[identity] = (warmup, sample_index)
        status = record.get("status")
        if status not in {"success", "failure"}:
            raise PerformanceError(
                f"invalid device I/O status on line {line_number}: {status!r}"
            )
        results_value = record.get("results")
        if not isinstance(results_value, list):
            raise PerformanceError("device I/O results must be an array")
        results = cast(list[object], results_value)
        if status == "failure":
            error_value = record.get("error")
            if not isinstance(error_value, str) or not error_value:
                raise PerformanceError("failed device I/O result must include an error")
            if results:
                raise PerformanceError(
                    "failed device I/O result must not include measurements"
                )
            continue

        expected_operations = set(DEVICE_IO_OPERATIONS[device])
        actual_operations: set[str] = set()
        parsed: dict[str, float] = {}
        for result_value in results:
            if not isinstance(result_value, dict):
                raise PerformanceError("device I/O measurement must be an object")
            result = cast(dict[str, object], result_value)
            required = {
                "device",
                "operation",
                "operations",
                "bytes_per_operation",
                "elapsed_ns",
            }
            if set(result) != required:
                raise PerformanceError(
                    f"device I/O measurement keys must be {sorted(required)}"
                )
            if result.get("device") != device:
                raise PerformanceError(
                    f"device I/O measurement device does not match {device}"
                )
            operation_value = result.get("operation")
            if (
                not isinstance(operation_value, str)
                or operation_value not in expected_operations
            ):
                raise PerformanceError(
                    f"invalid {device} operation in device I/O result: {operation_value!r}"
                )
            operation = operation_value
            if operation in actual_operations:
                raise PerformanceError(f"duplicate {device} {operation} measurement")
            actual_operations.add(operation)
            operations = _device_io_int(result.get("operations"), "operations")
            elapsed_ns = _device_io_int(result.get("elapsed_ns"), "elapsed_ns")
            bytes_per_operation = _device_io_int(
                result.get("bytes_per_operation"), "bytes_per_operation"
            )
            expected_bytes = 64 if device == "virtio-net" else 4096
            if operations == 0 or elapsed_ns == 0:
                raise PerformanceError(
                    f"{device} {operation} measurement must report nonzero work"
                )
            if bytes_per_operation != expected_bytes:
                raise PerformanceError(
                    f"{device} {operation} bytes_per_operation must be {expected_bytes}"
                )
            parsed[operation] = operations * 1_000_000_000 / elapsed_ns
        if actual_operations != expected_operations:
            missing = sorted(expected_operations - actual_operations)
            raise PerformanceError(
                f"missing {device} device I/O measurement(s): {', '.join(missing)}"
            )
        if not warmup:
            for operation, value in parsed.items():
                samples[(device, operation)].append(value)

    if records == 0:
        raise PerformanceError("missing device I/O result records")
    if (expected_warmups is None) != (expected_runs is None):
        raise PerformanceError(
            "device I/O expected warmups and runs must be supplied together"
        )
    if expected_warmups is not None and expected_runs is not None:
        if expected_warmups < 0 or expected_runs <= 0:
            raise PerformanceError("invalid expected device I/O sampling controls")
        total = expected_warmups + expected_runs
        expected_attempts = {
            (device, attempt_index)
            for device in DEVICE_IO_OPERATIONS
            for attempt_index in range(total)
        }
        missing_attempts = sorted(expected_attempts - set(attempts))
        extra_attempts = sorted(set(attempts) - expected_attempts)
        if missing_attempts or extra_attempts:
            details: list[str] = []
            if missing_attempts:
                details.append(f"missing {missing_attempts}")
            if extra_attempts:
                details.append(f"unexpected {extra_attempts}")
            raise PerformanceError(
                "device I/O attempt coverage mismatch: " + "; ".join(details)
            )
        for (device, attempt_index), (warmup, sample_index) in attempts.items():
            expected_warmup = attempt_index < expected_warmups
            expected_sample = (
                None if expected_warmup else attempt_index - expected_warmups
            )
            if warmup != expected_warmup or sample_index != expected_sample:
                raise PerformanceError(
                    f"device I/O attempt identity mismatch for {device}/{attempt_index}: "
                    f"warmup={warmup!r}, sample_index={sample_index!r}"
                )
    missing_metrics = [
        metric for key, metric in DEVICE_IO_METRICS.items() if not samples[key]
    ]
    if missing_metrics:
        raise PerformanceError(
            "missing successful device I/O samples: " + ", ".join(missing_metrics)
        )
    return {
        metric: ("ops/s", "higher", statistics.median(samples[key]))
        for key, metric in DEVICE_IO_METRICS.items()
    }


LOG_PARSERS: dict[str, tuple[Parser, bool]] = {
    "cold-start.log": (_parse_cold_start, True),
    "virtfs.log": (_parse_virtfs, True),
    "snapshot.log": (_parse_snapshot, False),
    "snapshot-hello.log": (_parse_hello_snapshot, False),
    "shell-snapshot.log": (_parse_shell_snapshot, False),
    "shell-snapshot-restore.log": (_parse_shell_snapshot_restore, False),
    "network.log": (_parse_network, False),
    "device-io.log": (_parse_device_io, False),
}

PLATFORM_NAMES = {
    "linux-kvm": "Linux / KVM",
    "linux-kvm-baremetal": "Linux / KVM / Bare metal",
    "linux-kvm-virtual-machine": "Linux / KVM / Virtual machine",
    "linux-mshv": "Linux / MSHV",
    "linux-mshv-baremetal": "Linux / MSHV / Bare metal",
    "linux-mshv-virtual-machine": "Linux / MSHV / Virtual machine",
    "windows-whp": "Windows / WHP",
    "windows-whp-baremetal": "Windows / WHP / Bare metal",
    "windows-whp-virtual-machine": "Windows / WHP / Virtual machine",
}
OPENVMM_BACKENDS = {
    "linux-kvm": "kvm",
    "linux-kvm-baremetal": "kvm",
    "linux-kvm-virtual-machine": "kvm",
    "linux-mshv": "mshv",
    "linux-mshv-baremetal": "mshv",
    "linux-mshv-virtual-machine": "mshv",
    "windows-whp": "whp",
    "windows-whp-baremetal": "whp",
    "windows-whp-virtual-machine": "whp",
}


def _platform_metric_name(platform: str, metric: str) -> str:
    return metric


def _validate_microvm_dimensions(
    microvm_abi_version: int, processors: int, location: str
) -> None:
    supported = MICROVM_PROCESSOR_COUNTS.get(microvm_abi_version)
    if supported is None or processors not in supported:
        raise PerformanceError(
            f"unsupported microVM ABI/processor dimensions at {location}: "
            f"v{microvm_abi_version}/{processors} vCPU"
        )


def _result_filename(dimensions: BenchmarkDimensions) -> str:
    if dimensions.microvm_abi_version == 1 and dimensions.processors == 1:
        return f"{dimensions.platform}.csv"
    return (
        f"{dimensions.platform}-microvm-v{dimensions.microvm_abi_version}-"
        f"{dimensions.processors}vcpu.csv"
    )


def write_results(path: Path, results: Iterable[Result]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    rows = list(results)
    with tempfile.NamedTemporaryFile(
        "w", encoding="utf-8", newline="", dir=path.parent, delete=False
    ) as output:
        writer = csv.DictWriter(output, fieldnames=CSV_FIELDS, lineterminator="\n")
        writer.writeheader()
        for result in rows:
            writer.writerow(
                {
                    "platform": result.platform,
                    "microvm_abi_version": result.microvm_abi_version,
                    "processors": result.processors,
                    "commit": result.commit,
                    "metric": result.metric,
                    "unit": result.unit,
                    "direction": result.direction,
                    "p50": format(result.p50, ".12g"),
                }
            )
        temporary_path = Path(output.name)
    os.replace(temporary_path, path)


def read_results(path: Path) -> list[Result]:
    try:
        with path.open("r", encoding="utf-8", newline="") as source:
            reader = csv.DictReader(source)
            legacy = reader.fieldnames == LEGACY_CSV_FIELDS
            if not legacy and reader.fieldnames != CSV_FIELDS:
                raise PerformanceError(
                    f"unsupported CSV header in {path}: {reader.fieldnames}"
                )
            results: list[Result] = []
            for line_number, row in enumerate(reader, start=2):
                try:
                    p50 = float(row["p50"])
                except (TypeError, ValueError) as error:
                    raise PerformanceError(
                        f"invalid p50 in {path}:{line_number}: {row.get('p50')!r}"
                    ) from error
                if not math.isfinite(p50) or p50 <= 0:
                    raise PerformanceError(
                        f"p50 must be positive and finite in {path}:{line_number}"
                    )
                if not row["commit"] or not row["metric"] or not row["unit"]:
                    raise PerformanceError(
                        f"empty required field in {path}:{line_number}"
                    )
                if row["direction"] not in DIRECTIONS:
                    raise PerformanceError(
                        f"invalid direction in {path}:{line_number}: "
                        f"{row['direction']!r}"
                    )
                if legacy:
                    platform = path.stem
                    microvm_abi_version = 1
                    processors = 1
                else:
                    platform = row["platform"]
                    try:
                        microvm_abi_version = int(row["microvm_abi_version"])
                        processors = int(row["processors"])
                    except (TypeError, ValueError) as error:
                        raise PerformanceError(
                            f"invalid microVM dimensions in {path}:{line_number}"
                        ) from error
                _validate_microvm_dimensions(
                    microvm_abi_version,
                    processors,
                    f"{path}:{line_number}",
                )
                results.append(
                    Result(
                        commit=row["commit"],
                        metric=row["metric"],
                        unit=row["unit"],
                        direction=row["direction"],
                        p50=p50,
                        platform=platform,
                        microvm_abi_version=microvm_abi_version,
                        processors=processors,
                    )
                )
    except FileNotFoundError as error:
        raise PerformanceError(f"results file not found: {path}") from error
    return results


def append_results_summary(
    path: Path, platform: str, results: Sequence[Result]
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    title = PLATFORM_NAMES.get(platform, platform)
    dimensions = results[0]
    lines: list[str] = [
        f"## {title} benchmark results",
        "",
        f"microVM ABI v{dimensions.microvm_abi_version}, {dimensions.processors} vCPU",
        "",
        "| Metric | p50 | Preferred direction |",
        "| --- | ---: | --- |",
    ]
    for result in results:
        direction = (
            "Lower is better" if result.direction == "lower" else "Higher is better"
        )
        lines.append(
            f"| `{result.metric}` | {_format_value(result.p50, result.unit)} | {direction} |"
        )
    lines.extend(["", f"Commit: `{results[0].commit}`", ""])
    with path.open("a", encoding="utf-8", newline="\n") as output:
        output.write("\n" + "\n".join(lines))


def _json_object(value: object, location: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise PerformanceError(f"expected an object at {location}")
    return cast(dict[str, object], value)


def _benchmark_dimensions(
    values: dict[str, object], expected_platform: str, location: str
) -> BenchmarkDimensions:
    abi_value = values.get("microvm_abi_version")
    processors_value = values.get("processors")
    if abi_value is None and processors_value is None:
        return BenchmarkDimensions(expected_platform, 1, 1)
    if (
        isinstance(abi_value, bool)
        or not isinstance(abi_value, int)
        or isinstance(processors_value, bool)
        or not isinstance(processors_value, int)
    ):
        raise PerformanceError(
            f"{location} must contain integer microvm_abi_version and processors"
        )
    platform_value = values.get("platform", expected_platform)
    if platform_value is None:
        platform_value = expected_platform
    if not isinstance(platform_value, str) or platform_value != expected_platform:
        raise PerformanceError(
            f"{location} platform {platform_value!r} does not match {expected_platform!r}"
        )
    _validate_microvm_dimensions(abi_value, processors_value, location)
    return BenchmarkDimensions(platform_value, abi_value, processors_value)


def read_workload_dimensions(
    input_dir: Path, expected_platform: str
) -> tuple[BenchmarkDimensions, dict[str, object] | None]:
    metadata_path = input_dir / BENCHMARK_METADATA_FILENAME
    try:
        document = _json_object(
            json.loads(metadata_path.read_text(encoding="utf-8")), str(metadata_path)
        )
    except FileNotFoundError:
        return BenchmarkDimensions(expected_platform, 1, 1), None
    except OSError as error:
        raise PerformanceError(
            f"cannot read benchmark metadata {metadata_path}: {error}"
        ) from error
    except (UnicodeError, json.JSONDecodeError) as error:
        raise PerformanceError(
            f"invalid benchmark metadata JSON {metadata_path}: {error}"
        ) from error
    dimensions = _benchmark_dimensions(document, expected_platform, str(metadata_path))
    expected_backend = OPENVMM_BACKENDS.get(expected_platform)
    if document.get("backend") != expected_backend:
        raise PerformanceError(
            f"{metadata_path} backend {document.get('backend')!r} does not match "
            f"platform {expected_platform!r}"
        )
    return dimensions, document


def _openvmm_backend_object(
    document: dict[str, object],
    section: str,
    backend: str,
    source: Path,
) -> dict[str, object]:
    section_value = _json_object(document.get(section), f"{source}:{section}")
    return _json_object(section_value.get(backend), f"{source}:{section}.{backend}")


def _openvmm_value(
    document: dict[str, object],
    section: str,
    backend: str,
    field: str,
    source: Path,
) -> float:
    backend_value = _openvmm_backend_object(document, section, backend, source)
    value = backend_value.get(field)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise PerformanceError(
            f"expected a number at {source}:{section}.{backend}.{field}"
        )
    value = float(value)
    if not math.isfinite(value) or value <= 0:
        raise PerformanceError(
            f"value at {source}:{section}.{backend}.{field} must be positive and finite"
        )
    return value


def _openvmm_non_negative_int(
    document: dict[str, object],
    section: str,
    backend: str,
    field: str,
    source: Path,
) -> int:
    backend_value = _openvmm_backend_object(document, section, backend, source)
    value = backend_value.get(field)
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise PerformanceError(
            f"expected a non-negative integer at {source}:{section}.{backend}.{field}"
        )
    return value


def _openvmm_samples(
    document: dict[str, object],
    section: str,
    backend: str,
    field: str,
    source: Path,
) -> list[float]:
    backend_value = _openvmm_backend_object(document, section, backend, source)
    samples = backend_value.get(field)
    if not isinstance(samples, list) or not samples:
        raise PerformanceError(
            f"expected a non-empty sample list at {source}:{section}.{backend}.{field}"
        )
    typed_samples = cast(list[object], samples)
    parsed: list[float] = []
    for index, value in enumerate(typed_samples):
        if (
            isinstance(value, bool)
            or not isinstance(value, (int, float))
            or not math.isfinite(value)
            or value <= 0
        ):
            raise PerformanceError(
                f"expected a positive finite sample at "
                f"{source}:{section}.{backend}.{field}[{index}]"
            )
        parsed.append(float(value))
    return parsed


def _openvmm_statistics(
    document: dict[str, object],
    section: str,
    backend: str,
    fields: tuple[str, str, str, str],
    source: Path,
    *,
    integer_median: bool = False,
) -> tuple[float, float, float, int]:
    p50_field, min_field, max_field, samples_field = fields
    p50 = _openvmm_value(document, section, backend, p50_field, source)
    minimum = _openvmm_value(document, section, backend, min_field, source)
    maximum = _openvmm_value(document, section, backend, max_field, source)
    samples = _openvmm_samples(document, section, backend, samples_field, source)
    expected_p50 = statistics.median(samples)
    if integer_median:
        expected_p50 = float(int(expected_p50))
    expected = (expected_p50, min(samples), max(samples))
    actual = (p50, minimum, maximum)
    if any(
        not math.isclose(observed, calculated, rel_tol=1e-12, abs_tol=1e-9)
        for observed, calculated in zip(actual, expected, strict=True)
    ):
        raise PerformanceError(
            f"inconsistent statistics at {source}:{section}.{backend}: "
            f"reported p50/min/max {actual!r}, calculated {expected!r}"
        )
    return p50, minimum, maximum, len(samples)


def _validate_sample_stability(samples: Sequence[float], series: str) -> None:
    """Reject a millisecond series that a transient host stall split.

    With at least ``STABILITY_MINIMUM_SAMPLES`` samples, a p50 more than 25%
    above p25, or a gap of more than 25% between adjacent sorted samples with
    at least two samples on each side, raises ``UnstablePerformanceError``.
    A single outlier and a uniformly slower series both pass. Callers must
    pass positive samples.
    """
    if len(samples) < STABILITY_MINIMUM_SAMPLES:
        return

    ordered = sorted(samples)
    p25 = ordered[math.ceil(len(ordered) * 0.25) - 1]
    p50 = statistics.median(ordered)
    ratio = p50 / p25
    if ratio > STABILITY_MAX_P50_OVER_P25:
        raise UnstablePerformanceError(
            f"unstable {series}: p50 {p50:.3f} ms is "
            f"{(ratio - 1) * 100:.1f}% above p25 {p25:.3f} ms "
            f"(limit {(STABILITY_MAX_P50_OVER_P25 - 1) * 100:.1f}%); "
            "rerun on an idle host"
        )

    cluster_gaps = (
        (
            ordered[split_index] / ordered[split_index - 1],
            split_index,
            ordered[split_index - 1],
            ordered[split_index],
        )
        for split_index in range(
            STABILITY_MINIMUM_CLUSTER_SAMPLES,
            len(ordered) - STABILITY_MINIMUM_CLUSTER_SAMPLES + 1,
        )
    )
    gap_ratio, split_index, lower, upper = max(cluster_gaps)
    if gap_ratio > STABILITY_MAX_CLUSTER_GAP:
        raise UnstablePerformanceError(
            f"unstable {series}: samples split {split_index}/"
            f"{len(ordered) - split_index} between {lower:.3f} ms and "
            f"{upper:.3f} ms ({(gap_ratio - 1) * 100:.1f}% gap, "
            f"limit {(STABILITY_MAX_CLUSTER_GAP - 1) * 100:.1f}%); "
            "rerun on an idle host"
        )


def _validate_snapshot_generation_stability(
    document: dict[str, object], backend: str, source: Path
) -> None:
    _validate_sample_stability(
        _openvmm_samples(document, "snapshot_capture", backend, "samples_ms", source),
        f"snapshot generation at {source}:snapshot_capture.{backend}.samples_ms",
    )


def _shell_snapshot_restore_logs(input_dir: Path) -> list[Path]:
    return [
        input_dir / filename
        for filename in SHELL_SNAPSHOT_RESTORE_LOGS
        if (input_dir / filename).is_file()
    ]


def _validate_restore_stability(logs: Sequence[Path]) -> int:
    """Reject the shell-snapshot restore series in ``logs`` that a transient
    host stall split, reporting every such series, and return how many series
    had enough samples to check."""
    checked = 0
    unstable: list[str] = []
    for path in logs:
        series = _shell_snapshot_restore_samples(_read_log(path), str(path))
        for memory_mib, samples in series.items():
            if len(samples) < STABILITY_MINIMUM_SAMPLES:
                continue
            checked += 1
            try:
                _validate_sample_stability(
                    samples, f"shell-snapshot restore at {path} ({memory_mib} MiB)"
                )
            except UnstablePerformanceError as error:
                unstable.append(str(error))
    if unstable:
        raise UnstablePerformanceError("\n".join(unstable))
    return checked


def validate_workload_stability(platform: str, input_dir: Path) -> None:
    """Check the workload logs in ``input_dir`` for series that a transient
    host stall split, so that the caller can remeasure them before collection.

    Only the shell-snapshot restore series have a guard, and only on the
    backends in ``RESTORE_STABILITY_BACKENDS``.
    """
    backend = OPENVMM_BACKENDS.get(platform)
    if backend is None:
        raise PerformanceError(f"unsupported OpenVMM benchmark platform: {platform!r}")
    logs = _shell_snapshot_restore_logs(input_dir)
    if not logs:
        raise PerformanceError(f"no shell-snapshot restore log found in {input_dir}")
    if backend not in RESTORE_STABILITY_BACKENDS:
        print(
            f"SKIP: shell-snapshot restore series have no stability guard on {backend}"
        )
        return
    checked = _validate_restore_stability(logs)
    print(
        f"Shell-snapshot restore series in {input_dir} are stable "
        f"({checked} with at least {STABILITY_MINIMUM_SAMPLES} samples checked)"
    )


def read_lifecycle_data(platform: str, input_path: Path) -> LifecycleData:
    backend = OPENVMM_BACKENDS.get(platform)
    if backend is None:
        raise PerformanceError(f"unsupported OpenVMM benchmark platform: {platform!r}")
    try:
        document = _json_object(
            json.loads(input_path.read_text(encoding="utf-8")), str(input_path)
        )
    except FileNotFoundError as error:
        raise PerformanceError(f"benchmark result not found: {input_path}") from error
    except (UnicodeError, json.JSONDecodeError) as error:
        raise PerformanceError(
            f"invalid benchmark JSON {input_path}: {error}"
        ) from error

    controls = _json_object(document.get("controls"), f"{input_path}:controls")
    dimensions = _benchmark_dimensions(controls, platform, f"{input_path}:controls")
    recorded_backend = controls.get("backend")
    if recorded_backend is not None and recorded_backend not in {backend, "both"}:
        raise PerformanceError(
            f"{input_path}:controls.backend {recorded_backend!r} does not match "
            f"platform {platform!r}"
        )
    if controls.get("suite") != "e2e":
        raise PerformanceError(
            f"{input_path} is not an e2e benchmark result: {controls.get('suite')!r}"
        )
    if controls.get("memory_mib") != LIFECYCLE_MEMORY_MIB:
        raise PerformanceError(
            f"{input_path} must use the {LIFECYCLE_MEMORY_MIB} MiB lifecycle "
            f"baseline, found {controls.get('memory_mib')!r}"
        )
    if controls.get("teardown_mode") != "guest-exit":
        raise PerformanceError(
            f"{input_path} must use guest-exit teardown, found "
            f"{controls.get('teardown_mode')!r}"
        )
    if controls.get("marker") != LIFECYCLE_BOOT_MARKER:
        raise PerformanceError(
            f"{input_path} has unexpected cold-start marker {controls.get('marker')!r}"
        )
    if controls.get("restore_marker") != LIFECYCLE_RESTORE_MARKER:
        raise PerformanceError(
            f"{input_path} has unexpected snapshot-restore marker "
            f"{controls.get('restore_marker')!r}"
        )
    if controls.get("snapshot_capture_timing") != LIFECYCLE_CAPTURE_TIMING:
        raise PerformanceError(
            f"{input_path} must use {LIFECYCLE_CAPTURE_TIMING} snapshot timing; "
            "host console request timing is not a generation metric"
        )
    if controls.get("snapshot_restore_guest_exit_prequeued") is not True:
        raise PerformanceError(
            f"{input_path} must prequeue snapshot-restore guest exit; "
            "host console command delivery must not be included in teardown"
        )

    runs = controls.get("runs")
    if isinstance(runs, bool) or not isinstance(runs, int) or runs <= 0:
        raise PerformanceError(f"{input_path}:controls.runs must be a positive integer")

    timing_statistics = (
        (
            "backends",
            ("p50_ms", "min_ms", "max_ms", "samples_ms"),
        ),
        (
            "snapshot_capture",
            ("p50_ms", "min_ms", "max_ms", "samples_ms"),
        ),
        (
            "snapshot_restore",
            ("p50_ms", "min_ms", "max_ms", "samples_ms"),
        ),
        (
            "backends",
            (
                "teardown_p50_ms",
                "teardown_min_ms",
                "teardown_max_ms",
                "teardown_completed_samples_ms",
            ),
        ),
        (
            "snapshot_restore",
            (
                "teardown_p50_ms",
                "teardown_min_ms",
                "teardown_max_ms",
                "teardown_completed_samples_ms",
            ),
        ),
    )
    for section, fields in timing_statistics:
        *_, count = _openvmm_statistics(document, section, backend, fields, input_path)
        if count != runs:
            raise PerformanceError(
                f"{input_path}:{section}.{backend}.{fields[3]} contains "
                f"{count} samples, expected {runs}"
            )
    _validate_snapshot_generation_stability(document, backend, input_path)

    for section in ("backends", "snapshot_restore"):
        timeouts = _openvmm_non_negative_int(
            document,
            section,
            backend,
            "teardown_timeout_count",
            input_path,
        )
        if timeouts:
            raise PerformanceError(
                f"{input_path}:{section}.{backend} contains {timeouts} "
                "guest-exit teardown timeout(s)"
            )

    rss_statistics = (
        "peak_rss_p50_bytes",
        "peak_rss_min_bytes",
        "peak_rss_max_bytes",
        "peak_rss_samples_bytes",
    )
    for section in ("backends", "snapshot_capture", "snapshot_restore"):
        *_, count = _openvmm_statistics(
            document,
            section,
            backend,
            rss_statistics,
            input_path,
            integer_median=True,
        )
        if count != runs:
            raise PerformanceError(
                f"{input_path}:{section}.{backend}.peak_rss_samples_bytes "
                f"contains {count} samples, expected {runs}"
            )

    metric_fields = (
        ("openvmm_cold_start", "backends", "p50_ms", "ms"),
        (
            "openvmm_snapshot_generation",
            "snapshot_capture",
            "p50_ms",
            "ms",
        ),
        ("openvmm_snapshot_restore", "snapshot_restore", "p50_ms", "ms"),
        (
            "openvmm_cold_start_guest_exit_teardown",
            "backends",
            "teardown_p50_ms",
            "ms",
        ),
        (
            "openvmm_snapshot_restore_guest_exit_teardown",
            "snapshot_restore",
            "teardown_p50_ms",
            "ms",
        ),
        (
            "openvmm_cold_start_peak_rss",
            "backends",
            "peak_rss_p50_bytes",
            "MiB",
        ),
        (
            "openvmm_snapshot_generation_peak_rss",
            "snapshot_capture",
            "peak_rss_p50_bytes",
            "MiB",
        ),
        (
            "openvmm_snapshot_restore_peak_rss",
            "snapshot_restore",
            "peak_rss_p50_bytes",
            "MiB",
        ),
    )
    metrics = {
        metric: (
            unit,
            "lower",
            bytes_to_mib(value) if unit == "MiB" else value,
        )
        for metric, section, field, unit in metric_fields
        for value in [_openvmm_value(document, section, backend, field, input_path)]
    }
    if metrics.keys() != LIFECYCLE_METRICS:
        raise AssertionError("lifecycle metric definition is incomplete")
    return LifecycleData(
        document,
        backend,
        metrics,
        dimensions.microvm_abi_version,
        dimensions.processors,
    )


def record_lifecycle_attempt(
    log_path: Path,
    platform: str,
    input_path: Path,
    exit_status: int,
    message: str | None,
) -> LifecycleAttempt:
    """Append a lifecycle validation outcome to the attempt log.

    The log keeps every bounded remeasurement in order, so collection can
    name the attempt that supplied the published lifecycle metrics.
    """
    attempts = (
        read_lifecycle_attempt_log(log_path, platform) if log_path.exists() else []
    )
    try:
        location = Path(
            os.path.relpath(input_path.resolve(), log_path.resolve().parent)
        ).as_posix()
    except ValueError:
        location = str(input_path.resolve())
    attempt = LifecycleAttempt(
        number=len(attempts) + 1,
        file=location,
        sha256=sha256_file(input_path) if input_path.is_file() else None,
        status=LIFECYCLE_ATTEMPT_STATUSES.get(exit_status, "invalid"),
        exit_status=exit_status,
        message=message,
    )
    attempts.append(attempt)
    document = {
        "schema_version": LIFECYCLE_ATTEMPT_LOG_SCHEMA_VERSION,
        "platform": platform,
        "attempts": [
            {
                "attempt": item.number,
                "file": item.file,
                "sha256": item.sha256,
                "status": item.status,
                "exit_status": item.exit_status,
                "message": item.message,
            }
            for item in attempts
        ],
    }
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    return attempt


def read_lifecycle_attempt_log(log_path: Path, platform: str) -> list[LifecycleAttempt]:
    try:
        document = _json_object(
            json.loads(log_path.read_text(encoding="utf-8")), str(log_path)
        )
    except FileNotFoundError as error:
        raise PerformanceError(
            f"lifecycle attempt log not found: {log_path}"
        ) from error
    except (UnicodeError, json.JSONDecodeError) as error:
        raise PerformanceError(
            f"invalid lifecycle attempt log {log_path}: {error}"
        ) from error
    if document.get("schema_version") != LIFECYCLE_ATTEMPT_LOG_SCHEMA_VERSION:
        raise PerformanceError(
            f"{log_path} has unsupported schema_version "
            f"{document.get('schema_version')!r}"
        )
    if document.get("platform") != platform:
        raise PerformanceError(
            f"{log_path} records platform {document.get('platform')!r}, "
            f"expected {platform!r}"
        )
    entries = document.get("attempts")
    if not isinstance(entries, list):
        raise PerformanceError(f"expected an attempt list at {log_path}:attempts")
    attempts: list[LifecycleAttempt] = []
    for index, value in enumerate(cast(list[object], entries)):
        location = f"{log_path}:attempts[{index}]"
        entry = _json_object(value, location)
        number = entry.get("attempt")
        file = entry.get("file")
        digest = entry.get("sha256")
        status = entry.get("status")
        exit_status = entry.get("exit_status")
        message = entry.get("message")
        if (
            isinstance(number, bool)
            or number != index + 1
            or not isinstance(file, str)
            or not file
            or isinstance(exit_status, bool)
            or not isinstance(exit_status, int)
            or status != LIFECYCLE_ATTEMPT_STATUSES.get(exit_status, "invalid")
            or not (digest is None or isinstance(digest, str))
            or (isinstance(digest, str) and not SHA256_HEX.fullmatch(digest))
            or not (message is None or isinstance(message, str))
        ):
            raise PerformanceError(f"invalid lifecycle attempt at {location}")
        attempts.append(
            LifecycleAttempt(
                number=index + 1,
                file=file,
                sha256=digest,
                status=str(status),
                exit_status=exit_status,
                message=message,
            )
        )
    return attempts


def read_lifecycle_attempts(
    log_path: Path, platform: str, published_path: Path
) -> list[LifecycleAttempt]:
    """Read every logged attempt and mark the one published as
    ``published_path``, which must be a byte-identical copy of exactly one
    valid attempt."""
    attempts = read_lifecycle_attempt_log(log_path, platform)
    try:
        digest = sha256_file(published_path)
    except OSError as error:
        raise PerformanceError(
            f"cannot read lifecycle result {published_path}: {error}"
        ) from error
    published = [
        attempt
        for attempt in attempts
        if attempt.status == "valid" and attempt.sha256 == digest
    ]
    if len(published) != 1:
        raise PerformanceError(
            f"{published_path} matches {len(published)} valid attempts in "
            f"{log_path}; expected exactly one"
        )
    return [
        replace(
            attempt,
            published=attempt is published[0],
            document=_read_attempt_document(log_path.parent / attempt.file),
        )
        for attempt in attempts
    ]


def _read_attempt_document(path: Path) -> dict[str, object] | None:
    # A rejected attempt's diagnostics must not block publication.
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        return None
    return cast(dict[str, object], document) if isinstance(document, dict) else None


def append_openvmm_diagnostics(
    path: Path,
    platform: str,
    lifecycle: LifecycleData,
    source: Path,
    attempts: Sequence[LifecycleAttempt] = (),
) -> None:
    """Append lifecycle diagnostics, the benchmark host, and storage
    telemetry. ``attempts`` adds every logged lifecycle attempt, including
    rejected ones, to the attempt and telemetry tables."""
    document = lifecycle.document
    backend = lifecycle.backend
    cold_start = _openvmm_value(document, "backends", backend, "p50_ms", source)
    snapshot_restore = _openvmm_value(
        document, "snapshot_restore", backend, "p50_ms", source
    )
    timing_rows = (
        (
            "Cold start",
            "backends",
            ("p50_ms", "min_ms", "max_ms", "samples_ms"),
        ),
        (
            "Snapshot generation",
            "snapshot_capture",
            ("p50_ms", "min_ms", "max_ms", "samples_ms"),
        ),
        (
            "Host snapshot request to publication (diagnostic)",
            "snapshot_capture",
            (
                "request_to_publication_p50_ms",
                "request_to_publication_min_ms",
                "request_to_publication_max_ms",
                "request_to_publication_samples_ms",
            ),
        ),
        (
            "Snapshot restore",
            "snapshot_restore",
            ("p50_ms", "min_ms", "max_ms", "samples_ms"),
        ),
        (
            "Cold-start guest-exit teardown",
            "backends",
            (
                "teardown_p50_ms",
                "teardown_min_ms",
                "teardown_max_ms",
                "teardown_completed_samples_ms",
            ),
        ),
        (
            "Snapshot-restore guest-exit teardown",
            "snapshot_restore",
            (
                "teardown_p50_ms",
                "teardown_min_ms",
                "teardown_max_ms",
                "teardown_completed_samples_ms",
            ),
        ),
    )
    rss_rows = (
        ("Cold start", "backends"),
        ("Snapshot generation", "snapshot_capture"),
        ("Snapshot restore", "snapshot_restore"),
    )
    title = PLATFORM_NAMES.get(platform, platform)
    speedup = cold_start / snapshot_restore
    savings = (1 - snapshot_restore / cold_start) * 100
    lines = [
        f"## {title} lifecycle diagnostics",
        "",
        (
            "128 MiB shell baseline. Timings and peak RSS p50 values are "
            "included in regression gating."
        ),
        "",
        "| Timing | p50 | min | max | samples |",
        "| --- | ---: | ---: | ---: | ---: |",
    ]
    for label, section, fields in timing_rows:
        p50, minimum, maximum, count = _openvmm_statistics(
            document, section, backend, fields, source
        )
        lines.append(
            f"| {label} | {p50:.2f} ms | {minimum:.2f} ms | "
            f"{maximum:.2f} ms | {count} |"
        )
    lines.extend(
        [
            "",
            "Guest-exit teardown timeouts: 0.",
            "",
            "| Process phase | Peak RSS p50 | Peak RSS max |",
            "| --- | ---: | ---: |",
        ]
    )
    for label, section in rss_rows:
        p50 = _openvmm_value(document, section, backend, "peak_rss_p50_bytes", source)
        maximum = _openvmm_value(
            document, section, backend, "peak_rss_max_bytes", source
        )
        lines.append(
            f"| {label} | {bytes_to_mib(p50):.2f} MiB | "
            f"{bytes_to_mib(maximum):.2f} MiB |"
        )
    lines.extend(
        [
            "",
            "| Comparison | Value |",
            "| --- | ---: |",
            f"| Snapshot-restore speedup | {speedup:.2f}x |",
            f"| Snapshot-restore latency savings | {savings:.2f}% |",
            "",
        ]
    )
    lines.extend(_lifecycle_attempt_lines(attempts, backend))
    host = _optional_object(document.get("host"))
    if host:
        lines.extend(_host_provenance_lines(host))
    sources = (
        [
            (attempt.document, _attempt_label(attempt))
            for attempt in attempts
            if attempt.document is not None
        ]
        if attempts
        else [(document, None)]
    )
    lines.extend(_storage_telemetry_lines(sources, backend))
    with path.open("a", encoding="utf-8", newline="\n") as output:
        output.write("\n" + "\n".join(lines))


def _optional_object(value: object) -> dict[str, object]:
    return cast(dict[str, object], value) if isinstance(value, dict) else {}


def _markdown_cell(value: str) -> str:
    return value.replace("|", "\\|").replace("\n", " ")


def _attempt_label(attempt: LifecycleAttempt) -> str:
    published = ", published" if attempt.published else ""
    return f"Attempt {attempt.number}: {attempt.status}{published} (`{attempt.file}`)"


def _lifecycle_attempt_lines(
    attempts: Sequence[LifecycleAttempt], backend: str
) -> list[str]:
    if not attempts:
        return []
    lines = [
        "### Lifecycle attempts",
        "",
        "| Attempt | File | Result | Snapshot generation p50 | Published |",
        "| ---: | --- | --- | ---: | --- |",
    ]
    for attempt in attempts:
        capture = _optional_object(
            _optional_object((attempt.document or {}).get("snapshot_capture")).get(
                backend
            )
        )
        result = attempt.status
        if attempt.message is not None:
            result = f"{result}: {attempt.message}"
        lines.append(
            f"| {attempt.number} | `{attempt.file}` | {_markdown_cell(result)} | "
            f"{_summary_number(capture.get('p50_ms'), ' ms', 2)} | "
            f"{'yes' if attempt.published else 'no'} |"
        )
    return [*lines, ""]


def _host_provenance_lines(host: dict[str, object]) -> list[str]:
    lines = ["### Benchmark host", "", "| Property | Value |", "| --- | --- |"]
    lines.extend(
        f"| {label} | {_markdown_cell(value)} |"
        for label, value in host_telemetry.host_provenance_rows(host)
    )
    return [*lines, ""]


def _summary_number(value: object, unit: str, digits: int) -> str:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return "n/a"
    return f"{value:.{digits}f}{unit}"


def _storage_telemetry_lines(
    sources: Sequence[tuple[dict[str, object], str | None]], backend: str
) -> list[str]:
    sections: list[str] = []
    for document, label in sources:
        table = _storage_telemetry_table(document, backend)
        if not table:
            continue
        if label is not None:
            sections.extend([f"#### {label}", ""])
        sections.extend([*table, ""])
    if not sections:
        return []
    return [
        "### Snapshot generation storage telemetry",
        "",
        (
            "Each row describes one measured capture. Rates and averages span "
            "the telemetry samples nearest to the start and end of its "
            "`capture.mapped_memory_flush` phase. Other volumes are the "
            "monitored workspace and system volumes that do not hold scratch."
        ),
        "",
        *sections,
    ]


def _storage_telemetry_table(document: dict[str, object], backend: str) -> list[str]:
    capture = _optional_object(
        _optional_object(document.get("snapshot_capture")).get(backend)
    )
    raw_samples = _optional_object(capture.get("profile")).get("raw_samples")
    if not isinstance(raw_samples, list):
        return []
    rows: list[str] = []
    scratch_volumes: set[str] = set()
    for number, sample in enumerate(
        map(_optional_object, cast(list[object], raw_samples)), 1
    ):
        telemetry = _optional_object(sample.get("storage_telemetry"))
        if not telemetry:
            continue
        window = _optional_object(
            _optional_object(telemetry.get("phases")).get("capture.mapped_memory_flush")
        )
        roles = _optional_object(telemetry.get("volumes"))
        volumes = _optional_object(window.get("volumes"))
        scratch_keys = {
            key
            for key, value in roles.items()
            if isinstance(value, list) and "scratch" in cast(list[object], value)
        }
        scratch_volumes.update(scratch_keys)
        scratch = _optional_object(
            next((volumes[key] for key in scratch_keys if key in volumes), None)
        )
        other_writes = [
            value
            for key, stats in volumes.items()
            if key not in scratch_keys
            and isinstance(
                value := _optional_object(stats).get("write_mib_per_second"),
                (int, float),
            )
        ]
        memory = _optional_object(window.get("memory"))
        defender = _optional_object(window.get("defender"))
        generation = sample.get("generation_duration_ns")
        records = sample.get("records")
        flush = next(
            (
                record.get("duration_ns")
                for record in map(
                    _optional_object,
                    cast(list[object], records) if isinstance(records, list) else [],
                )
                if record.get("operation") == "capture"
                and record.get("phase") == "mapped_memory_flush"
            ),
            None,
        )
        cells = [
            str(number),
            _summary_number(
                generation / 1_000_000 if isinstance(generation, int) else None,
                " ms",
                2,
            ),
            _summary_number(
                flush / 1_000_000 if isinstance(flush, int) else None, " ms", 2
            ),
            _summary_number(scratch.get("write_mib_per_second"), " MiB/s", 1),
            _summary_number(scratch.get("average_write_latency_ms"), " ms", 2),
            _summary_number(scratch.get("average_queue_length"), "", 1),
            _summary_number(scratch.get("busy_percent"), "%", 0),
            _summary_number(sum(other_writes) if other_writes else None, " MiB/s", 1),
            _summary_number(
                _optional_object(window.get("cpu")).get("busy_percent"), "%", 0
            ),
            _summary_number(memory.get("cache_dirty_mib_max"), " MiB", 1),
            _summary_number(memory.get("lazy_write_mib_per_second"), " MiB/s", 1),
            _summary_number(defender.get("cpu_percent"), "%", 1),
        ]
        rows.append("| " + " | ".join(cells) + " |")
    if not rows:
        return []
    scratch_names = ", ".join(f"`{name}`" for name in sorted(scratch_volumes))
    return [
        f"Scratch volume: {scratch_names or 'not monitored'}.",
        "",
        (
            "| Sample | Generation | Flush | Scratch write | Write latency | "
            "Queue | Busy | Other write | CPU busy | Dirty cache | Lazy writes | "
            "Defender CPU |"
        ),
        (
            "| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: "
            "| ---: | ---: |"
        ),
        *rows,
    ]


RecordedHost = tuple[Path, dict[str, object]]
"""A benchmark host identity and the first document that records it."""


def _annotation(kind: str, title: str, message: str) -> str:
    """Return a GitHub Actions workflow command that annotates the job."""

    def escape(text: str) -> str:
        return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")

    title = escape(title).replace(":", "%3A").replace(",", "%2C")
    return f"::{kind} title={title}::{escape(message)}"


def _read_host(path: Path, platform: str) -> dict[str, object]:
    try:
        return benchmark_host.read_host(path, platform)
    except benchmark_host.HostFileError as error:
        raise PerformanceError(str(error)) from error


def _host_differences(
    expected: dict[str, object], actual: dict[str, object]
) -> list[benchmark_host.HostField]:
    return [
        field
        for field in benchmark_host.compare_hosts(expected, actual)
        if field.result != benchmark_host.MATCH
    ]


def _describe_fields(fields: Sequence[benchmark_host.HostField]) -> str:
    return "; ".join(field.describe() for field in fields)


def _recorded_host(
    platform: str, sources: Sequence[tuple[Path, object]]
) -> RecordedHost | None:
    """Return the host that the benchmark documents ``sources`` record: all
    of them must record the same host, or none of them may record one."""
    recorded = [
        (
            path,
            benchmark_host.host_identity(platform, _json_object(host, f"{path}:host")),
        )
        for path, host in sources
        if host is not None
    ]
    if not recorded:
        return None
    unrecorded = [path for path, host in sources if host is None]
    if unrecorded:
        raise PerformanceError(
            f"{recorded[0][0]} records a host, but {unrecorded[0]} records none"
        )
    (first_path, first), *others = recorded
    for path, host in others:
        differences = _host_differences(first, host)
        if differences:
            raise PerformanceError(
                f"{first_path} and {path} record different hosts: "
                + _describe_fields(differences)
            )
    return first_path, first


def _check_recorded_host(
    platform: str,
    output_dir: Path,
    recorded: RecordedHost | None,
    output_path: Path,
) -> None:
    """Refuse to collect results into a directory whose results of the same
    platform record another host, or mix results that record a host with
    results that record none."""
    path = benchmark_host.host_file(output_dir, platform)
    if not path.exists():
        if recorded is not None:
            unrecorded = [
                result_path
                for result_path in sorted(output_dir.glob("*.csv"))
                if result_path != output_path
                and any(row.platform == platform for row in read_results(result_path))
            ]
            if unrecorded:
                raise PerformanceError(
                    f"{unrecorded[0]} holds {platform} results that record no "
                    f"host, so the results of {recorded[0]} cannot join them"
                )
        return
    if recorded is None:
        raise PerformanceError(
            f"the {platform} results record no host, but {path} records the "
            f"host of the results in {output_dir}"
        )
    source, host = recorded
    differences = _host_differences(_read_host(path, platform), host)
    if differences:
        raise PerformanceError(
            f"{path} records another host than {source}: "
            + _describe_fields(differences)
        )


def _write_recorded_host(
    platform: str, output_dir: Path, recorded: RecordedHost | None
) -> None:
    if recorded is None:
        return
    path = benchmark_host.host_file(output_dir, platform)
    benchmark_host.write_host(path, recorded[1])
    print(f"Recorded the {platform} benchmark host: {path}")


def collect_openvmm_results(
    platform: str,
    commit: str,
    input_path: Path,
    output_dir: Path,
    summary_path: Path | None = None,
) -> Path:
    if not commit:
        raise PerformanceError("commit must not be empty")
    lifecycle = read_lifecycle_data(platform, input_path)
    host = _recorded_host(platform, [(input_path, lifecycle.document.get("host"))])
    results = [
        Result(
            commit,
            metric,
            unit,
            direction,
            p50,
            platform,
            lifecycle.microvm_abi_version,
            lifecycle.processors,
        )
        for metric, (unit, direction, p50) in sorted(lifecycle.metrics.items())
    ]
    dimensions = BenchmarkDimensions(
        platform, lifecycle.microvm_abi_version, lifecycle.processors
    )
    output_path = output_dir / _result_filename(dimensions)
    _check_recorded_host(platform, output_dir, host, output_path)
    write_results(output_path, results)
    _write_recorded_host(platform, output_dir, host)
    if summary_path is not None:
        append_results_summary(summary_path, platform, results)
        append_openvmm_diagnostics(summary_path, platform, lifecycle, input_path)
    print(
        f"Collected {len(results)} OpenVMM p50 metric(s) for {platform}: {output_path}"
    )
    return output_path


def collect_results(
    platform: str,
    commit: str,
    input_dir: Path,
    output_dir: Path,
    require_network: bool = False,
    require_shell_snapshot: bool = False,
    require_shared_suite: bool = False,
    summary_path: Path | None = None,
    lifecycle_input: Path | None = None,
    require_shell_snapshot_restore_512: bool = False,
    lifecycle_attempt_log: Path | None = None,
) -> Path:
    if not platform or "/" in platform or platform in {".", ".."}:
        raise PerformanceError(f"invalid platform name: {platform!r}")
    if not commit:
        raise PerformanceError("commit must not be empty")
    if lifecycle_attempt_log is not None and lifecycle_input is None:
        raise PerformanceError("a lifecycle attempt log requires a lifecycle input")
    if require_shell_snapshot_restore_512 and (
        require_network
        or require_shell_snapshot
        or require_shared_suite
        or lifecycle_input is not None
    ):
        raise PerformanceError(
            "the 512 MiB shell restore result cannot be combined with full-suite "
            "collection options"
        )

    dimensions, workload_metadata = read_workload_dimensions(input_dir, platform)
    device_io_suite = (
        workload_metadata is not None and workload_metadata.get("suite") == "device-io"
    )
    if device_io_suite and (
        require_network
        or require_shell_snapshot
        or require_shared_suite
        or lifecycle_input is not None
        or require_shell_snapshot_restore_512
    ):
        raise PerformanceError(
            "device-io collection cannot be combined with lifecycle/shared-suite options"
        )

    required_optional_logs = {
        "network.log": require_network,
        "shell-snapshot.log": require_shell_snapshot,
    }
    collected: dict[str, MetricValue] = {}
    for filename, (parser, required_by_default) in LOG_PARSERS.items():
        path = input_dir / filename
        if device_io_suite:
            required = filename == "device-io.log"
        else:
            required = (
                filename == "shell-snapshot-restore.log"
                if require_shell_snapshot_restore_512
                else required_by_default or required_optional_logs.get(filename, False)
            )
        if not path.exists():
            if required:
                raise PerformanceError(f"required benchmark log not found: {path}")
            print(f"SKIP: optional benchmark log not found: {path}")
            continue
        if filename == "device-io.log" and not device_io_suite:
            print(f"SKIP: device-io log belongs to another suite: {path}")
            continue
        if filename == "device-io.log" and device_io_suite:
            assert workload_metadata is not None
            warmups = workload_metadata.get("warmups")
            runs = workload_metadata.get("measured_runs")
            if (
                isinstance(warmups, bool)
                or not isinstance(warmups, int)
                or isinstance(runs, bool)
                or not isinstance(runs, int)
            ):
                raise PerformanceError(
                    "device-io metadata must contain integer warmups and measured_runs"
                )
            parsed_metrics = _parse_device_io(
                _read_log(path),
                expected_warmups=warmups,
                expected_runs=runs,
            )
        else:
            parsed_metrics = parser(_read_log(path))
        for metric, value in parsed_metrics.items():
            metric = _platform_metric_name(platform, metric)
            if metric in collected:
                raise PerformanceError(f"duplicate collected metric: {metric}")
            collected[metric] = value
    if OPENVMM_BACKENDS.get(platform) in RESTORE_STABILITY_BACKENDS:
        _validate_restore_stability(_shell_snapshot_restore_logs(input_dir))

    lifecycle = (
        read_lifecycle_data(platform, lifecycle_input)
        if lifecycle_input is not None
        else None
    )
    attempts: list[LifecycleAttempt] = []
    if lifecycle_attempt_log is not None:
        assert lifecycle_input is not None
        attempts = read_lifecycle_attempts(
            lifecycle_attempt_log, platform, lifecycle_input
        )
        source = next(attempt for attempt in attempts if attempt.published)
        print(
            f"Lifecycle metrics come from attempt {source.number} of "
            f"{len(attempts)}: {source.file}"
        )
    if lifecycle is not None:
        lifecycle_dimensions = (
            lifecycle.microvm_abi_version,
            lifecycle.processors,
        )
        workload_dimensions = (
            dimensions.microvm_abi_version,
            dimensions.processors,
        )
        if lifecycle_dimensions != workload_dimensions:
            raise PerformanceError(
                "lifecycle/workload microVM metadata mismatch: "
                f"v{lifecycle_dimensions[0]}/{lifecycle_dimensions[1]} vCPU vs "
                f"v{workload_dimensions[0]}/{workload_dimensions[1]} vCPU"
            )
        if workload_metadata is not None:
            lifecycle_controls = _json_object(
                lifecycle.document.get("controls"), f"{lifecycle_input}:controls"
            )
            comparable = {
                "backend": (
                    lifecycle.backend,
                    workload_metadata.get("backend"),
                ),
                "host_affinity_set": (
                    lifecycle_controls.get("cpus"),
                    workload_metadata.get("host_affinity_set"),
                ),
                "host_cpu_reserve": (
                    lifecycle_controls.get("host_cpu_reserve"),
                    workload_metadata.get("host_cpu_reserve"),
                ),
                "memory_mib": (
                    lifecycle_controls.get("memory_mib"),
                    _json_object(
                        workload_metadata.get("memory_mib"),
                        f"{input_dir / BENCHMARK_METADATA_FILENAME}:memory_mib",
                    ).get("lifecycle"),
                ),
                "artifact_revisions": (
                    lifecycle_controls.get("artifact_revisions"),
                    workload_metadata.get("artifact_revisions"),
                ),
                "warmups": (
                    lifecycle_controls.get("warmups"),
                    workload_metadata.get("warmups"),
                ),
                "network": (
                    lifecycle_controls.get("network"),
                    workload_metadata.get("lifecycle_network"),
                ),
            }
            for field, (lifecycle_value, workload_value) in comparable.items():
                if lifecycle_value != workload_value:
                    raise PerformanceError(
                        f"lifecycle/workload metadata mismatch for {field}: "
                        f"{lifecycle_value!r} != {workload_value!r}"
                    )
            if require_shared_suite:
                sampling = {
                    "lifecycle warmups": lifecycle_controls.get("warmups"),
                    "lifecycle measured runs": lifecycle_controls.get("runs"),
                    "workload warmups": workload_metadata.get("warmups"),
                    "workload measured runs": workload_metadata.get("measured_runs"),
                    "virtio-fs measured runs": workload_metadata.get(
                        "virtfs_measured_runs"
                    ),
                }
                expected_sampling = {
                    "lifecycle warmups": 1,
                    "lifecycle measured runs": 10,
                    "workload warmups": 1,
                    "workload measured runs": 10,
                    "virtio-fs measured runs": 10,
                }
                for field, expected in expected_sampling.items():
                    if sampling[field] != expected:
                        raise PerformanceError(
                            f"noncanonical benchmark sampling for {field}: "
                            f"{sampling[field]!r}, expected {expected}"
                        )
                workload_controls = {
                    "payload_mib": 64,
                    "virtfs_memory_mib": 512,
                    "shell_memories_mib": [128, 256, 512],
                    "network_memory_mib": 256,
                    "network": "10.0.0.2/24",
                }
                for field, expected in workload_controls.items():
                    if workload_metadata.get(field) != expected:
                        raise PerformanceError(
                            f"noncanonical benchmark control {field}: "
                            f"{workload_metadata.get(field)!r}, expected {expected!r}"
                        )
        for metric, value in lifecycle.metrics.items():
            if metric in collected:
                raise PerformanceError(f"duplicate collected metric: {metric}")
            collected[metric] = value

    if not collected:
        raise PerformanceError(f"no performance metrics found in {input_dir}")
    if device_io_suite and collected.keys() != DEVICE_IO_METRIC_NAMES:
        missing = sorted(DEVICE_IO_METRIC_NAMES - collected.keys())
        extra = sorted(collected.keys() - DEVICE_IO_METRIC_NAMES)
        raise PerformanceError(
            "device-io suite must contain exactly five metrics "
            f"(missing: {missing}; unexpected: {extra})"
        )
    if require_shell_snapshot_restore_512:
        expected_restore_metrics = {"shell_snapshot_restore_512_mib"}
        if collected.keys() != expected_restore_metrics:
            missing = sorted(expected_restore_metrics - collected.keys())
            extra = sorted(collected.keys() - expected_restore_metrics)
            details: list[str] = []
            if missing:
                details.append("missing: " + ", ".join(missing))
            if extra:
                details.append("unexpected: " + ", ".join(extra))
            raise PerformanceError(
                "512 MiB shell restore result must contain exactly "
                "shell_snapshot_restore_512_mib (" + "; ".join(details) + ")"
            )
        if dimensions.processors not in {2, 4, 8}:
            raise PerformanceError(
                "512 MiB shell restore result requires 2, 4, or 8 vCPUs, got "
                f"{dimensions.processors}"
            )
        if workload_metadata is None:
            raise PerformanceError(
                "512 MiB shell restore result requires benchmark metadata"
            )
        expected_controls: dict[str, object] = {
            "warmups": 1,
            "measured_runs": 10,
            "shell_memories_mib": [512],
        }
        for field, expected in expected_controls.items():
            if workload_metadata.get(field) != expected:
                raise PerformanceError(
                    f"noncanonical 512 MiB shell restore control {field}: "
                    f"{workload_metadata.get(field)!r}, expected {expected!r}"
                )
    expected_metrics = (
        SHARED_METRICS | LIFECYCLE_METRICS if lifecycle is not None else SHARED_METRICS
    )
    if require_shared_suite and collected.keys() != expected_metrics:
        missing = sorted(expected_metrics - collected.keys())
        extra = sorted(collected.keys() - expected_metrics)
        details: list[str] = []
        if missing:
            details.append("missing: " + ", ".join(missing))
        if extra:
            details.append("unexpected: " + ", ".join(extra))
        raise PerformanceError(
            f"shared benchmark suite must contain exactly {len(expected_metrics)} metrics ("
            + "; ".join(details)
            + ")"
        )

    # The workload logs always contribute metrics; without metadata, they
    # record no host.
    host_sources: list[tuple[Path, object]] = [
        (
            input_dir / BENCHMARK_METADATA_FILENAME,
            None if workload_metadata is None else workload_metadata.get("host"),
        )
    ]
    if lifecycle is not None and lifecycle_input is not None:
        host_sources.append((lifecycle_input, lifecycle.document.get("host")))
    host = _recorded_host(platform, host_sources)
    results = [
        Result(
            commit,
            metric,
            unit,
            direction,
            p50,
            platform,
            dimensions.microvm_abi_version,
            dimensions.processors,
        )
        for metric, (unit, direction, p50) in sorted(collected.items())
    ]
    output_path = output_dir / _result_filename(dimensions)
    _check_recorded_host(platform, output_dir, host, output_path)
    write_results(output_path, results)
    _write_recorded_host(platform, output_dir, host)
    if summary_path is not None:
        append_results_summary(summary_path, platform, results)
        if lifecycle is not None:
            assert lifecycle_input is not None
            append_openvmm_diagnostics(
                summary_path,
                platform,
                lifecycle,
                lifecycle_input,
                attempts,
            )
    print(f"Collected {len(results)} p50 metric(s) for {platform}: {output_path}")
    return output_path


def _validate_current_results(path: Path, results: Sequence[Result]) -> None:
    seen: set[tuple[str, int, int, str, str]] = set()
    for result in results:
        key = _result_identity(result)
        if key in seen:
            raise PerformanceError(
                f"duplicate platform/ABI/processors/commit/metric row in {path}: "
                f"{result.platform}/v{result.microvm_abi_version}/"
                f"{result.processors}/{result.commit}/{result.metric}"
            )
        seen.add(key)


def _result_identity(
    result: Result, fallback_platform: str = ""
) -> tuple[str, int, int, str, str]:
    return (
        result.platform or fallback_platform,
        result.microvm_abi_version,
        result.processors,
        result.commit,
        result.metric,
    )


def _validate_result_files(
    files: Sequence[Path], loaded: dict[Path, list[Result]]
) -> None:
    seen: dict[tuple[str, int, int, str, str], Path] = {}
    for path in files:
        for result in loaded[path]:
            key = _result_identity(result, path.stem)
            previous = seen.get(key)
            if previous is not None:
                raise PerformanceError(
                    f"duplicate dimensional row across {previous} and {path}: "
                    f"{result.platform or path.stem}/v{result.microvm_abi_version}/"
                    f"{result.processors}/{result.commit}/{result.metric}"
                )
            seen[key] = path


def _compatible_baseline_results(
    platform: str, results: Sequence[Result]
) -> list[Result]:
    transition = next(
        (
            index
            for index, result in enumerate(results)
            if result.metric == "virtfs_verified_reuse"
        ),
        None,
    )
    if OPENVMM_BACKENDS.get(platform) != "whp" or transition is None:
        return list(results)

    compatible: list[Result] = []
    for index, result in enumerate(results):
        if result.metric == "virtfs_verified_reuse":
            compatible.append(
                Result(
                    result.commit,
                    "virtfs_reuse",
                    result.unit,
                    result.direction,
                    result.p50,
                    result.platform,
                    result.microvm_abi_version,
                    result.processors,
                )
            )
        elif result.metric != "virtfs_reuse" or index > transition:
            compatible.append(result)
    return compatible


def _verify_result_hosts(
    source_dir: Path, history_dir: Path, platforms: Iterable[str], require_host: bool
) -> None:
    """Refuse results whose recorded host, the source's host file, differs
    from the host of the history, the history's host file, in a required
    field, and results that record no host for a history that records one.
    With ``require_host``, both files must exist."""
    for platform in sorted(platforms):
        recorded_path = benchmark_host.host_file(source_dir, platform)
        expected_path = benchmark_host.host_file(history_dir, platform)
        missing = [
            path for path in (recorded_path, expected_path) if not path.is_file()
        ]
        if missing:
            if require_host or missing == [recorded_path]:
                raise PerformanceError(
                    f"cannot verify the host of the {platform} results: "
                    f"{missing[0]} does not exist"
                )
            continue
        fields = benchmark_host.compare_hosts(
            _read_host(expected_path, platform), _read_host(recorded_path, platform)
        )
        changed = benchmark_host.mismatches(fields)
        if changed:
            raise PerformanceError(
                f"refusing to persist the {platform} results: their host differs "
                f"from {expected_path} in required fields: " + _describe_fields(changed)
            )
        drifted = benchmark_host.drift(fields)
        if drifted:
            print(
                _annotation(
                    "warning",
                    "Benchmark host drift",
                    f"The host of the {platform} results differs from "
                    f"{expected_path} in advisory fields: " + _describe_fields(drifted),
                )
            )


def _git(checkout: Path, *args: str) -> str:
    completed = subprocess.run(
        ["git", "-C", str(checkout), *args],
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        raise PerformanceError(
            f"git {' '.join(args)} failed: {completed.stderr.strip()}"
        )
    return completed.stdout


def _resolve_commit_id(revision: str, checkout: Path) -> str:
    """Return the full ID of the commit that ``revision`` names in ``checkout``.

    A symbolic revision such as ``HEAD`` names another commit by the time its
    results are persisted, so results record the commit ID instead.
    """
    if COMMIT_ID.fullmatch(revision):
        return revision
    if not revision.startswith("-"):
        try:
            commit = _git(
                checkout, "rev-parse", "--verify", "--quiet", f"{revision}^{{commit}}"
            ).strip()
        except PerformanceError:
            commit = ""
        if COMMIT_ID.fullmatch(commit):
            return commit
    raise PerformanceError(f"--commit {revision} does not name a commit in {checkout}")


def _committed_results(top: Path, revision: str, relative: str) -> list[Result]:
    shown = subprocess.run(
        ["git", "-C", str(top), "show", f"{revision}:{relative}"],
        check=False,
        capture_output=True,
    )
    if shown.returncode != 0:
        return []
    with tempfile.TemporaryDirectory() as temporary:
        # Keep the basename: a legacy CSV takes its platform from the file name.
        path = Path(temporary) / Path(relative).name
        path.write_bytes(shown.stdout)
        return read_results(path)


def _dimension_counts(
    top: Path,
    revision: str,
    relative: str,
) -> Counter[tuple[str, int, int, str]]:
    return Counter(
        _dimension_key(result, "")
        for result in _committed_results(top, revision, relative)
    )


def _reset_dimensions_since(
    git_checkout: Path,
    history_path: Path,
    commit: str,
) -> set[tuple[str, int, int, str]]:
    """Return the dimensions whose committed history lost rows after ``commit``.

    A result measured at ``commit`` predates any later commit that removes the
    dimension's rows, so it must not be appended onto the reset series.
    """
    # A mutable reference such as HEAD would compare the history with itself.
    if not COMMIT_ID.fullmatch(commit):
        raise PerformanceError(f"result commit {commit} is not a full commit ID")
    top = Path(_git(git_checkout, "rev-parse", "--show-toplevel").strip()).resolve()
    try:
        relative = history_path.resolve().relative_to(top).as_posix()
    except ValueError as error:
        raise PerformanceError(f"{history_path} is outside the git checkout") from error
    ancestor = subprocess.run(
        ["git", "-C", str(top), "merge-base", "--is-ancestor", commit, "HEAD"],
        check=False,
        capture_output=True,
    )
    if ancestor.returncode != 0:
        raise PerformanceError(
            f"result commit {commit} is not an ancestor of the target history"
        )
    # Compare each commit with its own parents: a side branch forked before
    # ``commit`` can hold fewer rows than ``commit`` without having reset them.
    later = _git(
        top,
        "log",
        "--reverse",
        "--full-history",
        "--sparse",
        "--format=%H %P",
        f"{commit}..HEAD",
        "--",
        relative,
    ).splitlines()
    resets: set[tuple[str, int, int, str]] = set()
    for line in later:
        revision, *parents = line.split()
        # Ignore commits on side branches that are not part of the history
        # leading from the measurement to HEAD, even if later merged/reverted.
        if not _on_measurement_line(top, revision, commit):
            continue
        current = _dimension_counts(top, revision, relative)
        for parent in parents:
            # A branch that neither predates nor follows the measurement can drop rows
            # that were already reset before it, so only the measurement's line counts.
            if not _on_measurement_line(top, parent, commit):
                continue
            before = _dimension_counts(top, parent, relative)
            resets.update(key for key, count in before.items() if current[key] < count)
    return resets


def _on_measurement_line(top: Path, revision: str, measured: str) -> bool:
    """Return whether ``revision`` is on the path from ``measured`` to HEAD."""
    return all(
        subprocess.run(
            [
                "git",
                "-C",
                str(top),
                "merge-base",
                "--is-ancestor",
                ancestor,
                descendant,
            ],
            check=False,
            capture_output=True,
        ).returncode
        == 0
        for ancestor, descendant in ((measured, revision), (revision, "HEAD"))
    )


def _without_reset_rows(
    git_checkout: Path,
    history_path: Path,
    results: Sequence[Result],
) -> list[Result]:
    resets: dict[str, set[tuple[str, int, int, str]]] = {}
    kept: list[Result] = []
    for result in results:
        if result.commit not in resets:
            resets[result.commit] = _reset_dimensions_since(
                git_checkout, history_path, result.commit
            )
        if _dimension_key(result, "") in resets[result.commit]:
            print(
                f"Skipped {result.metric} measured at {result.commit[:12]}: "
                f"{history_path.name} was reset after that commit"
            )
            continue
        kept.append(result)
    return kept


def _git_checkout_of(path: Path) -> Path | None:
    """Return the nearest existing directory of ``path`` when it is in a git checkout.

    ``path`` need not exist, as for a new history directory or one that a reset
    removed; neither may skip the reset and ancestry checks. CI's history is
    always in a checkout.
    """
    directory = path.resolve()
    while not directory.is_dir():
        if directory.parent == directory:
            return None
        directory = directory.parent
    probe = subprocess.run(
        ["git", "-C", str(directory), "rev-parse", "--is-inside-work-tree"],
        check=False,
        capture_output=True,
        text=True,
    )
    inside = probe.returncode == 0 and probe.stdout.strip() == "true"
    return directory if inside else None


def persist_results(
    source_dir: Path,
    history_dir: Path,
    excluded_metrics: Iterable[str] = (),
    require_host: bool = False,
    git_checkout: Path | None = None,
) -> None:
    source_files = sorted(source_dir.glob("*.csv"))
    if not source_files:
        raise PerformanceError(f"no result CSV files found in {source_dir}")
    history_dir.mkdir(parents=True, exist_ok=True)
    excluded = frozenset(excluded_metrics)
    loaded = {path: read_results(path) for path in source_files}
    for path, results in loaded.items():
        _validate_current_results(path, results)
    _validate_result_files(source_files, loaded)
    _verify_result_hosts(
        source_dir,
        history_dir,
        {result.platform for results in loaded.values() for result in results},
        require_host,
    )

    for source_path in source_files:
        source_results = loaded[source_path]
        current = [result for result in source_results if result.metric not in excluded]
        history_path = history_dir / source_path.name
        existing = read_results(history_path) if history_path.exists() else []
        existing_keys = {_result_identity(result) for result in existing}
        current = [
            result
            for result in current
            if _result_identity(result) not in existing_keys
        ]
        if git_checkout is not None:
            current = _without_reset_rows(git_checkout, history_path, current)
        metadata = {
            (
                result.platform,
                result.microvm_abi_version,
                result.processors,
                result.metric,
            ): (result.unit, result.direction)
            for result in existing
        }
        for result in current:
            expected = metadata.get(
                (
                    result.platform,
                    result.microvm_abi_version,
                    result.processors,
                    result.metric,
                )
            )
            actual = (result.unit, result.direction)
            if expected is not None and expected != actual:
                raise PerformanceError(
                    f"metric metadata changed for {result.metric} in {history_path}: "
                    f"{expected} -> {actual}"
                )

        new_results = current
        if not new_results:
            print(f"No new performance rows to persist for {source_path.name}")
            continue
        write_results(history_path, [*existing, *new_results])
        print(f"Persisted {len(new_results)} row(s) to {history_path}")


def _format_value(value: float, unit: str) -> str:
    return f"{value:.2f} {unit}"


def _dimension_key(result: Result, fallback_platform: str) -> tuple[str, int, int, str]:
    return (
        result.platform or fallback_platform,
        result.microvm_abi_version,
        result.processors,
        result.metric,
    )


def _dimension_label(result: Result, fallback_platform: str) -> str:
    platform = result.platform or fallback_platform
    if result.microvm_abi_version == 1 and result.processors == 1:
        return platform
    return f"{platform}/microvm-v{result.microvm_abi_version}/{result.processors}vcpu"


def gate_results(
    baseline_dir: Path,
    target_dir: Path,
    window: int,
    threshold: float,
    summary_path: Path | None = None,
    absolute_tolerance_ms: float = 5.0,
    minimum_history: int = 10,
    history_reset_dir: Path | None = None,
) -> int:
    if minimum_history > window:
        raise PerformanceError(
            f"minimum history ({minimum_history}) exceeds window ({window})"
        )
    target_files = sorted(target_dir.glob("*.csv"))
    if not target_files:
        raise PerformanceError(f"no result CSV files found in {target_dir}")
    loaded_targets = {path: read_results(path) for path in target_files}
    for path, results in loaded_targets.items():
        _validate_current_results(path, results)
    _validate_result_files(target_files, loaded_targets)
    host_changes = _host_changes(
        baseline_dir,
        history_reset_dir,
        {
            result.platform or path.stem
            for path, results in loaded_targets.items()
            for result in results
        },
    )
    unreset: dict[str, int] = defaultdict(int)

    checked = 0
    regressions = 0
    summary = [
        "## Performance regression gate",
        "",
        f"PR p50 versus the median of the base branch's latest {window} p50 values "
        f"(minimum history: {minimum_history}; failure threshold: >{threshold:g}%; "
        f"lower-is-better millisecond metrics "
        f"must also increase by >{absolute_tolerance_ms:g} ms).",
        "",
    ]
    for platform, changed in host_changes.items():
        summary.extend(
            (
                f"This change accepts a new expected host for {platform}, so each "
                f"of its metrics must restart its warmup: {_describe_fields(changed)}.",
                "",
            )
        )
    summary.extend(
        (
            "| Platform | Metric | PR p50 | Base p50 median | Delta | Result |",
            "| --- | --- | ---: | ---: | ---: | --- |",
        )
    )

    for target_path in target_files:
        targets = loaded_targets[target_path]
        fallback_platform = target_path.stem
        platform = targets[0].platform or fallback_platform
        baseline_path = baseline_dir / target_path.name
        baselines = (
            _compatible_baseline_results(platform, read_results(baseline_path))
            if baseline_path.exists()
            else []
        )
        reset_dimensions: set[tuple[str, int, int, str]] = set()
        if history_reset_dir is not None:
            candidate = history_reset_dir / target_path.name
            if candidate.is_file():
                retained_dimensions = {
                    _dimension_key(result, platform)
                    for result in _compatible_baseline_results(
                        platform, read_results(candidate)
                    )
                }
                reset_dimensions = {
                    _dimension_key(result, platform) for result in baselines
                } - retained_dimensions
                baselines = [
                    result
                    for result in baselines
                    if _dimension_key(result, platform) not in reset_dimensions
                ]
        history: dict[tuple[str, int, int, str], deque[Result]] = defaultdict(
            lambda: deque(maxlen=window)
        )
        for result in baselines:
            history[_dimension_key(result, platform)].append(result)

        for target in sorted(targets, key=lambda result: result.metric):
            dimension = _dimension_label(target, platform)
            samples = history.get(_dimension_key(target, platform))
            if not samples:
                reset = _dimension_key(target, platform) in reset_dimensions
                reason = (
                    "history explicitly reset in this change"
                    if reset
                    else "has no base-branch history"
                )
                print(f"WARMUP: {dimension}/{target.metric} {reason}")
                status = "Warmup (history reset)" if reset else "Warmup"
                summary.append(
                    f"| {dimension} | `{target.metric}` | "
                    f"{_format_value(target.p50, target.unit)} | - | - | {status} |"
                )
                continue
            if platform in host_changes:
                # Persisting this host's results after the previous host's
                # would mix two hosts in one series.
                unreset[platform] += 1
                print(
                    f"HOST CHANGED: {dimension}/{target.metric} keeps "
                    f"{len(samples)} base-branch point(s) of the previous host"
                )
                summary.append(
                    f"| {dimension} | `{target.metric}` | "
                    f"{_format_value(target.p50, target.unit)} | - | - | "
                    "**Host changed without a history reset** |"
                )
                continue
            if len(samples) < minimum_history:
                message = (
                    f"WARMUP: {dimension}/{target.metric} has {len(samples)} "
                    f"of {minimum_history} required base-branch points"
                )
                print(message)
                summary.append(
                    f"| {dimension} | `{target.metric}` | "
                    f"{_format_value(target.p50, target.unit)} | - "
                    f"({len(samples)}/{minimum_history}) | - | "
                    f"Warmup ({len(samples)}/{minimum_history}) |"
                )
                continue

            for sample in samples:
                if (sample.unit, sample.direction) != (
                    target.unit,
                    target.direction,
                ):
                    raise PerformanceError(
                        f"metric metadata differs between target and baseline for "
                        f"{dimension}/{target.metric}"
                    )

            baseline_median = statistics.median(sample.p50 for sample in samples)
            if target.direction == "lower":
                delta = (target.p50 - baseline_median) / baseline_median * 100
            else:
                delta = (baseline_median - target.p50) / baseline_median * 100

            checked += 1
            absolute_delta_ms = (
                target.p50 - baseline_median
                if target.direction == "lower" and target.unit == "ms"
                else None
            )
            regressed = delta > threshold and (
                absolute_delta_ms is None or absolute_delta_ms > absolute_tolerance_ms
            )
            regressions += int(regressed)
            status = "REGRESSION" if regressed else "OK"
            absolute_detail = (
                f", {absolute_delta_ms:+.2f} ms"
                if absolute_delta_ms is not None
                else ""
            )
            print(
                f"{status}: {dimension}/{target.metric}: p50 "
                f"{_format_value(target.p50, target.unit)} vs "
                f"{len(samples)}-point base median "
                f"{_format_value(baseline_median, target.unit)} "
                f"({delta:+.1f}%{absolute_detail})"
            )
            summary.append(
                f"| {dimension} | `{target.metric}` | "
                f"{_format_value(target.p50, target.unit)} | "
                f"{_format_value(baseline_median, target.unit)} "
                f"({len(samples)}/{window}) | {delta:+.1f}%{absolute_detail} | "
                f"{status} |"
            )

    totals = f"Checked {checked} metric(s); found {regressions} regression(s)."
    summary.extend(["", totals, ""])
    # A series with a new host must keep no row of the previous host in any
    # history file, including metrics that this change no longer measures.
    kept_history: dict[str, str] = {}
    for platform in host_changes:
        assert history_reset_dir is not None
        rows = _retained_rows(history_reset_dir, platform)
        if rows:
            kept_history[platform] = ", ".join(
                f"{path.name} ({count} row(s))" for path, count in rows.items()
            )
        elif unreset[platform]:
            kept_history[platform] = (
                f"{unreset[platform]} metric(s) with base-branch history"
            )
    for platform, kept in kept_history.items():
        summary.extend(
            (
                f"**The {platform} series keeps the previous host's history**: "
                f"{kept}. Truncate each of its history files to its header line.",
                "",
            )
        )
    if summary_path is not None:
        summary_path.parent.mkdir(parents=True, exist_ok=True)
        with summary_path.open("a", encoding="utf-8") as output:
            output.write("\n".join(summary))

    for platform, kept in kept_history.items():
        assert history_reset_dir is not None
        print(
            _annotation(
                "error",
                "Benchmark host changed without a history reset",
                f"{benchmark_host.host_file(history_reset_dir, platform)} changes "
                f"required fields of the {platform} host "
                f"({_describe_fields(host_changes[platform])}), but the series "
                f"keeps the previous host's history: {kept}. Truncate each of the "
                "series' history files to its header line in the same change "
                '(doc/benchmarks.md, "Expected benchmark hosts").',
            )
        )
    print(
        f"Checked {checked} metric(s), found {regressions} regression(s) "
        f"(threshold: >{threshold:g}% vs {window}-point median; "
        f"minimum history: {minimum_history}; "
        f"absolute latency tolerance: {absolute_tolerance_ms:g} ms)."
    )
    return 1 if regressions or kept_history else 0


def _retained_rows(directory: Path, platform: str) -> dict[Path, int]:
    """Count the rows of ``platform`` in each history file in ``directory``."""
    counts: dict[Path, int] = {}
    for path in sorted(directory.glob("*.csv")):
        count = sum(result.platform == platform for result in read_results(path))
        if count:
            counts[path] = count
    return counts


def _host_changes(
    baseline_dir: Path, candidate_dir: Path | None, platforms: Iterable[str]
) -> dict[str, list[benchmark_host.HostField]]:
    """Return the required fields in which each platform's expected host in
    ``candidate_dir`` differs from the base branch's in ``baseline_dir``."""
    changes: dict[str, list[benchmark_host.HostField]] = {}
    if candidate_dir is None:
        return changes
    for platform in sorted(platforms):
        base = benchmark_host.host_file(baseline_dir, platform)
        candidate = benchmark_host.host_file(candidate_dir, platform)
        if base.is_file() and candidate.is_file():
            changed = benchmark_host.mismatches(
                benchmark_host.compare_hosts(
                    _read_host(base, platform), _read_host(candidate, platform)
                )
            )
            if changed:
                changes[platform] = changed
    return changes


def check_host(
    platform: str,
    history_dir: Path,
    scratch_dir: Path | None = None,
    output: Path | None = None,
    summary_path: Path | None = None,
) -> int:
    """Compare this host with the expected host of ``platform``'s series, in
    ``history_dir``. Return 0 when every required field matches, even with
    advisory drift, 1 when one differs, and 2 when the expected host is
    missing or malformed."""
    provenance = host_telemetry.host_provenance(
        benchmark_host.benchmark_volumes(scratch_dir)
    )
    print("Benchmark host: " + host_telemetry.describe_host_provenance(provenance))
    actual = benchmark_host.host_identity(platform, provenance)
    expected_path = benchmark_host.host_file(history_dir, platform)
    title = PLATFORM_NAMES.get(platform, platform)
    # Read the expected host first: --output may name the expected host file
    # itself, and the check must compare with the file as it was.
    expected: dict[str, object] = {}
    failure: benchmark_host.HostFileError | None = None
    try:
        expected = benchmark_host.read_host(expected_path, platform)
    except benchmark_host.HostFileError as error:
        failure = error
    if output is not None:
        benchmark_host.write_host(output, actual)
    if failure is not None:
        print(
            _annotation(
                "error",
                "Benchmark host",
                f"{title}: {failure}. Add the series' expected host, such as this "
                "runner's from the step summary, as doc/benchmarks.md "
                '("Expected benchmark hosts") describes.',
            )
        )
        print(benchmark_host.format_host(actual), end="")
        if summary_path is not None:
            with summary_path.open("a", encoding="utf-8", newline="\n") as summary:
                summary.write(
                    "\n".join(
                        (
                            "",
                            f"## Benchmark host check: {title}",
                            "",
                            f"**No valid expected host**: {failure}.",
                            "",
                            *benchmark_host.actual_host_markdown(actual),
                        )
                    )
                )
        return 2
    fields = benchmark_host.compare_hosts(expected, actual)
    changed = benchmark_host.mismatches(fields)
    drifted = benchmark_host.drift(fields)
    for field in fields:
        print(f"{field.result.upper()}: {field.describe()}")
    if changed:
        print(
            _annotation(
                "error",
                "Benchmark host mismatch",
                f"{title}: this runner differs from {expected_path} in "
                f"{len(changed)} required field(s): {_describe_fields(changed)}. "
                "The series' history comes from the expected host; to measure "
                "it here, accept this host and reset the history "
                '(doc/benchmarks.md, "Expected benchmark hosts").',
            )
        )
    if drifted:
        print(
            _annotation(
                "warning",
                "Benchmark host drift",
                f"{title}: this runner differs from {expected_path} in "
                f"advisory field(s): {_describe_fields(drifted)}. Update the "
                "file to acknowledge them; that needs no history reset.",
            )
        )
    if changed or drifted:
        print(benchmark_host.format_host(actual), end="")
    if summary_path is not None:
        with summary_path.open("a", encoding="utf-8", newline="\n") as summary:
            summary.write(
                "\n"
                + benchmark_host.host_check_markdown(
                    title, expected_path, fields, actual
                )
            )
    if changed:
        outcome = f"{len(changed)} required field(s) differ"
    elif drifted:
        outcome = f"match with {len(drifted)} advisory difference(s)"
    else:
        outcome = "match"
    print(f"Benchmark host check for {platform}: {outcome}")
    return 1 if changed else 0


def save_storage_events(
    since: dt.datetime, output_dir: Path, scratch_dir: Path | None = None
) -> int:
    """Save the storage events of a Windows job's window, from ``since`` to
    now, in ``output_dir``, print their summary, and warn of slow I/O and of
    an unreadable log. A failed job saves them as diagnostics, so an
    unreadable log doesn't fail the save; other hosts have no such events."""
    if sys.platform != "win32":
        print("ERROR: storage events are recorded only on Windows", file=sys.stderr)
        return 2

    def volumes() -> dict[str, Path]:
        # Resolving a path on a stalled volume can wait for it, so this runs
        # only after the export, and within its bound.
        volumes = benchmark_host.benchmark_volumes(scratch_dir)
        system_root = os.environ.get("SystemRoot")
        if system_root:
            volumes["system"] = Path(system_root)
        return volumes

    report = storage_events.save_storage_events(since, output_dir, volumes)
    for line in report.lines():
        print(line)
    if report.unsaved:
        print(f"Saved only part of the storage events in {output_dir}")
    else:
        print(f"Saved the storage events in {output_dir}")
    for title, message in report.warnings():
        print(_annotation("warning", title, message))
    return 0


def _non_negative_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed < 0:
        raise argparse.ArgumentTypeError("must be finite and non-negative")
    return parsed


def configure_parser(parser: argparse.ArgumentParser) -> None:
    commands = parser.add_subparsers(dest="performance_command", required=True)

    collect = commands.add_parser("collect", help="parse benchmark logs into p50 CSV")
    collect.add_argument("--platform", required=True)
    collect.add_argument("--commit", required=True)
    collect.add_argument("--input-dir", type=Path, required=True)
    collect.add_argument("--output-dir", type=Path, required=True)
    collect.add_argument("--require-network", action="store_true")
    collect.add_argument("--require-shell-snapshot", action="store_true")
    collect.add_argument("--require-shared-suite", action="store_true")
    collect.add_argument(
        "--require-shell-snapshot-restore-512",
        action="store_true",
        help="require the canonical restore-only 512 MiB higher-vCPU result",
    )
    collect.add_argument(
        "--lifecycle-input",
        type=Path,
        help="merge a 128 MiB e2e lifecycle benchmark JSON result",
    )
    collect.add_argument(
        "--lifecycle-attempt-log",
        type=Path,
        help=(
            "lifecycle attempt log that must name --lifecycle-input as exactly "
            "one valid attempt; the summary reports every attempt"
        ),
    )
    collect.add_argument("--summary", type=Path)

    validate_openvmm = commands.add_parser(
        "validate-openvmm", help="validate an OpenVMM benchmark JSON result"
    )
    validate_openvmm.add_argument("--platform", required=True)
    validate_openvmm.add_argument("--input", type=Path, required=True)
    validate_openvmm.add_argument(
        "--attempt-log",
        type=Path,
        help="append the validation outcome to this lifecycle attempt log",
    )

    validate_workload = commands.add_parser(
        "validate-workload",
        help="check workload logs for series that a transient host stall split",
    )
    validate_workload.add_argument("--platform", required=True)
    validate_workload.add_argument("--input-dir", type=Path, required=True)

    collect_openvmm = commands.add_parser(
        "collect-openvmm", help="convert an OpenVMM benchmark JSON result to p50 CSV"
    )
    collect_openvmm.add_argument("--platform", required=True)
    collect_openvmm.add_argument("--commit", required=True)
    collect_openvmm.add_argument("--input", type=Path, required=True)
    collect_openvmm.add_argument("--output-dir", type=Path, required=True)
    collect_openvmm.add_argument("--summary", type=Path)

    gate = commands.add_parser("gate", help="check current p50 values for regressions")
    gate.add_argument(
        "--baseline-dir",
        type=Path,
        required=True,
        help="base-branch histories and expected hosts (<platform>-host.json)",
    )
    gate.add_argument("--target-dir", type=Path, required=True)
    gate.add_argument(
        "--history-reset-dir",
        type=Path,
        help=(
            "tracked candidate histories and expected hosts; metrics removed "
            "from an existing history file restart baseline warmup, and a "
            "changed required field of an expected host requires that every "
            "metric of its series restart"
        ),
    )
    gate.add_argument("--window", type=positive_int, default=10)
    gate.add_argument(
        "--minimum-history",
        type=positive_int,
        default=10,
        help="base-branch points required before gating a metric (default: 10)",
    )
    gate.add_argument("--threshold", type=_non_negative_float, default=40)
    gate.add_argument(
        "--absolute-tolerance-ms",
        type=_non_negative_float,
        default=5,
        help=(
            "absolute increase required in addition to --threshold for "
            "lower-is-better millisecond metrics (default: 5)"
        ),
    )
    gate.add_argument("--summary", type=Path)

    persist = commands.add_parser(
        "persist", help="append current p50 values to branch history"
    )
    persist.add_argument("--source-dir", type=Path, required=True)
    persist.add_argument("--history-dir", type=Path, required=True)
    persist.add_argument(
        "--exclude-metric",
        action="append",
        default=[],
        metavar="NAME",
        help="metric to omit from persistence (repeatable)",
    )
    persist.add_argument(
        "--require-host",
        action="store_true",
        help=(
            "require each platform's recorded host in --source-dir and expected "
            "host in --history-dir (<platform>-host.json); without it, only "
            "hosts that both directories record are compared"
        ),
    )

    check = commands.add_parser(
        "check-host",
        help="compare this host with the expected host of a performance series",
    )
    check.add_argument("--platform", required=True)
    check.add_argument(
        "--history-dir",
        type=Path,
        required=True,
        help="directory of the series' expected host, <platform>-host.json",
    )
    check.add_argument(
        "--scratch-dir",
        type=Path,
        help=(
            "benchmark scratch directory that the benchmarks will use "
            "(default: the system temporary directory)"
        ),
    )
    check.add_argument(
        "--output", type=Path, help="write this host as a host file to this path"
    )
    check.add_argument("--summary", type=Path)

    events = commands.add_parser(
        "storage-events",
        help=("save the Windows storage events of a job's window and warn of slow I/O"),
    )
    events.add_argument(
        "--since",
        type=storage_events.parse_utc,
        required=True,
        help="start of the window, an ISO 8601 time with a UTC offset or Z",
    )
    events.add_argument("--output-dir", type=Path, required=True)
    events.add_argument(
        "--scratch-dir",
        type=Path,
        help=(
            "benchmark scratch directory, whose volume the summary names "
            "(default: the system temporary directory)"
        ),
    )
    parser.set_defaults(handler=command_performance)


def command_performance(args: argparse.Namespace) -> int:
    try:
        if args.performance_command == "collect":
            collect_results(
                args.platform,
                _resolve_commit_id(args.commit, ROOT),
                args.input_dir,
                args.output_dir,
                require_network=args.require_network,
                require_shell_snapshot=args.require_shell_snapshot,
                require_shared_suite=args.require_shared_suite,
                summary_path=args.summary,
                lifecycle_input=args.lifecycle_input,
                require_shell_snapshot_restore_512=(
                    args.require_shell_snapshot_restore_512
                ),
                lifecycle_attempt_log=args.lifecycle_attempt_log,
            )
            return 0
        if args.performance_command == "validate-openvmm":
            status, message = 0, None
            try:
                read_lifecycle_data(args.platform, args.input)
            except UnstablePerformanceError as error:
                status, message = UNSTABLE_EXIT_CODE, str(error)
            except PerformanceError as error:
                status, message = 2, str(error)
            if message is not None:
                print(f"ERROR: {message}", file=sys.stderr)
            if args.attempt_log is not None:
                attempt = record_lifecycle_attempt(
                    args.attempt_log, args.platform, args.input, status, message
                )
                print(
                    f"Recorded lifecycle attempt {attempt.number} "
                    f"({attempt.status}) in {args.attempt_log}"
                )
            return status
        if args.performance_command == "validate-workload":
            try:
                validate_workload_stability(args.platform, args.input_dir)
            except UnstablePerformanceError as error:
                for line in str(error).splitlines():
                    print(f"ERROR: {line}", file=sys.stderr)
                return UNSTABLE_EXIT_CODE
            return 0
        if args.performance_command == "collect-openvmm":
            collect_openvmm_results(
                args.platform,
                _resolve_commit_id(args.commit, ROOT),
                args.input,
                args.output_dir,
                args.summary,
            )
            return 0
        if args.performance_command == "gate":
            return gate_results(
                baseline_dir=args.baseline_dir,
                target_dir=args.target_dir,
                window=args.window,
                threshold=args.threshold,
                summary_path=args.summary,
                absolute_tolerance_ms=args.absolute_tolerance_ms,
                minimum_history=args.minimum_history,
                history_reset_dir=args.history_reset_dir,
            )
        if args.performance_command == "check-host":
            return check_host(
                args.platform,
                args.history_dir,
                scratch_dir=args.scratch_dir,
                output=args.output,
                summary_path=args.summary,
            )
        if args.performance_command == "storage-events":
            return save_storage_events(args.since, args.output_dir, args.scratch_dir)
        persist_results(
            args.source_dir,
            args.history_dir,
            args.exclude_metric,
            require_host=args.require_host,
            git_checkout=_git_checkout_of(args.history_dir),
        )
        return 0
    except (PerformanceError, OSError, csv.Error) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2
