#!/usr/bin/env python3
"""Collect, persist, and gate CI performance results."""

from __future__ import annotations

import argparse
import csv
import math
import os
import re
import statistics
import sys
import tempfile
from collections import defaultdict, deque
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Sequence


CSV_FIELDS = ["commit", "metric", "unit", "direction", "p50"]
DIRECTIONS = {"lower", "higher"}
NUMBER = r"[0-9]+(?:,[0-9]{3})*(?:\.[0-9]+)?"
ANSI_ESCAPE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
SHELL_SNAPSHOT_MEMORIES_MIB = (64, 128, 256, 512)
HCS_SHELL_SNAPSHOT_MEMORIES_MIB = (256, 512)
SHELL_SNAPSHOT_SECTION = re.compile(
    r"^==\s*(?P<memory>[0-9]+)\s+MiB\s*==\s*$"
    r"(?P<body>.*?)(?=^==\s*[0-9]+\s+MiB\s*==\s*$|\Z)",
    re.IGNORECASE | re.MULTILINE | re.DOTALL,
)


def _snapshot_section(text: str, memory_mib: int, source: str) -> str:
    sections = [
        match.group("body")
        for match in SHELL_SNAPSHOT_SECTION.finditer(text)
        if int(match.group("memory")) == memory_mib
    ]
    if not sections:
        raise PerformanceError(f"missing {memory_mib} MiB section in {source}")
    if len(sections) > 1:
        raise PerformanceError(f"duplicate {memory_mib} MiB section in {source}")
    return sections[0]


class PerformanceError(RuntimeError):
    """Raised when benchmark data is missing or malformed."""


@dataclass(frozen=True)
class Result:
    commit: str
    metric: str
    unit: str
    direction: str
    p50: float


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
        raise PerformanceError(f"cannot decode benchmark log {path}: {error}") from error


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
            "cold_start_kernel_handoff_loud",
            "ms",
            "lower",
            rf"^\s*loud \(rendered\)\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_kernel_handoff_quiet",
            "ms",
            "lower",
            rf"^\s*quiet \(discarded\)\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_shell_loud",
            "ms",
            "lower",
            rf"^\s*loud full logs\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_shell_silent",
            "ms",
            "lower",
            rf"^\s*silent \(quiet klog\)\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "cold_start_fast",
            "ms",
            "lower",
            rf"^\s*fast\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
    ]
    return _parse_fixed(text, "cold-start.log", patterns)


