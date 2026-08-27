"""Collect, persist, and gate CI performance results."""

from __future__ import annotations

import argparse
import csv
import json
import math
import os
import re
import statistics
import sys
import tempfile
from collections import defaultdict, deque
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

CSV_FIELDS = ["commit", "metric", "unit", "direction", "p50"]
DIRECTIONS = {"lower", "higher"}
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
        "shell_snapshot_cold_64_mib",
        "shell_snapshot_restore_64_mib",
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
BYTES_PER_MIB = 1024 * 1024
LIFECYCLE_MEMORY_MIB = 128
LIFECYCLE_BOOT_MARKER = "ALPINE-MICROVM-BOOT-OK"
LIFECYCLE_RESTORE_MARKER = "OPENVMM-SNAPSHOT-RESTORE-OK"
NUMBER = r"[0-9]+(?:,[0-9]{3})*(?:\.[0-9]+)?"
ANSI_ESCAPE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
SHELL_SNAPSHOT_MEMORIES_MIB = (64, 128, 256, 512)
SHELL_SNAPSHOT_SECTION = re.compile(
    r"^==\s*(?P<memory>[0-9]+)\s+MiB\s*==\s*$"
    r"(?P<body>.*?)(?=^==\s*[0-9]+\s+MiB\s*==\s*$|\Z)",
    re.IGNORECASE | re.MULTILINE | re.DOTALL,
)


class PerformanceError(RuntimeError):
    """Raised when benchmark data is missing or malformed."""


@dataclass(frozen=True)
class Result:
    commit: str
    metric: str
    unit: str
    direction: str
    p50: float


@dataclass(frozen=True)
class LifecycleData:
    document: dict[str, object]
    backend: str
    metrics: dict[str, MetricValue]


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


def _parse_shell_snapshot(text: str) -> dict[str, MetricValue]:
    sections: dict[int, str] = {}
    for match in SHELL_SNAPSHOT_SECTION.finditer(text):
        memory_mib = int(match.group("memory"))
        if memory_mib in sections:
            raise PerformanceError(
                f"duplicate {memory_mib} MiB section in shell-snapshot.log"
            )
        sections[memory_mib] = match.group("body")

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


LOG_PARSERS: dict[str, tuple[Parser, bool]] = {
    "cold-start.log": (_parse_cold_start, True),
    "virtfs.log": (_parse_virtfs, True),
    "snapshot.log": (_parse_snapshot, False),
    "snapshot-hello.log": (_parse_hello_snapshot, False),
    "shell-snapshot.log": (_parse_shell_snapshot, False),
    "network.log": (_parse_network, False),
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
            if reader.fieldnames != CSV_FIELDS:
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
                results.append(
                    Result(
                        commit=row["commit"],
                        metric=row["metric"],
                        unit=row["unit"],
                        direction=row["direction"],
                        p50=p50,
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
    lines: list[str] = [
        f"## {title} benchmark results",
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
            (
                "request_to_publication_p50_ms",
                "request_to_publication_min_ms",
                "request_to_publication_max_ms",
                "request_to_publication_samples_ms",
            ),
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
            "request_to_publication_p50_ms",
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
            value / BYTES_PER_MIB if unit == "MiB" else value,
        )
        for metric, section, field, unit in metric_fields
        for value in [_openvmm_value(document, section, backend, field, input_path)]
    }
    if metrics.keys() != LIFECYCLE_METRICS:
        raise AssertionError("lifecycle metric definition is incomplete")
    return LifecycleData(document, backend, metrics)


def append_openvmm_diagnostics(
    path: Path,
    platform: str,
    lifecycle: LifecycleData,
    source: Path,
) -> None:
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
            f"| {label} | {p50 / BYTES_PER_MIB:.2f} MiB | "
            f"{maximum / BYTES_PER_MIB:.2f} MiB |"
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
    with path.open("a", encoding="utf-8", newline="\n") as output:
        output.write("\n" + "\n".join(lines))


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
    results = [
        Result(commit, metric, unit, direction, p50)
        for metric, (unit, direction, p50) in sorted(lifecycle.metrics.items())
    ]
    output_path = output_dir / f"{platform}.csv"
    write_results(output_path, results)
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
) -> Path:
    if not platform or "/" in platform or platform in {".", ".."}:
        raise PerformanceError(f"invalid platform name: {platform!r}")
    if not commit:
        raise PerformanceError("commit must not be empty")

    required_optional_logs = {
        "network.log": require_network,
        "shell-snapshot.log": require_shell_snapshot,
    }
    collected: dict[str, MetricValue] = {}
    for filename, (parser, required) in LOG_PARSERS.items():
        path = input_dir / filename
        if not path.exists():
            if required or required_optional_logs.get(filename, False):
                raise PerformanceError(f"required benchmark log not found: {path}")
            print(f"SKIP: optional benchmark log not found: {path}")
            continue
        for metric, value in parser(_read_log(path)).items():
            metric = _platform_metric_name(platform, metric)
            if metric in collected:
                raise PerformanceError(f"duplicate collected metric: {metric}")
            collected[metric] = value

    lifecycle = (
        read_lifecycle_data(platform, lifecycle_input)
        if lifecycle_input is not None
        else None
    )
    if lifecycle is not None:
        for metric, value in lifecycle.metrics.items():
            if metric in collected:
                raise PerformanceError(f"duplicate collected metric: {metric}")
            collected[metric] = value

    if not collected:
        raise PerformanceError(f"no performance metrics found in {input_dir}")
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

    results = [
        Result(commit, metric, unit, direction, p50)
        for metric, (unit, direction, p50) in sorted(collected.items())
    ]
    output_path = output_dir / f"{platform}.csv"
    write_results(output_path, results)
    if summary_path is not None:
        append_results_summary(summary_path, platform, results)
        if lifecycle is not None:
            assert lifecycle_input is not None
            append_openvmm_diagnostics(
                summary_path,
                platform,
                lifecycle,
                lifecycle_input,
            )
    print(f"Collected {len(results)} p50 metric(s) for {platform}: {output_path}")
    return output_path


def _validate_current_results(path: Path, results: Sequence[Result]) -> None:
    seen: set[tuple[str, str]] = set()
    for result in results:
        key = (result.commit, result.metric)
        if key in seen:
            raise PerformanceError(
                f"duplicate commit/metric row in {path}: {result.commit}/{result.metric}"
            )
        seen.add(key)


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
                )
            )
        elif result.metric != "virtfs_reuse" or index > transition:
            compatible.append(result)
    return compatible


