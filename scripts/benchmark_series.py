#!/usr/bin/env python3
"""Run and verify a reproducible NVX benchmark series on the current host.

The tool turns the benchmark action's bounded validation and retry contract into
one cross-platform command. It keeps every measurement attempt in a fresh result
directory, collects only validated attempts, and checks the retained evidence
before reporting its location.
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import math
import os
import platform as host_platform
import re
import shlex
import shutil
import subprocess
import sys
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO, Literal, Protocol, cast

import host_probe

REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
ACTION_PATH = REPOSITORY_ROOT / ".github" / "actions" / "run-benchmark" / "action.yml"
NVX_SCRIPT = Path("scripts") / "nvx.py"
DEFAULT_OUTPUT_ROOT = Path("data") / "runs" / "manual"
PROFILES = ("smoke", "ci", "canonical")
BACKENDS = ("kvm", "mshv", "whp")
HOST_TYPES = ("baremetal", "virtual-machine")
TEARDOWN_MODES = ("guest-exit", "host-terminate", "host-sigterm")
PERFORMANCE_LOGS = (
    "cold-start.log",
    "virtfs.log",
    "shell-snapshot.log",
    "shell-snapshot-restore.log",
    "network.log",
)
SHA256 = re.compile(r"[0-9a-f]{64}")

Profile = Literal["smoke", "ci", "canonical"]
Backend = Literal["kvm", "mshv", "whp"]
HostType = Literal["baremetal", "virtual-machine"]
OperatingSystem = Literal["Linux", "Windows"]


class SeriesError(RuntimeError):
    """An expected preflight, configuration, or result validation failed."""


@dataclass(frozen=True)
class CommandResult:
    """The status and captured text of one subprocess."""

    returncode: int
    stdout: str = ""
    stderr: str = ""


class Runner(Protocol):
    """Run commands while allowing tests to replace every subprocess."""

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        capture_output: bool,
    ) -> CommandResult: ...


class SubprocessRunner:
    """Run a command in the repository, streaming benchmark output by default."""

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        capture_output: bool,
    ) -> CommandResult:
        completed = subprocess.run(
            list(args),
            cwd=cwd,
            stdout=subprocess.PIPE if capture_output else None,
            stderr=subprocess.PIPE if capture_output else None,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        return CommandResult(
            completed.returncode,
            completed.stdout or "",
            completed.stderr or "",
        )


@dataclass(frozen=True)
class ActionDefaults:
    """Benchmark controls supplied by the action's input defaults."""

    teardown_mode: str
    cpus: str | None
    host_cpu_reserve: int
    timeout: str
    runs: int
    warmups: int


@dataclass(frozen=True)
class ProfileSettings:
    """Sampling and host controls for one benchmark profile."""

    acceptance_warmups: int
    acceptance_runs: int
    performance_warmups: int
    performance_runs: int
    virtfs_runs: int
    device_warmups: int
    device_runs: int
    device_duration_seconds: int
    device_size_mib: int | None
    multi_warmups: int
    multi_runs: int
    with_multi_vcpu: bool
    strict_collection: bool
    persist: bool
    cpus: str | None
    host_cpu_reserve: int
    timeout: str
    teardown_mode: str


@dataclass(frozen=True)
class CliOptions:
    """Typed command-line options."""

    profile: Profile
    backend: Backend
    host_type: HostType
    output_root: Path
    with_multi_vcpu: bool
    scratch_dir: Path | None
    cpus: str | None
    host_cpu_reserve: int | None
    timeout: str | None
    teardown_mode: str | None
    dry_run: bool
    json_output: bool
    repo: Path | None = None


@dataclass(frozen=True)
class SeriesPlan:
    """Resolved paths, identity, and settings of one series."""

    profile: Profile
    backend: Backend
    host_type: HostType
    system: OperatingSystem
    platform: str
    commit: str
    dirty_paths: tuple[str, ...]
    run_id: str
    repository_root: Path
    output_dir: Path
    scratch_dir: Path | None
    automatic_scratch: bool
    settings: ProfileSettings

    @property
    def acceptance_output(self) -> Path:
        return self.output_dir / "acceptance.json"

    @property
    def acceptance_attempt_log(self) -> Path:
        return self.output_dir / "acceptance-attempts.json"

    @property
    def performance_dir(self) -> Path:
        return self.output_dir / "performance"

    @property
    def performance_results_dir(self) -> Path:
        return self.performance_dir / "results"

    @property
    def device_dir(self) -> Path:
        return self.output_dir / "device-io"

    @property
    def device_results_dir(self) -> Path:
        return self.device_dir / "results"

    @property
    def summary_path(self) -> Path:
        return self.output_dir / "summary.md"

    def multi_dir(self, processors: int) -> Path:
        return self.output_dir / f"{processors}vcpu"

    def multi_results_dir(self, processors: int) -> Path:
        return self.multi_dir(processors) / "results"


@dataclass(frozen=True)
class PlannedCommand:
    """One command in a dry-run plan, including its execution condition."""

    step: str
    argv: tuple[str, ...]
    condition: str = "always"
    attempt: int | None = None


@dataclass(frozen=True)
class CommandRecord:
    """One command that ran and its status."""

    step: str
    argv: tuple[str, ...]
    status: int
    attempt: int | None = None


@dataclass(frozen=True)
class ResultCheck:
    """One retained-result check and its diagnostic."""

    name: str
    passed: bool
    detail: str


@dataclass
class ExecutionReport:
    """Commands, attempts, checks, and final status of an execution."""

    status: int = 0
    commands: list[CommandRecord] = field(default_factory=lambda: list[CommandRecord]())
    attempts: dict[str, int] = field(default_factory=lambda: dict[str, int]())
    checks: list[ResultCheck] = field(default_factory=lambda: list[ResultCheck]())


class SeriesExecutor:
    """Execute series commands and record their statuses."""

    def __init__(
        self,
        plan: SeriesPlan,
        runner: Runner,
        report: ExecutionReport,
        output: IO[str],
    ) -> None:
        self.plan = plan
        self.runner = runner
        self.report = report
        self.output = output

    def run(
        self,
        step: str,
        argv: Sequence[str],
        *,
        attempt: int | None = None,
    ) -> int:
        command = tuple(argv)
        print(f"RUN: {_format_command(command, self.plan.system)}", file=self.output)
        result = self.runner.run(
            command,
            cwd=self.plan.repository_root,
            capture_output=False,
        )
        self.report.commands.append(
            CommandRecord(step, command, result.returncode, attempt)
        )
        return result.returncode