def _parse_snapshot(text: str) -> dict[str, MetricValue]:
    patterns = [
        (
            "python_snapshot_cold",
            "ms",
            "lower",
            rf"^\s*cold\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
        (
            "python_snapshot_restore",
            "ms",
            "lower",
            rf"^\s*restore\s*:\s*(?P<value>{NUMBER})\s*ms\b",
        ),
    ]
    return _parse_fixed(text, "snapshot.log", patterns)


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


def _parse_hcs_summary(
    text: str, source: str, prefix: str
) -> dict[str, MetricValue]:
    return _parse_fixed(
        text,
        source,
        [
            (
                f"{prefix}_cold_guest",
                "ms",
                "lower",
                rf"^\s*cold guest latency\s*:\s*(?P<value>{NUMBER})\s*ms\b",
            ),
            (
                f"{prefix}_cold_wall",
                "ms",
                "lower",
                rf"^\s*cold process wall\s*:\s*(?P<value>{NUMBER})\s*ms\b",
            ),
            (
                f"{prefix}_restore_guest",
                "ms",
                "lower",
                rf"^\s*restore guest latency\s*:\s*(?P<value>{NUMBER})\s*ms\b",
            ),
            (
                f"{prefix}_restore_wall",
                "ms",
                "lower",
                rf"^\s*restore process wall\s*:\s*(?P<value>{NUMBER})\s*ms\b",
            ),
        ],
    )


def _parse_hcs_shell_snapshot(text: str) -> dict[str, MetricValue]:
    metrics: dict[str, MetricValue] = {}
    for memory_mib in HCS_SHELL_SNAPSHOT_MEMORIES_MIB:
        metrics.update(
            _parse_hcs_summary(
                _snapshot_section(text, memory_mib, "hcs-shell-snapshot.log"),
                f"hcs-shell-snapshot.log ({memory_mib} MiB)",
                f"hcs_shell_{memory_mib}_mib",
            )
        )
    return metrics


def _parse_hcs_python_snapshot(text: str) -> dict[str, MetricValue]:
    return _parse_hcs_summary(text, "hcs-python-snapshot.log", "hcs_python")


def _parse_hcs_network_snapshot(text: str) -> dict[str, MetricValue]:
    if re.search(r"^\s*verified marker\s*:\s*HELLOPY-NET OK\s*$", text, re.MULTILINE) is None:
        raise PerformanceError(
            "missing verified marker 'HELLOPY-NET OK' in hcs-network-snapshot.log"
        )
    return _parse_hcs_summary(text, "hcs-network-snapshot.log", "hcs_network")


def _parse_hcn_afxdp(text: str) -> dict[str, MetricValue]:
    if re.search(
        r"^\s*verified marker\s*:\s*NVX-HCN-AFXDP-SMOKE-OK\s*$",
        text,
        re.MULTILINE,
    ) is None:
        raise PerformanceError(
            "missing verified marker 'NVX-HCN-AFXDP-SMOKE-OK' in hcn-afxdp.log"
        )
    return _parse_fixed(
        text,
        "hcn-afxdp.log",
        [
            (
                "hcn_afxdp_verified_network_wall",
                "ms",
                "lower",
                rf"^\s*verified network wall\s*:\s*(?P<value>{NUMBER})\s*ms\b",
            )
        ],
    )


def _parse_virtfs(text: str) -> dict[str, MetricValue]:
    write_pattern = re.compile(
        rf"^\s*rw\s+(?P<kind>ephemeral|persistent)\b.*?\bwrite\s+"
        rf"(?:(?P<value>{NUMBER})\s+MB/s|(?P<na>n/a)\b)",
        re.IGNORECASE,
    )
    read_pattern = re.compile(
        rf"^\s*read\s+(?:(?P<value>{NUMBER})\s+MB/s|(?P<na>n/a)\b)",
        re.IGNORECASE,
    )
    metrics: dict[str, MetricValue] = {}
    seen: set[str] = set()
    current_kind: str | None = None

    for line in text.splitlines():
        write_match = write_pattern.search(line)
        if write_match is not None:
            current_kind = write_match.group("kind").lower()
            key = f"{current_kind}_write"
            seen.add(key)
            if write_match.group("value") is not None:
                metrics[f"virtfs_{key}"] = (
                    "MB/s",
                    "higher",
                    _number(write_match.group("value")),
                )
            continue

        read_match = read_pattern.search(line)
        if read_match is not None and current_kind is not None:
            key = f"{current_kind}_read"
            seen.add(key)
            if read_match.group("value") is not None:
                metrics[f"virtfs_{key}"] = (
                    "MB/s",
                    "higher",
                    _number(read_match.group("value")),
                )

    expected = {
        "ephemeral_write",
        "ephemeral_read",
        "persistent_write",
        "persistent_read",
    }
    missing = sorted(expected - seen)
    if missing:
        raise PerformanceError(
            f"missing virt-fs throughput result(s) in virtfs.log: {', '.join(missing)}"
        )

    metrics.update(
        _parse_fixed(
            text,
            "virtfs.log",
            [
                (
                    "virtfs_reuse",
                    "ms",
                    "lower",
                    rf"^\s*reuse image \+ verify \(cold each\)\s*:\s*"
                    rf"(?P<value>{NUMBER})\s*ms\b",
                )
            ],
        )
    )
    return metrics


LOG_PARSERS: dict[str, tuple[Parser, bool]] = {
    "cold-start.log": (_parse_cold_start, True),
    "virtfs.log": (_parse_virtfs, True),
    "snapshot.log": (_parse_snapshot, True),
    "shell-snapshot.log": (_parse_shell_snapshot, False),
    "network.log": (_parse_network, False),
}

HCS_LOG_PARSERS: dict[str, tuple[Parser, bool]] = {
    "hcs-shell-snapshot.log": (_parse_hcs_shell_snapshot, True),
    "hcs-python-snapshot.log": (_parse_hcs_python_snapshot, True),
    "hcs-network-snapshot.log": (_parse_hcs_network_snapshot, False),
}

HCN_AFXDP_LOG_PARSERS: dict[str, tuple[Parser, bool]] = {
    "hcn-afxdp.log": (_parse_hcn_afxdp, True),
}

PLATFORM_NAMES = {
    "linux-kvm": "Linux / KVM",
    "windows-whp": "Windows / WHP",
    "windows-hcs": "Windows / HCS",
    "windows-hcn-afxdp": "Windows / HCN + AF_XDP",
}


def _platform_metric_name(platform: str, metric: str) -> str:
    # Earlier Windows virt-fs runs could queue commands before the guest shell was ready and record
    # a fast no-op as `virtfs_reuse`. Keep Linux history intact, but warm a new Windows baseline for
    # the now fail-fast, checksum-verified workload.
    if platform == "windows-whp" and metric == "virtfs_reuse":
        return "virtfs_verified_reuse"
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
            results = []
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
    lines = [
        f"## {title} benchmark results",
        "",
        "| Metric | p50 | Preferred direction |",
        "| --- | ---: | --- |",
    ]
    for result in results:
        direction = (
            "Lower is better"
            if result.direction == "lower"
            else "Higher is better"
        )
        lines.append(
            f"| `{result.metric}` | {_format_value(result.p50, result.unit)} | {direction} |"
        )
    lines.extend(["", f"Commit: `{results[0].commit}`", ""])
    with path.open("a", encoding="utf-8", newline="\n") as output:
        output.write("\n" + "\n".join(lines))


def collect_results(
    platform: str,
    commit: str,
    input_dir: Path,
    output_dir: Path,
    require_network: bool = False,
    require_shell_snapshot: bool = False,
    summary_path: Path | None = None,
) -> Path:
    if not platform or "/" in platform or platform in {".", ".."}:
        raise PerformanceError(f"invalid platform name: {platform!r}")
    if not commit:
        raise PerformanceError("commit must not be empty")

    required_optional_logs = {
        "network.log": require_network,
        "hcs-network-snapshot.log": require_network,
        "shell-snapshot.log": require_shell_snapshot,
    }
    platform_parsers = {
        "windows-hcs": HCS_LOG_PARSERS,
        "windows-hcn-afxdp": HCN_AFXDP_LOG_PARSERS,
    }
    parsers = platform_parsers.get(platform, LOG_PARSERS)
    collected: dict[str, MetricValue] = {}
    for filename, (parser, required) in parsers.items():
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

    if not collected:
        raise PerformanceError(f"no performance metrics found in {input_dir}")

    results = [
        Result(commit, metric, unit, direction, p50)
        for metric, (unit, direction, p50) in sorted(collected.items())
    ]
    output_path = output_dir / f"{platform}.csv"
    write_results(output_path, results)
    if summary_path is not None:
        append_results_summary(summary_path, platform, results)
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


def persist_results(source_dir: Path, history_dir: Path) -> None:
    source_files = sorted(source_dir.glob("*.csv"))
    if not source_files:
        raise PerformanceError(f"no result CSV files found in {source_dir}")
    history_dir.mkdir(parents=True, exist_ok=True)

    for source_path in source_files:
        current = read_results(source_path)
        _validate_current_results(source_path, current)
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
        f"(failure threshold: >{threshold:g}%).",
        "",
        "| Platform | Metric | PR p50 | Base p50 average | Delta | Result |",
        "| --- | --- | ---: | ---: | ---: | --- |",
    ]

    for target_path in target_files:
        platform = target_path.stem
        targets = read_results(target_path)
        _validate_current_results(target_path, targets)
        baseline_path = baseline_dir / target_path.name
        baselines = read_results(baseline_path) if baseline_path.exists() else []
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
            regressed = delta > threshold
            regressions += int(regressed)
            status = "REGRESSION" if regressed else "OK"
            print(
                f"{status}: {platform}/{target.metric}: p50 "
                f"{_format_value(target.p50, target.unit)} vs "
                f"{len(samples)}-point base average "
                f"{_format_value(baseline_average, target.unit)} ({delta:+.1f}%)"
            )
            summary.append(
                f"| {platform} | `{target.metric}` | "
                f"{_format_value(target.p50, target.unit)} | "
                f"{_format_value(baseline_average, target.unit)} "
                f"({len(samples)}/{window}) | {delta:+.1f}% | {status} |"
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
        f"(threshold: >{threshold:g}% vs {window}-point moving average)."
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


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    collect = commands.add_parser("collect", help="parse benchmark logs into p50 CSV")
    collect.add_argument("--platform", required=True)
    collect.add_argument("--commit", required=True)
    collect.add_argument("--input-dir", type=Path, required=True)
    collect.add_argument("--output-dir", type=Path, required=True)
    collect.add_argument("--require-network", action="store_true")
    collect.add_argument("--require-shell-snapshot", action="store_true")
    collect.add_argument("--summary", type=Path)

    gate = commands.add_parser("gate", help="check current p50 values for regressions")
    gate.add_argument("--baseline-dir", type=Path, required=True)
    gate.add_argument("--target-dir", type=Path, required=True)
    gate.add_argument("--window", type=_positive_int, default=10)
    gate.add_argument("--threshold", type=_non_negative_float, default=40)
    gate.add_argument("--summary", type=Path)

    persist = commands.add_parser(
        "persist", help="append current p50 values to branch history"
    )
    persist.add_argument("--source-dir", type=Path, required=True)
    persist.add_argument("--history-dir", type=Path, required=True)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        if args.command == "collect":
            collect_results(
                args.platform,
                args.commit,
                args.input_dir,
                args.output_dir,
                require_network=args.require_network,
                require_shell_snapshot=args.require_shell_snapshot,
                summary_path=args.summary,
            )
            return 0
        if args.command == "gate":
            return gate_results(
                args.baseline_dir,
                args.target_dir,
                args.window,
                args.threshold,
                args.summary,
            )
        persist_results(args.source_dir, args.history_dir)
        return 0
    except (PerformanceError, OSError, csv.Error) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