def persist_results(
    source_dir: Path,
    history_dir: Path,
    excluded_metrics: Iterable[str] = (),
) -> None:
    source_files = sorted(source_dir.glob("*.csv"))
    if not source_files:
        raise PerformanceError(f"no result CSV files found in {source_dir}")
    history_dir.mkdir(parents=True, exist_ok=True)
    excluded = frozenset(excluded_metrics)

    for source_path in source_files:
        source_results = read_results(source_path)
        _validate_current_results(source_path, source_results)
        current = [result for result in source_results if result.metric not in excluded]
        history_path = history_dir / source_path.name
        existing = read_results(history_path) if history_path.exists() else []
        metadata = {
            result.metric: (result.unit, result.direction) for result in existing
        }
        for result in current:
            expected = metadata.get(result.metric)
            actual = (result.unit, result.direction)
            if expected is not None and expected != actual:
                raise PerformanceError(
                    f"metric metadata changed for {result.metric} in {history_path}: "
                    f"{expected} -> {actual}"
                )

        existing_keys = {(result.commit, result.metric) for result in existing}
        new_results = [
            result
            for result in current
            if (result.commit, result.metric) not in existing_keys
        ]
        if not new_results:
            print(f"No new performance rows to persist for {source_path.name}")
            continue
        write_results(history_path, [*existing, *new_results])
        print(f"Persisted {len(new_results)} row(s) to {history_path}")


def _format_value(value: float, unit: str) -> str:
    return f"{value:.2f} {unit}"