def _mapping(value: object, location: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise SeriesError(f"{location} must be a mapping")
    return cast(dict[str, object], value)


def _integer(value: object, location: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, (int, str)):
        raise SeriesError(f"{location} must be an integer")
    try:
        parsed = int(value)
    except ValueError as error:
        raise SeriesError(f"{location} must be an integer") from error
    if parsed < minimum:
        raise SeriesError(f"{location} must be at least {minimum}")
    return parsed


def _text(value: object, location: str) -> str:
    if not isinstance(value, (str, int, float)) or isinstance(value, bool):
        raise SeriesError(f"{location} must be text")
    result = str(value)
    if not result:
        raise SeriesError(f"{location} must not be empty")
    return result


def _action_default(inputs: dict[str, object], name: str) -> object:
    entry = _mapping(inputs.get(name), f"action input {name!r}")
    if "default" not in entry:
        raise SeriesError(f"action input {name!r} has no default")
    return entry["default"]


def load_action_defaults(path: Path = ACTION_PATH) -> ActionDefaults:
    """Read every CI sampling and host-control default from the action."""
    # Only the ci profile reads the action, so the others need no PyYAML,
    # which host setup does not install.
    try:
        import yaml
    except ImportError as error:
        raise SeriesError(
            f"reading CI's sampling from {path} needs PyYAML; install it with "
            "python3 -m pip install --requirement "
            "scripts/setup/azure/requirements.txt"
        ) from error
    try:
        loaded = cast(object, yaml.safe_load(path.read_text(encoding="utf-8")))
    except OSError as error:
        raise SeriesError(f"cannot read benchmark action {path}: {error}") from error
    except yaml.YAMLError as error:
        raise SeriesError(f"invalid benchmark action YAML {path}: {error}") from error
    document = _mapping(loaded, str(path))
    inputs = _mapping(document.get("inputs"), f"{path}:inputs")
    cpus_value = _action_default(inputs, "cpus")
    if not isinstance(cpus_value, str):
        raise SeriesError("action input 'cpus' default must be text")
    teardown_mode = _text(
        _action_default(inputs, "teardown-mode"),
        "action input 'teardown-mode' default",
    )
    if teardown_mode not in TEARDOWN_MODES:
        raise SeriesError(
            f"action input 'teardown-mode' has unsupported default {teardown_mode!r}"
        )
    timeout = _text(
        _action_default(inputs, "marker-timeout"),
        "action input 'marker-timeout' default",
    )
    try:
        parsed_timeout = float(timeout)
    except ValueError as error:
        raise SeriesError(
            "action input 'marker-timeout' default must be a positive number"
        ) from error
    if not math.isfinite(parsed_timeout) or parsed_timeout <= 0:
        raise SeriesError(
            "action input 'marker-timeout' default must be a positive number"
        )
    return ActionDefaults(
        teardown_mode=teardown_mode,
        cpus=cpus_value.strip() or None,
        host_cpu_reserve=_integer(
            _action_default(inputs, "host-cpu-reserve"),
            "action input 'host-cpu-reserve' default",
        ),
        timeout=timeout,
        runs=_integer(
            _action_default(inputs, "runs"),
            "action input 'runs' default",
            minimum=1,
        ),
        warmups=_integer(
            _action_default(inputs, "warmups"),
            "action input 'warmups' default",
        ),
    )


def _profile_settings(
    options: CliOptions,
    action_path: Path,
) -> ProfileSettings:
    if options.profile == "ci":
        defaults = load_action_defaults(action_path)
        acceptance_warmups = defaults.warmups
        acceptance_runs = defaults.runs
        performance_warmups = defaults.warmups
        performance_runs = defaults.runs
        virtfs_runs = defaults.runs
        device_warmups = defaults.warmups
        device_runs = defaults.runs
        multi_warmups = defaults.warmups
        multi_runs = defaults.runs
        cpus = defaults.cpus
        host_cpu_reserve = defaults.host_cpu_reserve
        timeout = defaults.timeout
        teardown_mode = defaults.teardown_mode
        with_multi_vcpu = True
        strict_collection = True
        persist = True
        device_duration = 1
        device_size: int | None = None
    elif options.profile == "canonical":
        acceptance_warmups = 1
        acceptance_runs = 10
        performance_warmups = 1
        performance_runs = 5
        virtfs_runs = 3
        device_warmups = 5
        device_runs = 30
        # The collector accepts the 512 MiB restore at 2, 4, and 8 vCPUs only
        # with CI's sampling, one warmup and ten runs.
        multi_warmups = 1
        multi_runs = 10
        cpus = None
        host_cpu_reserve = 2
        timeout = "10"
        teardown_mode = "guest-exit"
        with_multi_vcpu = options.with_multi_vcpu
        strict_collection = False
        persist = True
        device_duration = 10
        device_size = 512
    else:
        acceptance_warmups = 0
        acceptance_runs = 1
        performance_warmups = 0
        performance_runs = 1
        virtfs_runs = 1
        device_warmups = 0
        device_runs = 1
        multi_warmups = 0
        multi_runs = 1
        cpus = None
        host_cpu_reserve = 2
        timeout = "10"
        teardown_mode = "guest-exit"
        with_multi_vcpu = options.with_multi_vcpu
        strict_collection = False
        persist = False
        device_duration = 1
        device_size = None

    return ProfileSettings(
        acceptance_warmups=acceptance_warmups,
        acceptance_runs=acceptance_runs,
        performance_warmups=performance_warmups,
        performance_runs=performance_runs,
        virtfs_runs=virtfs_runs,
        device_warmups=device_warmups,
        device_runs=device_runs,
        device_duration_seconds=device_duration,
        device_size_mib=device_size,
        multi_warmups=multi_warmups,
        multi_runs=multi_runs,
        with_multi_vcpu=with_multi_vcpu,
        strict_collection=strict_collection,
        persist=persist,
        cpus=options.cpus if options.cpus is not None else cpus,
        host_cpu_reserve=(
            options.host_cpu_reserve
            if options.host_cpu_reserve is not None
            else host_cpu_reserve
        ),
        timeout=options.timeout if options.timeout is not None else timeout,
        teardown_mode=(
            options.teardown_mode
            if options.teardown_mode is not None
            else teardown_mode
        ),
    )


def _nonnegative_int(value: str) -> int:
    try:
        parsed = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be an integer") from error
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be nonnegative")
    return parsed


def _positive_number(value: str) -> str:
    try:
        parsed = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be a positive number") from error
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("must be a positive number")
    return value


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Run a fresh NVX benchmark series with CI validation and bounded retry"
        )
    )
    parser.add_argument("--profile", choices=PROFILES, required=True)
    parser.add_argument("--backend", choices=BACKENDS, required=True)
    parser.add_argument("--host-type", choices=HOST_TYPES, required=True)
    parser.add_argument(
        "--output-root", type=Path, default=DEFAULT_OUTPUT_ROOT, metavar="DIR"
    )
    parser.add_argument("--with-multi-vcpu", action="store_true")
    parser.add_argument("--scratch-dir", type=Path, metavar="DIR")
    parser.add_argument("--cpus", metavar="LIST")
    parser.add_argument("--host-cpu-reserve", type=_nonnegative_int, metavar="N")
    parser.add_argument("--timeout", type=_positive_number, metavar="SECONDS")
    parser.add_argument("--teardown-mode", choices=TEARDOWN_MODES, metavar="MODE")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--json", action="store_true", dest="json_output")
    parser.add_argument(
        "--repo",
        type=Path,
        metavar="DIR",
        help="NVX checkout to measure (default: the checkout holding this script)",
    )
    return parser


