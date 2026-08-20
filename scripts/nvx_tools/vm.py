"""Shared VM launch and smoke-test workflows."""

from __future__ import annotations

import contextlib
import json
import re
import shutil
import statistics
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Iterator, Mapping, Sequence

from .backends.base import HostBackend
from .common import (
    REPO_ROOT,
    CommandResult,
    ScriptError,
    diagnostic_tail,
    remove_tree,
    require_file,
    require_success,
    run_capture,
)


DEFAULT_CMDLINE = "earlycon=xe9 console=hvc0 reboot=t panic=-1"
BOOT_MARKER = "ALPINE-MICROVM-BOOT-OK"
# The workload is read from the mount after the snapshot resumes, so it is not part of the snapshot.
PYTHON_SNAPSHOT_CMDLINE = (
    "earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1 "
    "nvx_mode=hostfs nvx_snapshot=1 pyapp=app.py"
)
PYTHON_SNAPSHOT_PROFILE_WORKLOADS = (
    ("hello.py", "hello world"),
    ("pandas.py", "{'x': 10, 'y': 30}"),
)
PYTHON_SNAPSHOT_PROFILE_RUNS = 1
Runner = Callable[..., CommandResult]
METRIC_PATTERN = re.compile(r"(?:cold-start|restore):\s*([0-9.]+)")


@dataclass(frozen=True)
class BootTestConfig:
    kernel: Path
    initrd: Path
    mem: int = 512
    timeout: int = 90
    vcpus: int = 1


@dataclass(frozen=True)
class BootTestReport:
    passed: bool
    alpine_release: str | None
    uname: str | None
    result: CommandResult


@dataclass(frozen=True)
class VmConfig:
    kernel: Path
    initrd: Path
    mem: int = 512
    vcpus: int = 1
    cmdline: str = DEFAULT_CMDLINE


@dataclass(frozen=True)
class SnapshotConfig:
    kernel: Path
    initrd: Path
    snapshot: Path
    mem: int = 512
    runs: int = 8


@dataclass(frozen=True)
class HotSnapshotConfig:
    snapshot: Path
    marker: str
    mem: int = 512
    runs: int = 5
    training_runs: int = 5


def boot_test(
    config: BootTestConfig,
    backend: HostBackend,
    *,
    runner: Runner = run_capture,
) -> BootTestReport:
    executable = backend.executable()
    require_file(executable, "build the VMM first: cargo build --release")
    require_file(
        config.kernel,
        f"missing kernel: {config.kernel} ({backend.build_hint})",
    )
    require_file(
        config.initrd,
        f"missing initrd: {config.initrd} ({backend.build_hint})",
    )

    args = cold_boot_args(
        executable,
        VmConfig(
            config.kernel,
            config.initrd,
            config.mem,
            config.vcpus,
            DEFAULT_CMDLINE,
        ),
        backend,
    )
    result = runner(
        args,
        input_text="cat /etc/alpine-release\nuname -a\nreboot -f\n",
        timeout=config.timeout,
    )
    text = result.text
    alpine_match = re.search(r"(?m)^3\.[0-9]+\.[0-9]+[^\r\n]*", text)
    uname_match = re.search(r"(?m)^Linux .* x86_64[^\r\n]*", text)
    return BootTestReport(
        passed=BOOT_MARKER in text,
        alpine_release=alpine_match.group(0).strip() if alpine_match else None,
        uname=uname_match.group(0).strip() if uname_match else None,
        result=result,
    )


def print_boot_test(report: BootTestReport) -> int:
    if report.passed:
        print(f"PASS: guest reached userspace ({BOOT_MARKER})")
        if report.alpine_release:
            print(f"  alpine-release: {report.alpine_release}")
        if report.uname:
            print(f"  uname: {report.uname}")
        return 0

    print(f"FAIL: '{BOOT_MARKER}' not found in boot output")
    if report.result.timed_out:
        print("  process timed out")
    print("--- last 25 lines ---")
    print("\n".join(report.result.text.splitlines()[-25:]))
    return 1