def gate_results(
    baseline_dir: Path,
    target_dir: Path,
    window: int,
    threshold: float,
    summary_path: Path | None = None,
    absolute_tolerance_ms: float = 5.0,
) -> int:
    target_files = sorted(target_dir.glob("*.csv"))
    if not target_files:
        raise PerformanceError(f"no result CSV files found in {target_dir}")

    checked = 0
    regressions = 0
    summary = [
        "## Performance regression gate",
        "",
        f"PR p50 versus the base branch's latest {window}-point p50 moving average "
        f"(failure threshold: >{threshold:g}%; lower-is-better millisecond metrics "
        f"must also increase by >{absolute_tolerance_ms:g} ms).",
        "",
        "| Platform | Metric | PR p50 | Base p50 average | Delta | Result |",
        "| --- | --- | ---: | ---: | ---: | --- |",
    ]

    for target_path in target_files:
        platform = target_path.stem
        targets = read_results(target_path)
        _validate_current_results(target_path, targets)
        baseline_path = baseline_dir / target_path.name
        baselines = (
            _compatible_baseline_results(platform, read_results(baseline_path))
            if baseline_path.exists()
            else []
        )
        history: dict[str, deque[Result]] = defaultdict(lambda: deque(maxlen=window))
        for result in baselines:
            history[result.metric].append(result)

        for target in sorted(targets, key=lambda result: result.metric):
            samples = history.get(target.metric)
            if not samples:
                message = (
                    f"WARMUP: {platform}/{target.metric} has no base-branch history"
                )
                print(message)
                summary.append(
                    f"| {platform} | `{target.metric}` | "
                    f"{_format_value(target.p50, target.unit)} | - | - | Warmup |"
                )
                continue

            for sample in samples:
                if (sample.unit, sample.direction) != (
                    target.unit,
                    target.direction,
                ):
                    raise PerformanceError(
                        f"metric metadata differs between target and baseline for "
                        f"{platform}/{target.metric}"
                    )

            baseline_average = statistics.fmean(sample.p50 for sample in samples)
            if target.direction == "lower":
                delta = (target.p50 - baseline_average) / baseline_average * 100
            else:
                delta = (baseline_average - target.p50) / baseline_average * 100

            checked += 1
            absolute_delta_ms = (
                target.p50 - baseline_average
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
                f"{status}: {platform}/{target.metric}: p50 "
                f"{_format_value(target.p50, target.unit)} vs "
                f"{len(samples)}-point base average "
                f"{_format_value(baseline_average, target.unit)} "
                f"({delta:+.1f}%{absolute_detail})"
            )
            summary.append(
                f"| {platform} | `{target.metric}` | "
                f"{_format_value(target.p50, target.unit)} | "
                f"{_format_value(baseline_average, target.unit)} "
                f"({len(samples)}/{window}) | {delta:+.1f}%{absolute_detail} | "
                f"{status} |"
            )

    summary.extend(
        [
            "",
            f"Checked {checked} metric(s); found {regressions} regression(s).",
            "",
        ]
    )
    if summary_path is not None:
        summary_path.parent.mkdir(parents=True, exist_ok=True)
        with summary_path.open("a", encoding="utf-8") as output:
            output.write("\n".join(summary))

    print(
        f"Checked {checked} metric(s), found {regressions} regression(s) "
        f"(threshold: >{threshold:g}% vs {window}-point moving average; "
        f"absolute latency tolerance: {absolute_tolerance_ms:g} ms)."
    )
    return 1 if regressions else 0


def _positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


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
        "--lifecycle-input",
        type=Path,
        help="merge a 128 MiB e2e lifecycle benchmark JSON result",
    )
    collect.add_argument("--summary", type=Path)

    collect_openvmm = commands.add_parser(
        "collect-openvmm", help="convert an OpenVMM benchmark JSON result to p50 CSV"
    )
    collect_openvmm.add_argument("--platform", required=True)
    collect_openvmm.add_argument("--commit", required=True)
    collect_openvmm.add_argument("--input", type=Path, required=True)
    collect_openvmm.add_argument("--output-dir", type=Path, required=True)
    collect_openvmm.add_argument("--summary", type=Path)

    gate = commands.add_parser("gate", help="check current p50 values for regressions")
    gate.add_argument("--baseline-dir", type=Path, required=True)
    gate.add_argument("--target-dir", type=Path, required=True)
    gate.add_argument("--window", type=_positive_int, default=10)
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
    parser.set_defaults(handler=command_performance)


def command_performance(args: argparse.Namespace) -> int:
    try:
        if args.performance_command == "collect":
            collect_results(
                args.platform,
                args.commit,
                args.input_dir,
                args.output_dir,
                require_network=args.require_network,
                require_shell_snapshot=args.require_shell_snapshot,
                require_shared_suite=args.require_shared_suite,
                summary_path=args.summary,
                lifecycle_input=args.lifecycle_input,
            )
            return 0
        if args.performance_command == "collect-openvmm":
            collect_openvmm_results(
                args.platform,
                args.commit,
                args.input,
                args.output_dir,
                args.summary,
            )
            return 0
        if args.performance_command == "gate":
            return gate_results(
                args.baseline_dir,
                args.target_dir,
                args.window,
                args.threshold,
                args.summary,
                args.absolute_tolerance_ms,
            )
        persist_results(args.source_dir, args.history_dir, args.exclude_metric)
        return 0
    except (PerformanceError, OSError, csv.Error) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2