def _options(argv: Sequence[str] | None) -> CliOptions:
    parser = _parser()
    parsed = parser.parse_args(argv)
    if parsed.profile == "smoke" and parsed.with_multi_vcpu:
        parser.error(
            "--with-multi-vcpu needs the ci or canonical profile: the collector "
            "accepts the multi-vCPU restore only with CI's sampling"
        )
    cpus_value = cast(str | None, parsed.cpus)
    cpus = cpus_value.strip() if cpus_value is not None else None
    return CliOptions(
        profile=cast(Profile, parsed.profile),
        backend=cast(Backend, parsed.backend),
        host_type=cast(HostType, parsed.host_type),
        output_root=cast(Path, parsed.output_root),
        with_multi_vcpu=cast(bool, parsed.with_multi_vcpu),
        scratch_dir=cast(Path | None, parsed.scratch_dir),
        cpus=cpus or None,
        host_cpu_reserve=cast(int | None, parsed.host_cpu_reserve),
        timeout=cast(str | None, parsed.timeout),
        teardown_mode=cast(str | None, parsed.teardown_mode),
        dry_run=cast(bool, parsed.dry_run),
        json_output=cast(bool, parsed.json_output),
        repo=cast(Path | None, parsed.repo),
    )


def _operating_system(name: str) -> OperatingSystem:
    if name == "Windows":
        return "Windows"
    if name == "Linux":
        return "Linux"
    raise SeriesError(f"unsupported host operating system: {name}")


def _platform_for(
    system: OperatingSystem,
    backend: Backend,
    host_type: HostType,
) -> str:
    if system == "Windows" and backend != "whp":
        raise SeriesError(f"backend {backend} is unsupported on Windows; use whp")
    if system == "Linux" and backend == "whp":
        raise SeriesError("backend whp is unsupported on Linux; use kvm or mshv")
    prefix = "windows" if backend == "whp" else "linux"
    return f"{prefix}-{backend}-{host_type}"


def _command_detail(result: CommandResult) -> str:
    return result.stderr.strip() or result.stdout.strip() or "no diagnostic output"


def _git(runner: Runner, root: Path, *args: str) -> str:
    result = runner.run(
        ["git", *args],
        cwd=root,
        capture_output=True,
    )
    if result.returncode != 0:
        raise SeriesError(f"git {' '.join(args)} failed: {_command_detail(result)}")
    return result.stdout.rstrip("\r\n")