def require_vm_inputs(config: VmConfig, backend: HostBackend) -> Path:
    executable = backend.executable()
    require_file(executable, "build the VMM first: cargo build --release")
    require_file(
        config.kernel, f"missing kernel: {config.kernel} ({backend.build_hint})"
    )
    require_file(
        config.initrd, f"missing initrd: {config.initrd} ({backend.build_hint})"
    )
    return executable


def cold_boot_args(
    executable: Path,
    config: VmConfig,
    backend: HostBackend,
    extra: Iterable[str | Path] = (),
) -> list[str | Path]:
    args: list[str | Path] = [executable]
    backend.add_vcpus(args, config.vcpus)
    args.extend(
        [
            "--kernel",
            config.kernel,
            "--initrd",
            config.initrd,
            "--mem",
            str(config.mem),
            "--cmdline",
            config.cmdline,
            *extra,
        ]
    )
    return args


def invoke_vm_metric(
    args: Sequence[str | Path],
    *,
    timeout: int = 30,
    runner: Runner = run_capture,
) -> float | None:
    metric, _, _ = invoke_vm_measurement(args, timeout=timeout, runner=runner)
    return metric


def invoke_vm_measurement(
    args: Sequence[str | Path],
    *,
    timeout: int = 30,
    runner: Runner = run_capture,
) -> tuple[float | None, float, CommandResult]:
    started = time.perf_counter()
    result = runner(args, timeout=timeout)
    wall_ms = (time.perf_counter() - started) * 1000.0
    require_success(result, "VM timing run")
    match = METRIC_PATTERN.search(result.text)
    return (float(match.group(1)) if match else None, wall_ms, result)


def collect_metrics(
    args: Sequence[str | Path],
    runs: int,
    *,
    timeout: int = 30,
    runner: Runner = run_capture,
) -> list[float]:
    if runs < 1:
        raise ScriptError("number of runs must be at least 1")
    values = [
        invoke_vm_metric(args, timeout=timeout, runner=runner) for _ in range(runs)
    ]
    return [value for value in values if value is not None]


def require_hot_page_diagnostic(
    result: CommandResult,
    label: str,
    expected: Mapping[str, str],
) -> dict[str, str]:
    prefix = "snapshot-hot-pages:"
    lines = [
        line.partition(prefix)[2].strip()
        for line in result.text.splitlines()
        if prefix in line
    ]
    if len(lines) != 1:
        raise ScriptError(
            f"{label} produced {len(lines)} hot-page diagnostics; expected exactly 1"
        )
    fields: dict[str, str] = {}
    for token in lines[0].split():
        if "=" not in token:
            continue
        key, value = token.split("=", 1)
        if key in fields:
            raise ScriptError(f"{label} repeated diagnostic field {key!r}")
        fields[key] = value
    for key, value in expected.items():
        actual = fields.get(key)
        if actual != value:
            raise ScriptError(
                f"{label} reported {key}={actual or '<missing>'}; expected {value}"
            )
    return fields


def require_positive_diagnostic_count(
    fields: Mapping[str, str], key: str, label: str
) -> None:
    value = fields.get(key)
    try:
        count = int(value) if value is not None else 0
    except ValueError as error:
        raise ScriptError(f"{label} reported invalid {key}={value}") from error
    if count < 1:
        raise ScriptError(
            f"{label} reported {key}={value or '<missing>'}; expected > 0"
        )


def inspect_hot_page_profile(
    executable: Path,
    snapshot: Path,
    *,
    timeout: int = 30,
    runner: Runner = run_capture,
) -> dict[str, object]:
    result = runner(
        [
            executable,
            "--restore",
            snapshot,
            "--snapshot-profile-inspect",
            "--quiet",
        ],
        timeout=timeout,
    )
    require_success(result, "hot-page profile inspection")
    lines = [line.strip() for line in result.text.splitlines() if line.strip()]
    if len(lines) != 1:
        raise ScriptError(
            f"hot-page profile inspection produced {len(lines)} nonempty lines; expected 1"
        )
    try:
        inspection = json.loads(lines[0])
    except json.JSONDecodeError as error:
        raise ScriptError(
            "hot-page profile inspection produced invalid JSON"
        ) from error
    if not isinstance(inspection, dict):
        raise ScriptError("hot-page profile inspection did not produce a JSON object")
    return inspection


def require_profile_inspection(
    inspection: Mapping[str, object],
    label: str,
    *,
    training_runs: int,
    ram_size: int,
    snapshot_identity: str | None = None,
) -> None:
    expected: dict[str, object] = {
        "validation": "ok",
        "format_version": 1,
        "algorithm_version": 1,
        "training_runs": training_runs,
        "ram_size": ram_size,
    }
    if snapshot_identity is not None:
        expected["snapshot_identity"] = snapshot_identity
    for key, value in expected.items():
        if inspection.get(key) != value:
            raise ScriptError(
                f"{label} inspection reported {key}={inspection.get(key)!r}; expected {value!r}"
            )
    identity = inspection.get("snapshot_identity")
    if not isinstance(identity, str) or len(identity) != 64:
        raise ScriptError(f"{label} inspection reported an invalid snapshot identity")
    for key in ("selected_pages", "selected_bytes", "ranges"):
        value = inspection.get(key)
        if type(value) is not int or value < 1:
            raise ScriptError(f"{label} inspection reported invalid {key}={value!r}")
    selected_pages = inspection["selected_pages"]
    selected_bytes = inspection["selected_bytes"]
    ranges = inspection["ranges"]
    if selected_pages * 4096 != selected_bytes:
        raise ScriptError(f"{label} inspection page and byte counts are inconsistent")
    if selected_bytes > min(128 << 20, ram_size // 4):
        raise ScriptError(f"{label} inspection exceeds the selected-byte cap")
    if ranges > 4096:
        raise ScriptError(f"{label} inspection exceeds the range-count cap")
    regions = inspection.get("regions")
    if not isinstance(regions, list) or len(regions) not in (1, 2):
        raise ScriptError(f"{label} inspection reported an invalid RAM layout")
    if (
        sum(
            region.get("size", 0) if isinstance(region, dict) else 0
            for region in regions
        )
        != ram_size
    ):
        raise ScriptError(f"{label} inspection RAM layout does not cover the snapshot")


def benchmark_snapshot_prefetch(
    config: HotSnapshotConfig,
    backend: HostBackend,
    *,
    runner: Runner = run_capture,
) -> None:
    if backend.name != "windows-whp":
        raise ScriptError(
            "hot snapshot-page profiling is available only on Windows/WHP"
        )
    if config.runs < 1 or config.training_runs < 1:
        raise ScriptError("runs and training runs must be positive")
    if not config.marker:
        raise ScriptError("the profile endpoint marker must not be empty")

    executable = backend.executable()
    require_file(executable, "build the release VMM first: cargo build --release")
    require_file(config.snapshot / "state.bin", "snapshot is missing state.bin")
    require_file(config.snapshot / "mem.bin", "snapshot is missing mem.bin")
    profile = config.snapshot / "hot-pages.whp.v1"
    profile.unlink(missing_ok=True)

    common: list[str | Path] = [
        executable,
        "--restore",
        config.snapshot,
        "--mem",
        str(config.mem),
        "--console",
        "auto",
        "--exit-on-boot",
        "--quiet",
        "--boot-marker",
        config.marker,
    ]
    print(
        f"training {profile} through {config.marker!r} "
        f"({config.training_runs} controlled restores, prefetch disabled)"
    )
    for run in range(config.training_runs):
        result = runner(
            [
                *common,
                "--snapshot-prefetch",
                "off",
                "--snapshot-profile-generate",
                "--log-level",
                "info",
            ],
            timeout=60,
        )
        require_success(result, f"hot-page training run {run + 1}")
        require_file(profile, "hot-page trainer did not publish its sidecar")
        label = f"hot-page training run {run + 1}"
        fields = require_hot_page_diagnostic(
            result,
            label,
            {"status": "trained", "training_runs": str(run + 1)},
        )
        for key in ("observed_pages", "selected_pages", "ranges"):
            require_positive_diagnostic_count(fields, key, label)
        inspection = inspect_hot_page_profile(
            executable, config.snapshot, timeout=60, runner=runner
        )
        require_profile_inspection(
            inspection,
            label,
            training_runs=run + 1,
            ram_size=config.mem << 20,
        )
        identity = inspection["snapshot_identity"]
        if fields.get("identity") != identity[:16]:
            raise ScriptError(f"{label} diagnostic identity does not match its sidecar")

    off: list[float] = []
    auto: list[float] = []
    off_wall: list[float] = []
    auto_wall: list[float] = []
    for _ in range(config.runs):
        baseline, baseline_wall, baseline_result = invoke_vm_measurement(
            [*common, "--snapshot-prefetch", "off"], timeout=60, runner=runner
        )
        if "snapshot-hot-pages:" in baseline_result.text:
            raise ScriptError(
                "snapshot prefetch off run unexpectedly consumed the sidecar"
            )
        optimized_started = time.perf_counter()
        optimized_result = runner([*common, "--snapshot-prefetch", "auto"], timeout=60)
        optimized_wall = (time.perf_counter() - optimized_started) * 1000.0
        require_success(optimized_result, "snapshot prefetch auto run")
        fields = require_hot_page_diagnostic(
            optimized_result,
            "snapshot prefetch auto run",
            {
                "status": "active",
                "population": "background",
                "prefetch": "skipped",
                "prefetch_reason": "none",
                "populate": "ok",
                "populate_reason": "none",
            },
        )
        for key in ("selected_pages", "ranges"):
            require_positive_diagnostic_count(fields, key, "snapshot prefetch auto run")
        metric_match = METRIC_PATTERN.search(optimized_result.text)
        optimized = float(metric_match.group(1)) if metric_match else None
        if baseline is not None:
            off.append(baseline)
            off_wall.append(baseline_wall)
        if optimized is not None:
            auto.append(optimized)
            auto_wall.append(optimized_wall)
    require_samples(off, "snapshot prefetch off", config.runs)
    require_samples(auto, "snapshot prefetch auto", config.runs)
    print(f"  off : {format_median(off)}")
    print(f"  auto: {format_median(auto)}")
    baseline_median = statistics.median(off)
    optimized_median = statistics.median(auto)
    improvement = (
        100.0 * (baseline_median - optimized_median) / baseline_median
        if baseline_median
        else 0.0
    )
    print(f"  first-entry-to-marker improvement: {improvement:.1f}%")
    print(f"  off process-start-to-marker : {format_median(off_wall)}")
    print(f"  auto process-start-to-marker: {format_median(auto_wall)}")
    wall_improvement = (
        100.0
        * (statistics.median(off_wall) - statistics.median(auto_wall))
        / statistics.median(off_wall)
        if statistics.median(off_wall)
        else 0.0
    )
    print(f"  process-start-to-marker improvement: {wall_improvement:.1f}%")


def require_samples(values: Sequence[float], label: str, expected: int) -> None:
    if len(values) != expected:
        raise ScriptError(f"{label} produced {len(values)}/{expected} timing samples")


def format_median(values: Sequence[float], width: int = 7) -> str:
    if not values:
        return "NO DATA"
    return (
        f"{statistics.median(values):{width}.1f} ms  "
        f"(min {min(values):.1f}, max {max(values):.1f}, n={len(values)})"
    )


def run_vm(
    config: VmConfig,
    backend: HostBackend,
    *,
    profile: str = "release",
    quiet: bool = False,
    exit_on_boot: bool = False,
    mount: Path | None = None,
    mount_target: str = "/mnt/host",
    mount_rw: bool = False,
    net: str | None = None,
    allow_hosts: Sequence[str] = (),
    block_hosts: Sequence[str] = (),
    allow_endpoints: Sequence[str] = (),
) -> int:
    executable = backend.executable(profile)
    require_file(executable, f"microvm not found at {executable}; build it first")
    require_file(
        config.kernel, f"missing kernel: {config.kernel} ({backend.build_hint})"
    )
    require_file(
        config.initrd, f"missing initrd: {config.initrd} ({backend.build_hint})"
    )
    args = cold_boot_args(executable, config, backend)
    if quiet:
        args.append("--quiet")
    if exit_on_boot:
        args.append("--exit-on-boot")
    if mount is not None:
        args.extend(["--mount", mount, "--mount-target", mount_target])
        if mount_rw:
            args.append("--mount-rw")
    if net:
        args.extend(["--net", net])
    for destination in allow_hosts:
        args.extend(["--allow-host", destination])
    for destination in block_hosts:
        args.extend(["--block-host", destination])
    for endpoint in allow_endpoints:
        args.extend(["--allow-endpoint", endpoint])
    return subprocess.call([str(value) for value in args])


def measure_coldstart(config: VmConfig, backend: HostBackend, runs: int) -> None:
    executable = require_vm_inputs(config, backend)
    quiet = "earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1"
    clocksource = f"clocksource={backend.clocksource}"
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

    def measure(cmdline: str, *, mem: int) -> str:
        run_config = VmConfig(config.kernel, config.initrd, mem, config.vcpus, cmdline)
        args = cold_boot_args(
            executable,
            run_config,
            backend,
            ["--exit-on-boot", "--boot-marker", BOOT_MARKER, "--quiet"],
        )
        samples = collect_metrics(args, runs)
        require_samples(samples, BOOT_MARKER, runs)
        return format_median(samples, width=8)

    print(
        f"cold-start (guest start -> marker), median of {runs} runs, "
        f"{config.mem} MiB, {config.vcpus} vCPU"
    )
    print()
    print("end-to-end (to interactive shell, one tuning per scenario):")
    for label, parameter in scenarios:
        cmdline = quiet if parameter is None else f"{quiet} {parameter}"
        print(f"  {label:<25}: {measure(cmdline, mem=config.mem)}")


def capture_snapshot(
    args: Sequence[str | Path],
    snapshot: Path,
    *,
    timeout: int = 40,
    runner: Runner = run_capture,
) -> CommandResult:
    remove_tree(snapshot, label="snapshot")
    result = runner(args, timeout=timeout)
    state = snapshot / "state.bin"
    memory = snapshot / "mem.bin"
    if (
        result.timed_out
        or result.returncode not in (0, None)
        or not state.is_file()
        or not memory.is_file()
    ):
        diagnostic = diagnostic_tail(result.text)
        reason = "timed out" if result.timed_out else f"exited {result.returncode}"
        raise ScriptError(f"snapshot capture {reason}\n{diagnostic}".rstrip())
    return result


@contextlib.contextmanager
def published_workload(app: str) -> Iterator[Path]:
    """Exports a guest workload through a host directory the VM mounts after it resumes."""
    source = REPO_ROOT / "alpine" / app
    require_file(source, f"missing Python workload: {source}")
    with tempfile.TemporaryDirectory(prefix="nvx-workload-") as directory:
        target = Path(directory)
        shutil.copyfile(source, target / "app.py")
        yield target


def train_python_snapshot_profile(
    executable: Path,
    snapshot: Path,
    mem: int,
    *,
    runner: Runner = run_capture,
) -> None:
    profile = snapshot / "hot-pages.whp.v1"
    profile.unlink(missing_ok=True)
    for app, marker in PYTHON_SNAPSHOT_PROFILE_WORKLOADS:
        for run in range(PYTHON_SNAPSHOT_PROFILE_RUNS):
            with published_workload(app) as workload:
                result = runner(
                    [
                        executable,
                        "--restore",
                        snapshot,
                        "--mem",
                        str(mem),
                        "--mount",
                        workload,
                        "--exit-on-boot",
                        "--quiet",
                        "--boot-marker",
                        marker,
                        "--snapshot-prefetch",
                        "off",
                        "--snapshot-profile-generate",
                        "--log-level",
                        "warn",
                    ],
                    timeout=60,
                )
                require_success(
                    result,
                    f"training Python snapshot profile with {app} (run {run + 1})",
                )
    require_file(
        profile, "Python snapshot profile training did not publish its sidecar"
    )


def _snapshot_python_demo(
    config: SnapshotConfig,
    backend: HostBackend,
    *,
    app: str,
    marker: str,
    cold_heading: str,
    capture_heading: str,
    restore_heading: str,
) -> None:
    vm_config = VmConfig(config.kernel, config.initrd, config.mem)
    executable = require_vm_inputs(vm_config, backend)
    boot = VmConfig(
        config.kernel, config.initrd, config.mem, 1, PYTHON_SNAPSHOT_CMDLINE
    )
    with published_workload(app) as workload:
        mount: list[str | Path] = ["--mount", workload]
        cold_args = cold_boot_args(
            executable,
            boot,
            backend,
            [*mount, "--exit-on-boot", "--quiet", "--boot-marker", marker],
        )
        print(cold_heading)
        cold = collect_metrics(cold_args, config.runs, timeout=40)
        require_samples(cold, f"{app} cold boot", config.runs)
        print(f"  cold:    {format_median(cold)}")

        print(capture_heading)
        capture_args = cold_boot_args(
            executable,
            boot,
            backend,
            [*mount, "--snapshot", config.snapshot, "--quiet"],
        )
        capture_snapshot(capture_args, config.snapshot)
        state = config.snapshot / "state.bin"
        memory = config.snapshot / "mem.bin"
        footprint = backend.allocated_size(memory) / (1024 * 1024)
        print(
            f"  snapshot: {config.snapshot} (state.bin {state.stat().st_size} B; "
            f"mem.bin footprint ~{footprint:.0f} MiB on disk)"
        )

        print(restore_heading)
        restore_args: list[str | Path] = [
            executable,
            "--restore",
            config.snapshot,
            "--mem",
            str(config.mem),
            *mount,
            "--exit-on-boot",
            "--quiet",
            "--boot-marker",
            marker,
        ]
        restored = collect_metrics(restore_args, config.runs)
        require_samples(restored, f"{app} snapshot restore", config.runs)
        print(f"  restore: {format_median(restored)}")


def snapshot_pandas_demo(config: SnapshotConfig, backend: HostBackend) -> None:
    _snapshot_python_demo(
        config,
        backend,
        app="pandas.py",
        marker="{'x': 10, 'y': 30}",
        cold_heading=(
            "== cold boot -> pandas result "
            "(kernel boot + Python startup + pandas/numpy import) =="
        ),
        capture_heading="== taking snapshot at the generic Python trampoline boundary ==",
        restore_heading="== restore trampoline -> mount -> pandas import/result ==",
    )


def snapshot_hello_demo(config: SnapshotConfig, backend: HostBackend) -> None:
    _snapshot_python_demo(
        config,
        backend,
        app="hello.py",
        marker="hello world",
        cold_heading="== cold boot -> pure Python hello ==",
        capture_heading="== taking snapshot at the generic Python trampoline boundary ==",
        restore_heading="== restore trampoline -> mount host workload -> hello ==",
    )


def benchmark_shell_snapshot(
    config: SnapshotConfig,
    backend: HostBackend,
    memories: Sequence[int],
) -> None:
    vm_config = VmConfig(config.kernel, config.initrd, config.mem)
    executable = require_vm_inputs(vm_config, backend)
    cmdline = "earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1"
    print(
        f"boot-to-shell: cold PVH boot vs snapshot restore, median of {config.runs} runs, 1 vCPU"
    )
    print(f'marker : "{BOOT_MARKER}"')
    print(f"kernel : {config.kernel}")
    print(f"initrd : {config.initrd}")
    print()

    for memory_mib in memories:
        cold_args = cold_boot_args(
            executable,
            VmConfig(config.kernel, config.initrd, memory_mib, 1, cmdline),
            backend,
            ["--exit-on-boot", "--quiet", "--boot-marker", BOOT_MARKER],
        )
        cold = collect_metrics(cold_args, config.runs)
        snapshot = Path(f"{config.snapshot}-{memory_mib}")
        capture_args = cold_boot_args(
            executable,
            VmConfig(
                config.kernel,
                config.initrd,
                memory_mib,
                1,
                f"{cmdline} shellsnap",
            ),
            backend,
            ["--snapshot", snapshot, "--quiet"],
        )
        capture_snapshot(capture_args, snapshot, timeout=30)
        restore_args: list[str | Path] = [
            executable,
            "--restore",
            snapshot,
            "--mem",
            str(memory_mib),
            "--exit-on-boot",
            "--quiet",
            "--boot-marker",
            BOOT_MARKER,
        ]
        restored = collect_metrics(restore_args, config.runs)
        require_samples(cold, f"shell cold boot ({memory_mib} MiB)", config.runs)
        require_samples(restored, f"shell restore ({memory_mib} MiB)", config.runs)
        _print_shell_snapshot_summary(memory_mib, cold, restored)


def _print_shell_snapshot_summary(
    memory_mib: int, cold: Sequence[float], restored: Sequence[float]
) -> None:
    split_ms = 900.0
    fast = [value for value in cold if value < split_ms]
    slow = [value for value in cold if value >= split_ms]

    def line(name: str, values: Sequence[float]) -> str:
        if not values:
            return f"  {name:<20}: NO DATA"
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
            f"+~{statistics.median(slow) - statistics.median(fast):.0f} ms TSC PIT-calib)"
        )
    print(line("snapshot restore", restored))
    if restored and statistics.median(restored) > 0:
        base = statistics.median(fast) if fast else statistics.median(cold)
        print(
            f"  {'speedup':<20}: {base / statistics.median(restored):.0f}x "
            f"(fast-path cold) .. {statistics.median(cold) / statistics.median(restored):.0f}x "
            "(median cold) faster via snapshot"
        )
    print()