def _make_plan(
    options: CliOptions,
    *,
    runner: Runner,
    repository_root: Path,
    action_path: Path,
    system_name: str,
    now: dt.datetime,
    environment: Mapping[str, str],
) -> SeriesPlan:
    system = _operating_system(system_name)
    platform = _platform_for(system, options.backend, options.host_type)
    commit = _git(runner, repository_root, "rev-parse", "HEAD")
    short_head = _git(runner, repository_root, "rev-parse", "--short", "HEAD")
    dirty = tuple(
        line
        for line in _git(
            runner,
            repository_root,
            "status",
            "--porcelain",
        ).splitlines()
        if line
    )
    if not commit or not short_head:
        raise SeriesError("git returned an empty commit identity")
    timestamp = now.astimezone(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_id = f"{timestamp}-{short_head}"
    # The series' commands run in the repository, so a relative output root
    # names a directory there, whatever this process's working directory.
    output_root = (
        options.output_root
        if options.output_root.is_absolute()
        else repository_root / options.output_root
    )
    output_dir = output_root / f"{run_id}-{platform}"
    scratch_dir = options.scratch_dir
    automatic_scratch = False
    if scratch_dir is None and system == "Windows":
        scratch_root = environment.get("NVX_BENCHMARK_SCRATCH", "").strip()
        if scratch_root:
            scratch_dir = Path(scratch_root) / f"{run_id}-{platform}"
            automatic_scratch = True
    return SeriesPlan(
        profile=options.profile,
        backend=options.backend,
        host_type=options.host_type,
        system=system,
        platform=platform,
        commit=commit,
        dirty_paths=dirty,
        run_id=run_id,
        repository_root=repository_root,
        output_dir=output_dir,
        scratch_dir=scratch_dir,
        automatic_scratch=automatic_scratch,
        settings=_profile_settings(options, action_path),
    )


def _nvx(*args: str) -> tuple[str, ...]:
    return (sys.executable, str(NVX_SCRIPT), *args)


def _cpu_arguments(plan: SeriesPlan) -> list[str]:
    result = ["--host-cpu-reserve", str(plan.settings.host_cpu_reserve)]
    if plan.settings.cpus is not None:
        result.extend(("--cpus", plan.settings.cpus))
    return result


def _scratch_arguments(plan: SeriesPlan) -> list[str]:
    if plan.scratch_dir is None:
        return []
    return ["--scratch-dir", str(plan.scratch_dir)]


def _benchmark_arguments(
    plan: SeriesPlan,
    *,
    suite: str,
    processors: int,
    warmups: int,
    runs: int,
    output_option: str,
    output_path: Path,
    extra: Sequence[str] = (),
    snapshot_profile: bool = False,
) -> tuple[str, ...]:
    args = [
        sys.executable,
        str(NVX_SCRIPT),
        "benchmark",
        "--suite",
        suite,
        "--backend",
        plan.backend,
        "--platform",
        plan.platform,
        "--processors",
        str(processors),
    ]
    if plan.system == "Linux":
        args.extend(_cpu_arguments(plan))
    args.extend(
        (
            "--warmups",
            str(warmups),
            "--runs",
            str(runs),
            *extra,
            "--timeout",
            plan.settings.timeout,
            "--skip-build",
            "--teardown-mode",
            plan.settings.teardown_mode,
        )
    )
    if snapshot_profile:
        args.append("--snapshot-profile")
    args.extend((output_option, str(output_path)))
    if plan.system == "Windows":
        args.extend(_cpu_arguments(plan))
    args.extend(_scratch_arguments(plan))
    return tuple(args)


def _acceptance_attempt(plan: SeriesPlan, attempt: int) -> Path:
    return plan.output_dir / f"acceptance-attempt-{attempt}.json"


def _acceptance_benchmark(plan: SeriesPlan, attempt: int) -> tuple[str, ...]:
    return _benchmark_arguments(
        plan,
        suite="e2e",
        processors=1,
        warmups=plan.settings.acceptance_warmups,
        runs=plan.settings.acceptance_runs,
        output_option="--output",
        output_path=_acceptance_attempt(plan, attempt),
        snapshot_profile=True,
    )


def _acceptance_validation(plan: SeriesPlan, attempt: int) -> tuple[str, ...]:
    return _nvx(
        "performance",
        "validate-openvmm",
        "--platform",
        plan.platform,
        "--input",
        str(_acceptance_attempt(plan, attempt)),
        "--attempt-log",
        str(plan.acceptance_attempt_log),
    )


def _performance_attempt(plan: SeriesPlan, attempt: int) -> Path:
    return plan.performance_dir / f"performance-attempt-{attempt}"


def _performance_benchmark(plan: SeriesPlan, attempt: int) -> tuple[str, ...]:
    return _benchmark_arguments(
        plan,
        suite="performance",
        processors=1,
        warmups=plan.settings.performance_warmups,
        runs=plan.settings.performance_runs,
        extra=("--virtfs-runs", str(plan.settings.virtfs_runs)),
        output_option="--output-dir",
        output_path=_performance_attempt(plan, attempt),
    )


def _performance_validation(plan: SeriesPlan, attempt: int) -> tuple[str, ...]:
    return _nvx(
        "performance",
        "validate-workload",
        "--platform",
        plan.platform,
        "--input-dir",
        str(_performance_attempt(plan, attempt)),
    )


def _multi_attempt(plan: SeriesPlan, processors: int, attempt: int) -> Path:
    return plan.multi_dir(processors) / f"shell-snapshot-restore-attempt-{attempt}"


def _multi_benchmark(
    plan: SeriesPlan,
    processors: int,
    attempt: int,
) -> tuple[str, ...]:
    return _benchmark_arguments(
        plan,
        suite="shell-snapshot-restore",
        processors=processors,
        warmups=plan.settings.multi_warmups,
        runs=plan.settings.multi_runs,
        extra=("--shell-memories", "512"),
        output_option="--output-dir",
        output_path=_multi_attempt(plan, processors, attempt),
    )


def _multi_validation(
    plan: SeriesPlan,
    processors: int,
    attempt: int,
) -> tuple[str, ...]:
    return _nvx(
        "performance",
        "validate-workload",
        "--platform",
        plan.platform,
        "--input-dir",
        str(_multi_attempt(plan, processors, attempt)),
    )


def _device_benchmark(plan: SeriesPlan) -> tuple[str, ...]:
    extra = [
        "--device-io-duration-seconds",
        str(plan.settings.device_duration_seconds),
    ]
    if plan.settings.device_size_mib is not None:
        extra.extend(("--device-io-size-mib", str(plan.settings.device_size_mib)))
    return _benchmark_arguments(
        plan,
        suite="device-io",
        processors=1,
        warmups=plan.settings.device_warmups,
        runs=plan.settings.device_runs,
        extra=extra,
        output_option="--output-dir",
        output_path=plan.device_dir,
    )


def _performance_collection(plan: SeriesPlan) -> tuple[str, ...]:
    args = [
        sys.executable,
        str(NVX_SCRIPT),
        "performance",
        "collect",
        "--platform",
        plan.platform,
        "--commit",
        plan.commit,
        "--input-dir",
        str(plan.performance_dir),
        "--output-dir",
        str(plan.performance_results_dir),
        "--require-network",
        "--require-shell-snapshot",
    ]
    if plan.settings.strict_collection:
        args.append("--require-shared-suite")
    args.extend(
        (
            "--lifecycle-input",
            str(plan.acceptance_output),
            "--lifecycle-attempt-log",
            str(plan.acceptance_attempt_log),
            "--summary",
            str(plan.summary_path),
        )
    )
    return tuple(args)


def _device_collection(plan: SeriesPlan) -> tuple[str, ...]:
    return _nvx(
        "performance",
        "collect",
        "--platform",
        plan.platform,
        "--commit",
        plan.commit,
        "--input-dir",
        str(plan.device_dir),
        "--output-dir",
        str(plan.device_results_dir),
        "--summary",
        str(plan.summary_path),
    )


def _persist_device_results(plan: SeriesPlan) -> tuple[str, ...]:
    return _nvx(
        "performance",
        "persist",
        "--source-dir",
        str(plan.device_results_dir),
        "--history-dir",
        str(plan.performance_results_dir),
    )


def _multi_collection(plan: SeriesPlan, processors: int) -> tuple[str, ...]:
    return _nvx(
        "performance",
        "collect",
        "--platform",
        plan.platform,
        "--commit",
        plan.commit,
        "--input-dir",
        str(plan.multi_dir(processors)),
        "--output-dir",
        str(plan.multi_results_dir(processors)),
        "--require-shell-snapshot-restore-512",
        "--summary",
        str(plan.summary_path),
    )


def planned_commands(plan: SeriesPlan) -> list[PlannedCommand]:
    """Return every command, including conditional second attempts."""
    commands = [PlannedCommand("verify", _nvx("verify"))]
    for attempt in (1, 2):
        condition = "always" if attempt == 1 else "validation status 75 on attempt 1"
        commands.extend(
            (
                PlannedCommand(
                    "acceptance benchmark",
                    _acceptance_benchmark(plan, attempt),
                    condition,
                    attempt,
                ),
                PlannedCommand(
                    "acceptance validation",
                    _acceptance_validation(plan, attempt),
                    condition,
                    attempt,
                ),
            )
        )
    for attempt in (1, 2):
        condition = "always" if attempt == 1 else "validation status 75 on attempt 1"
        commands.extend(
            (
                PlannedCommand(
                    "performance benchmark",
                    _performance_benchmark(plan, attempt),
                    condition,
                    attempt,
                ),
                PlannedCommand(
                    "performance validation",
                    _performance_validation(plan, attempt),
                    condition,
                    attempt,
                ),
            )
        )
    if plan.settings.with_multi_vcpu:
        for processors in (2, 4, 8):
            for attempt in (1, 2):
                condition = (
                    "always" if attempt == 1 else "validation status 75 on attempt 1"
                )
                commands.extend(
                    (
                        PlannedCommand(
                            f"{processors}-vCPU benchmark",
                            _multi_benchmark(plan, processors, attempt),
                            condition,
                            attempt,
                        ),
                        PlannedCommand(
                            f"{processors}-vCPU validation",
                            _multi_validation(plan, processors, attempt),
                            condition,
                            attempt,
                        ),
                    )
                )
    commands.extend(
        (
            PlannedCommand("device-I/O benchmark", _device_benchmark(plan)),
            PlannedCommand("performance collection", _performance_collection(plan)),
            PlannedCommand("device-I/O collection", _device_collection(plan)),
        )
    )
    if plan.settings.persist:
        commands.append(
            PlannedCommand("device-I/O persistence", _persist_device_results(plan))
        )
    if plan.settings.with_multi_vcpu:
        for processors in (2, 4, 8):
            commands.append(
                PlannedCommand(
                    f"{processors}-vCPU collection",
                    _multi_collection(plan, processors),
                )
            )
    return commands


def _format_command(argv: Sequence[str], system: OperatingSystem) -> str:
    if system == "Windows":
        return subprocess.list2cmdline(list(argv))
    return shlex.join(argv)


def _check_openvmm_processes(
    system: OperatingSystem,
    runner: Runner,
    repository_root: Path,
) -> None:
    if system == "Linux":
        result = runner.run(
            ["pgrep", "-af", "openvmm"],
            cwd=repository_root,
            capture_output=True,
        )
        if result.returncode not in (0, 1):
            raise SeriesError(f"pgrep -af openvmm failed: {_command_detail(result)}")
        processes = result.stdout.strip()
    else:
        result = runner.run(
            list(host_probe.powershell_arguments(host_probe.OPENVMM_PROCESSES)),
            cwd=repository_root,
            capture_output=True,
        )
        if result.returncode != 0:
            raise SeriesError(f"Get-Process openvmm failed: {_command_detail(result)}")
        processes = result.stdout.strip()
    if processes:
        raise SeriesError(
            "OpenVMM is already running; refusing to start a benchmark:\n" + processes
        )


def _copy_file(source: Path, destination: Path) -> None:
    try:
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
    except OSError as error:
        raise SeriesError(
            f"cannot retain validated result {source}: {error}"
        ) from error


def _copy_direct_files(source: Path, destination: Path) -> None:
    try:
        files = [path for path in source.iterdir() if path.is_file()]
        if not files:
            raise SeriesError(f"validated attempt has no result files: {source}")
        destination.mkdir(parents=True, exist_ok=True)
        for path in files:
            shutil.copy2(path, destination / path.name)
    except OSError as error:
        raise SeriesError(
            f"cannot retain validated result {source}: {error}"
        ) from error


def _run_validated_file(
    executor: SeriesExecutor,
    *,
    name: str,
    benchmark: Callable[[int], tuple[str, ...]],
    validation: Callable[[int], tuple[str, ...]],
    attempt_path: Callable[[int], Path],
    accepted_path: Path,
) -> tuple[int, int]:
    for attempt in (1, 2):
        status = executor.run(
            f"{name} benchmark",
            benchmark(attempt),
            attempt=attempt,
        )
        if status != 0:
            return status, attempt
        status = executor.run(
            f"{name} validation",
            validation(attempt),
            attempt=attempt,
        )
        if status == 0:
            _copy_file(attempt_path(attempt), accepted_path)
            return 0, attempt
        if status != 75 or attempt == 2:
            return status, attempt
        print(
            f"warning: {name} was unstable on attempt 1; remeasuring once",
            file=executor.output,
        )
    raise AssertionError("the bounded retry loop did not return")


def _run_validated_directory(
    executor: SeriesExecutor,
    *,
    name: str,
    benchmark: Callable[[int], tuple[str, ...]],
    validation: Callable[[int], tuple[str, ...]],
    attempt_path: Callable[[int], Path],
    accepted_path: Path,
) -> tuple[int, int]:
    for attempt in (1, 2):
        status = executor.run(
            f"{name} benchmark",
            benchmark(attempt),
            attempt=attempt,
        )
        if status != 0:
            return status, attempt
        status = executor.run(
            f"{name} validation",
            validation(attempt),
            attempt=attempt,
        )
        if status == 0:
            _copy_direct_files(attempt_path(attempt), accepted_path)
            return 0, attempt
        if status != 75 or attempt == 2:
            return status, attempt
        print(
            f"warning: {name} was unstable on attempt 1; remeasuring once",
            file=executor.output,
        )
    raise AssertionError("the bounded retry loop did not return")


def _acceptance_failure(error: IO[str]) -> None:
    print(
        "error: acceptance failed; use the nvx-run-microvm skill to debug it; "
        "no performance data may be collected from a failing host",
        file=error,
    )


def _run_required(
    executor: SeriesExecutor,
    step: str,
    argv: Sequence[str],
) -> int:
    return executor.run(step, argv)


def _prepare_directories(plan: SeriesPlan) -> None:
    try:
        plan.output_dir.mkdir(parents=True, exist_ok=False)
    except FileExistsError as error:
        raise SeriesError(
            f"fresh result directory already exists: {plan.output_dir}"
        ) from error
    except OSError as error:
        raise SeriesError(
            f"cannot create result directory {plan.output_dir}: {error}"
        ) from error

    if plan.scratch_dir is None:
        return
    if plan.automatic_scratch:
        root = plan.scratch_dir.parent
        if not root.is_dir():
            raise SeriesError(f"benchmark scratch root was not found: {root}")
        try:
            plan.scratch_dir.mkdir(exist_ok=False)
        except OSError as error:
            raise SeriesError(
                f"cannot create benchmark scratch {plan.scratch_dir}: {error}"
            ) from error
    elif not plan.scratch_dir.is_dir():
        raise SeriesError(
            f"benchmark scratch directory was not found: {plan.scratch_dir}"
        )


def _remove_automatic_scratch(plan: SeriesPlan, error: IO[str]) -> None:
    if not plan.automatic_scratch or plan.scratch_dir is None:
        return
    try:
        shutil.rmtree(plan.scratch_dir)
    except FileNotFoundError:
        return
    except OSError as cleanup_error:
        print(
            f"warning: could not remove benchmark scratch: {cleanup_error}",
            file=error,
        )


def execute_series(
    plan: SeriesPlan,
    runner: Runner,
    *,
    output: IO[str],
    error: IO[str],
) -> ExecutionReport:
    """Execute a resolved plan, retaining only validated attempts for collection."""
    report = ExecutionReport()
    _prepare_directories(plan)
    executor = SeriesExecutor(plan, runner, report, output)
    try:
        status = _run_required(executor, "verify", _nvx("verify"))
        if status != 0:
            report.status = status
            print(f"error: NVX verification failed with status {status}", file=error)
            return report

        status, attempts = _run_validated_file(
            executor,
            name="acceptance",
            benchmark=lambda attempt: _acceptance_benchmark(plan, attempt),
            validation=lambda attempt: _acceptance_validation(plan, attempt),
            attempt_path=lambda attempt: _acceptance_attempt(plan, attempt),
            accepted_path=plan.acceptance_output,
        )
        report.attempts["acceptance"] = attempts
        if status != 0:
            report.status = status
            _acceptance_failure(error)
            return report

        status, attempts = _run_validated_directory(
            executor,
            name="performance",
            benchmark=lambda attempt: _performance_benchmark(plan, attempt),
            validation=lambda attempt: _performance_validation(plan, attempt),
            attempt_path=lambda attempt: _performance_attempt(plan, attempt),
            accepted_path=plan.performance_dir,
        )
        report.attempts["performance"] = attempts
        if status != 0:
            report.status = status
            print(
                f"error: performance validation failed with status {status}",
                file=error,
            )
            return report

        if plan.settings.with_multi_vcpu:
            for processors in (2, 4, 8):
                status, attempts = _run_validated_directory(
                    executor,
                    name=f"{processors}-vCPU shell-snapshot restore",
                    benchmark=lambda attempt, count=processors: _multi_benchmark(
                        plan, count, attempt
                    ),
                    validation=lambda attempt, count=processors: _multi_validation(
                        plan, count, attempt
                    ),
                    attempt_path=lambda attempt, count=processors: _multi_attempt(
                        plan, count, attempt
                    ),
                    accepted_path=plan.multi_dir(processors),
                )
                report.attempts[f"{processors}vcpu"] = attempts
                if status != 0:
                    report.status = status
                    print(
                        "error: "
                        f"{processors}-vCPU validation failed with status {status}",
                        file=error,
                    )
                    return report

        for step, argv in (
            ("device-I/O benchmark", _device_benchmark(plan)),
            ("performance collection", _performance_collection(plan)),
            ("device-I/O collection", _device_collection(plan)),
        ):
            status = _run_required(executor, step, argv)
            if status != 0:
                report.status = status
                print(f"error: {step} failed with status {status}", file=error)
                return report

        if plan.settings.persist:
            status = _run_required(
                executor,
                "device-I/O persistence",
                _persist_device_results(plan),
            )
            if status != 0:
                report.status = status
                print(
                    f"error: device-I/O persistence failed with status {status}",
                    file=error,
                )
                return report

        if plan.settings.with_multi_vcpu:
            for processors in (2, 4, 8):
                step = f"{processors}-vCPU collection"
                status = _run_required(
                    executor,
                    step,
                    _multi_collection(plan, processors),
                )
                if status != 0:
                    report.status = status
                    print(f"error: {step} failed with status {status}", file=error)
                    return report

        report.checks = verify_result_set(plan)
        for check in report.checks:
            label = "PASS" if check.passed else "FAIL"
            print(f"{label}: {check.name}: {check.detail}", file=output)
        if not all(check.passed for check in report.checks):
            report.status = 1
            print("error: retained benchmark result verification failed", file=error)
        return report
    finally:
        _remove_automatic_scratch(plan, error)


def _read_json(path: Path) -> tuple[dict[str, object] | None, str]:
    try:
        loaded = cast(object, json.loads(path.read_text(encoding="utf-8")))
    except FileNotFoundError:
        return None, "missing"
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        return None, f"invalid JSON: {error}"
    if not isinstance(loaded, dict):
        return None, "JSON root is not an object"
    return cast(dict[str, object], loaded), "valid JSON object"


def _nested(document: dict[str, object], *keys: str) -> object:
    value: object = document
    for key in keys:
        if not isinstance(value, dict):
            return None
        value = _mapping(cast(object, value), "nested JSON value").get(key)
    return value


def _has_host_provenance(document: dict[str, object]) -> bool:
    host = document.get("host")
    if not isinstance(host, dict) or not host:
        return False
    volumes = _mapping(cast(object, host), "host").get("volumes")
    if not isinstance(volumes, dict):
        return False
    scratch = _mapping(cast(object, volumes), "host.volumes").get("scratch")
    return isinstance(scratch, dict) and bool(
        _mapping(cast(object, scratch), "scratch volume")
    )


def _expected_scratch(plan: SeriesPlan) -> str | None:
    if plan.scratch_dir is None:
        return None
    candidate = plan.scratch_dir
    if not candidate.is_absolute():
        candidate = plan.repository_root / candidate
    return str(candidate.resolve())


def _check_file(checks: list[ResultCheck], name: str, path: Path) -> None:
    try:
        present = path.is_file() and path.stat().st_size > 0
    except OSError as error:
        checks.append(ResultCheck(name, False, str(error)))
        return
    checks.append(
        ResultCheck(
            name, present, str(path) if present else f"missing or empty: {path}"
        )
    )


def _metadata_expectations(
    plan: SeriesPlan,
    *,
    suite: str,
    processors: int,
    warmups: int,
    runs: int,
    virtfs_runs: int,
    shell_memories: list[int] | None = None,
    device_duration: int | None = None,
    device_size: int | None = None,
) -> dict[str, object]:
    result: dict[str, object] = {
        "suite": suite,
        "platform": plan.platform,
        "backend": plan.backend,
        "processors": processors,
        "warmups": warmups,
        "measured_runs": runs,
        "virtfs_measured_runs": virtfs_runs,
    }
    if shell_memories is not None:
        result["shell_memories_mib"] = shell_memories
    if device_duration is not None:
        result["device_io_duration_seconds"] = device_duration
    if device_size is not None:
        result["device_io_size_mib"] = device_size
    return result


def _verify_metadata(
    checks: list[ResultCheck],
    name: str,
    path: Path,
    plan: SeriesPlan,
    expected: dict[str, object],
    *,
    require_hashes: bool = False,
) -> dict[str, object] | None:
    document, detail = _read_json(path)
    checks.append(ResultCheck(f"{name} metadata JSON", document is not None, detail))
    if document is None:
        return None
    actual = {field: document.get(field) for field in expected}
    checks.append(
        ResultCheck(
            f"{name} metadata controls",
            actual == expected,
            f"expected {expected!r}; found {actual!r}",
        )
    )
    revision = _nested(document, "artifact_revisions", "nvx")
    checks.append(
        ResultCheck(
            f"{name} metadata commit",
            revision == plan.commit,
            f"expected {plan.commit}; found {revision!r}",
        )
    )
    expected_scratch = _expected_scratch(plan)
    scratch = document.get("scratch_directory")
    scratch_ok = isinstance(scratch, str) and bool(scratch)
    if expected_scratch is not None:
        scratch_ok = scratch_ok and scratch == expected_scratch
    checks.append(
        ResultCheck(
            f"{name} metadata scratch",
            scratch_ok,
            f"expected {expected_scratch or 'a recorded directory'}; found {scratch!r}",
        )
    )
    checks.append(
        ResultCheck(
            f"{name} metadata host provenance",
            _has_host_provenance(document),
            "host.volumes.scratch is recorded",
        )
    )
    if require_hashes:
        hashes = document.get("artifact_sha256")
        required = ("openvmm", "kernel", "initrd", "benchmark_coordinator")
        valid = isinstance(hashes, dict) and all(
            isinstance(cast(dict[str, object], hashes).get(field), str)
            and SHA256.fullmatch(cast(str, cast(dict[str, object], hashes)[field]))
            is not None
            for field in required
        )
        checks.append(
            ResultCheck(
                f"{name} artifact hashes",
                valid,
                "device-I/O metadata records all required SHA-256 values",
            )
        )
    return document


def _result_filename(metadata: dict[str, object], platform: str) -> str | None:
    abi = metadata.get("microvm_abi_version")
    processors = metadata.get("processors")
    if (
        isinstance(abi, bool)
        or not isinstance(abi, int)
        or isinstance(processors, bool)
        or not isinstance(processors, int)
    ):
        return None
    if abi == 1 and processors == 1:
        return f"{platform}.csv"
    return f"{platform}-microvm-v{abi}-{processors}vcpu.csv"


def _verify_csv(
    checks: list[ResultCheck],
    name: str,
    path: Path,
    plan: SeriesPlan,
    processors: int,
    expected_rows: int,
    expected_metric: str | None = None,
) -> None:
    try:
        with path.open(encoding="utf-8", newline="") as source:
            rows = list(csv.DictReader(source))
    except (OSError, UnicodeError, csv.Error) as error:
        checks.append(ResultCheck(name, False, f"cannot read {path}: {error}"))
        return
    identities_match = all(
        row.get("platform") == plan.platform
        and row.get("commit") == plan.commit
        and row.get("processors") == str(processors)
        for row in rows
    )
    metric_matches = expected_metric is None or (
        len(rows) == 1 and rows[0].get("metric") == expected_metric
    )
    valid = len(rows) == expected_rows and identities_match and metric_matches
    metric_detail = (
        "" if expected_metric is None else f" and metric {expected_metric!r}"
    )
    checks.append(
        ResultCheck(
            name,
            valid,
            f"{len(rows)} row(s), expected {expected_rows}{metric_detail}, with "
            "expected platform, commit, and processors",
        )
    )


def _verify_collected_result(
    checks: list[ResultCheck],
    name: str,
    results_dir: Path,
    metadata: dict[str, object] | None,
    plan: SeriesPlan,
    processors: int,
    expected_rows: int,
    expected_metric: str | None = None,
) -> None:
    filename = (
        _result_filename(metadata, plan.platform) if metadata is not None else None
    )
    if filename is None:
        checks.append(
            ResultCheck(f"{name} collected CSV", False, "metadata lacks dimensions")
        )
    else:
        _verify_csv(
            checks,
            f"{name} collected CSV",
            results_dir / filename,
            plan,
            processors,
            expected_rows,
            expected_metric,
        )
    host_path = results_dir / f"{plan.platform}-host.json"
    host, detail = _read_json(host_path)
    checks.append(
        ResultCheck(
            f"{name} collected host",
            host is not None,
            f"{host_path}: {detail}",
        )
    )


def verify_result_set(plan: SeriesPlan) -> list[ResultCheck]:
    """Check every retained raw result, metadata record, and collected CSV."""
    checks: list[ResultCheck] = []
    acceptance, detail = _read_json(plan.acceptance_output)
    checks.append(ResultCheck("acceptance JSON", acceptance is not None, detail))
    if acceptance is not None:
        expected_controls = {
            "suite": "e2e",
            "platform": plan.platform,
            "backend": plan.backend,
            "processors": 1,
            "warmups": plan.settings.acceptance_warmups,
            "runs": plan.settings.acceptance_runs,
            "teardown_mode": plan.settings.teardown_mode,
            "snapshot_profile": True,
        }
        controls = acceptance.get("controls")
        actual = (
            {
                field: cast(dict[str, object], controls).get(field)
                for field in expected_controls
            }
            if isinstance(controls, dict)
            else None
        )
        checks.append(
            ResultCheck(
                "acceptance controls",
                actual == expected_controls,
                f"expected {expected_controls!r}; found {actual!r}",
            )
        )
        revision = _nested(acceptance, "controls", "artifact_revisions", "nvx")
        checks.append(
            ResultCheck(
                "acceptance commit",
                revision == plan.commit,
                f"expected {plan.commit}; found {revision!r}",
            )
        )
        scratch = _nested(acceptance, "controls", "scratch_directory")
        expected_scratch = _expected_scratch(plan)
        scratch_ok = isinstance(scratch, str) and bool(scratch)
        if expected_scratch is not None:
            scratch_ok = scratch_ok and scratch == expected_scratch
        checks.append(
            ResultCheck(
                "acceptance scratch",
                scratch_ok,
                f"expected {expected_scratch or 'a recorded directory'}; "
                f"found {scratch!r}",
            )
        )
        checks.append(
            ResultCheck(
                "acceptance host provenance",
                _has_host_provenance(acceptance),
                "host.volumes.scratch is recorded",
            )
        )
    _check_file(checks, "acceptance attempt log", plan.acceptance_attempt_log)
    for filename in PERFORMANCE_LOGS:
        _check_file(
            checks,
            f"performance raw log {filename}",
            plan.performance_dir / filename,
        )
    _check_file(checks, "device-I/O raw log", plan.device_dir / "device-io.log")
    _check_file(checks, "summary", plan.summary_path)

    performance_metadata = _verify_metadata(
        checks,
        "performance",
        plan.performance_dir / "benchmark-metadata.json",
        plan,
        _metadata_expectations(
            plan,
            suite="performance",
            processors=1,
            warmups=plan.settings.performance_warmups,
            runs=plan.settings.performance_runs,
            virtfs_runs=plan.settings.virtfs_runs,
        ),
    )
    device_metadata = _verify_metadata(
        checks,
        "device-I/O",
        plan.device_dir / "benchmark-metadata.json",
        plan,
        _metadata_expectations(
            plan,
            suite="device-io",
            processors=1,
            warmups=plan.settings.device_warmups,
            runs=plan.settings.device_runs,
            virtfs_runs=3,
            device_duration=plan.settings.device_duration_seconds,
            device_size=plan.settings.device_size_mib or 512,
        ),
        require_hashes=True,
    )
    _verify_collected_result(
        checks,
        "performance",
        plan.performance_results_dir,
        performance_metadata,
        plan,
        1,
        29 if plan.profile == "smoke" else 34,
    )
    _verify_collected_result(
        checks,
        "device-I/O",
        plan.device_results_dir,
        device_metadata,
        plan,
        1,
        5,
    )

    if plan.settings.with_multi_vcpu:
        for processors in (2, 4, 8):
            directory = plan.multi_dir(processors)
            _check_file(
                checks,
                f"{processors}-vCPU raw restore log",
                directory / "shell-snapshot-restore.log",
            )
            metadata = _verify_metadata(
                checks,
                f"{processors}-vCPU",
                directory / "benchmark-metadata.json",
                plan,
                _metadata_expectations(
                    plan,
                    suite="shell-snapshot-restore",
                    processors=processors,
                    warmups=plan.settings.multi_warmups,
                    runs=plan.settings.multi_runs,
                    virtfs_runs=3,
                    shell_memories=[512],
                ),
            )
            _verify_collected_result(
                checks,
                f"{processors}-vCPU",
                plan.multi_results_dir(processors),
                metadata,
                plan,
                processors,
                1,
                "shell_snapshot_restore_512_mib",
            )
    return checks


def _plan_json(
    plan: SeriesPlan,
    commands: Sequence[PlannedCommand],
) -> dict[str, object]:
    return {
        "profile": plan.profile,
        "platform": plan.platform,
        "backend": plan.backend,
        "host_type": plan.host_type,
        "commit": plan.commit,
        "dirty": bool(plan.dirty_paths),
        "dirty_paths": list(plan.dirty_paths),
        "output_dir": str(plan.output_dir),
        "dry_run": True,
        "commands": [
            {
                "step": command.step,
                "argv": list(command.argv),
                "status": None,
                "attempt": command.attempt,
                "condition": command.condition,
            }
            for command in commands
        ],
        "attempts": {},
        "result_checks": [],
    }


def _report_json(plan: SeriesPlan, report: ExecutionReport) -> dict[str, object]:
    return {
        "profile": plan.profile,
        "platform": plan.platform,
        "backend": plan.backend,
        "host_type": plan.host_type,
        "commit": plan.commit,
        "dirty": bool(plan.dirty_paths),
        "dirty_paths": list(plan.dirty_paths),
        "output_dir": str(plan.output_dir),
        "dry_run": False,
        "status": report.status,
        "commands": [
            {
                "step": command.step,
                "argv": list(command.argv),
                "status": command.status,
                "attempt": command.attempt,
            }
            for command in report.commands
        ],
        "attempts": report.attempts,
        "result_checks": [
            {
                "name": check.name,
                "passed": check.passed,
                "detail": check.detail,
            }
            for check in report.checks
        ],
    }


def _print_identity(plan: SeriesPlan, output: IO[str]) -> None:
    print(f"Profile: {plan.profile}", file=output)
    print(f"Platform: {plan.platform}", file=output)
    print(f"Commit: {plan.commit}", file=output)
    if plan.dirty_paths:
        print("Dirty state:", file=output)
        for path in plan.dirty_paths:
            print(f"  {path}", file=output)
    else:
        print("Dirty state: clean", file=output)
    print(f"Output: {plan.output_dir}", file=output)
    if plan.scratch_dir is not None:
        print(f"Scratch: {plan.scratch_dir}", file=output)


def _print_dry_run(plan: SeriesPlan, output: IO[str]) -> list[PlannedCommand]:
    commands = planned_commands(plan)
    _print_identity(plan, output)
    print("Dry-run plan:", file=output)
    for index, command in enumerate(commands, start=1):
        suffix = "" if command.condition == "always" else f" [{command.condition}]"
        print(
            f"{index:02d}. {command.step}{suffix}: "
            f"{_format_command(command.argv, plan.system)}",
            file=output,
        )
    return commands


def run_cli(
    argv: Sequence[str] | None = None,
    *,
    runner: Runner | None = None,
    repository_root: Path = REPOSITORY_ROOT,
    action_path: Path = ACTION_PATH,
    system_name: str | None = None,
    now: dt.datetime | None = None,
    environment: Mapping[str, str] | None = None,
    output: IO[str] | None = None,
    error: IO[str] | None = None,
) -> int:
    """Run the CLI with injectable host effects for hermetic tests."""
    selected_runner = runner or SubprocessRunner()
    selected_output = output or sys.stdout
    selected_error = error or sys.stderr
    options = _options(argv)
    if options.repo is not None:
        # A checkout that predates this tool runs its own scripts/nvx.py and
        # CI sampling under the coordinator of a newer one.
        repository_root = options.repo.resolve()
        action_path = (
            repository_root / ".github" / "actions" / "run-benchmark" / "action.yml"
        )
    try:
        plan = _make_plan(
            options,
            runner=selected_runner,
            repository_root=repository_root,
            action_path=action_path,
            system_name=system_name or host_platform.system(),
            now=now or dt.datetime.now(dt.timezone.utc),
            environment=environment or os.environ,
        )
        if options.dry_run:
            commands = _print_dry_run(plan, selected_output)
            if options.json_output:
                print(
                    json.dumps(_plan_json(plan, commands), indent=2),
                    file=selected_output,
                )
            print(f"NVX_RESULTS={plan.output_dir}", file=selected_output)
            return 0

        _check_openvmm_processes(
            plan.system,
            selected_runner,
            repository_root,
        )
        _print_identity(plan, selected_output)
        report = execute_series(
            plan,
            selected_runner,
            output=selected_output,
            error=selected_error,
        )
        if options.json_output:
            print(
                json.dumps(_report_json(plan, report), indent=2), file=selected_output
            )
        print(f"NVX_RESULTS={plan.output_dir}", file=selected_output)
        return report.status
    except SeriesError as series_error:
        print(f"error: {series_error}", file=selected_error)
        return 1


def main(argv: Sequence[str] | None = None) -> int:
    """Run a benchmark series from command-line arguments."""
    return run_cli(argv)


if __name__ == "__main__":
    raise SystemExit(main())