def snapshot_boot(
    config: SnapshotConfig,
    backend: HostBackend,
    *,
    smoke_test: bool = False,
) -> int:
    executable = backend.executable()
    require_file(executable, "build the VMM first: cargo build --release")
    cmdline = (
        "earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1 "
        "nvx_mode=app pyapp=repl.py"
    )
    state = config.snapshot / "state.bin"
    memory = config.snapshot / "mem.bin"
    if not state.is_file() or not memory.is_file():
        require_file(
            config.kernel, f"missing kernel: {config.kernel} ({backend.build_hint})"
        )
        require_file(
            config.initrd,
            f"missing python initramfs: {config.initrd} ({backend.build_hint})",
        )
        print(
            f">> no snapshot at {config.snapshot} yet; capturing a warmed interpreter "
            "(one-off cold boot)"
        )
        capture_args = cold_boot_args(
            executable,
            VmConfig(config.kernel, config.initrd, config.mem, 1, cmdline),
            backend,
            ["--snapshot", config.snapshot, "--log-level", "warn"],
        )
        capture_snapshot(capture_args, config.snapshot)

    print(
        f">> resuming interactive Python interpreter from snapshot {config.snapshot} "
        "(Ctrl-D or exit() to quit)"
    )
    restore_args: list[str | Path] = [
        executable,
        "--restore",
        config.snapshot,
        "--mem",
        str(config.mem),
    ]
    if not smoke_test:
        return subprocess.call([str(value) for value in restore_args])
    result = run_capture(restore_args, input_text="exit()\n", timeout=30)
    print(result.text)
    if result.timed_out:
        raise ScriptError("snapshot restore smoke test timed out")
    if result.returncode != 0:
        raise ScriptError(f"snapshot restore smoke test exited {result.returncode}")
    if "resumed from snapshot" not in result.text:
        raise ScriptError("restored Python banner was not observed")
    return 0
