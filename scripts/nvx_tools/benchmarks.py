"""Cross-platform device and networking benchmark workflows."""

from __future__ import annotations

import functools
import http.server
import json
import re
import shutil
import statistics
import tempfile
import threading
import time
import urllib.request
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator, Sequence

from .backends.base import HostBackend
from .common import (
    CommandResult,
    ScriptError,
    diagnostic_tail,
    format_size,
    remove_tree,
    require_success,
    run_capture,
)
from .vm import (
    BOOT_MARKER,
    SnapshotConfig,
    VmConfig,
    capture_snapshot,
    cold_boot_args,
    format_median,
    require_samples,
    require_vm_inputs,
)


QUIET_CMDLINE = "earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1"
VIRTFS_CMDLINE = "console=hvc0 quiet loglevel=0 reboot=t panic=-1"
HCS_QUIET_CMDLINE = (
    "console=ttyS0,115200 8250_core.nr_uarts=2 "
    "8250_core.skip_txen_test=1 quiet loglevel=0 panic=-1"
)
RATE_PATTERN = re.compile(r"([0-9.]+)\s*([KMG]?)B/s")
CHECKSUM_PATTERN = re.compile(r"(?m)^\s*([0-9]+)\s+[0-9]+\s+.*data\.bin\s*$")
METRIC_PATTERN = re.compile(r"(?:cold-start|restore):\s*([0-9.]+)")


@dataclass(frozen=True)
class VirtfsConfig:
    kernel: Path
    initrd: Path
    mem: int = 512
    runs: int = 5
    vcpus: int = 1
    payload_mib: int = 64
    image_mib: int = 192


@dataclass(frozen=True)
class NetworkSnapshotConfig:
    kernel: Path
    initrd: Path
    snapshot: Path
    mem: int = 256
    runs: int = 10
    vcpus: int = 1
    net: str = "10.0.0.2/24"


@dataclass(frozen=True)
class NetworkPythonConfig:
    kernel: Path
    initrd: Path
    mem: int = 512
    runs: int = 8
    vcpus: int = 1
    net: str = "10.0.0.2/24"
    port: int = 8099


@dataclass(frozen=True)
class HcsNetworkSnapshotConfig:
    kernel: Path
    initrd: Path
    snapshot: Path
    mem: int = 512
    runs: int = 8
    net: str = "10.0.0.2/24"
    port: int = 8099
    endpoint_config: Path | None = None


def hcs_cold_boot_args(
    executable: Path,
    config: SnapshotConfig,
    memory_mib: int,
    cmdline: str,
    marker: str,
) -> list[str | Path]:
    return [
        executable,
        "--backend",
        "hcs",
        "--kernel",
        config.kernel,
        "--initrd",
        config.initrd,
        "--mem",
        str(memory_mib),
        "--cmdline",
        cmdline,
        "--exit-on-boot",
        "--quiet",
        "--boot-marker",
        marker,
    ]


def hcs_snapshot_capture_args(
    executable: Path,
    config: SnapshotConfig,
    snapshot: Path,
    memory_mib: int,
    cmdline: str,
) -> list[str | Path]:
    return [
        executable,
        "--backend",
        "hcs",
        "--kernel",
        config.kernel,
        "--initrd",
        config.initrd,
        "--mem",
        str(memory_mib),
        "--cmdline",
        cmdline,
        "--snapshot",
        snapshot,
        "--quiet",
        "--log-level",
        "info",
    ]


def hcs_snapshot_restore_args(
    executable: Path, snapshot: Path, marker: str
) -> list[str | Path]:
    return [
        executable,
        "--backend",
        "hcs",
        "--restore",
        snapshot,
        "--exit-on-boot",
        "--quiet",
        "--boot-marker",
        marker,
    ]


def _failure_tail(text: str, lines: int = 30) -> str:
    return diagnostic_tail(text, lines)


def _guest_run(
    config: VirtfsConfig,
    backend: HostBackend,
    script: str,
    mount_args: Sequence[str | Path],
) -> CommandResult:
    executable = backend.executable()
    args = cold_boot_args(
        executable,
        VmConfig(
            config.kernel,
            config.initrd,
            config.mem,
            config.vcpus,
            VIRTFS_CMDLINE,
        ),
        backend,
        [
            "--log-level",
            "off",
            "--defer-stdin-until-boot",
            "--boot-marker",
            "/ # ",
            *mount_args,
        ],
    )
    result = run_capture(args, input_text=script, timeout=120)
    require_success(result, "virt-fs guest run")
    return result


def parse_dd_rate(text: str, occurrence: int) -> float | None:
    lines = [line for line in text.splitlines() if "copied" in line]
    if len(lines) < occurrence:
        return None
    matches = RATE_PATTERN.findall(lines[occurrence - 1])
    if not matches:
        return None
    raw_value, prefix = matches[-1]
    multiplier = {"": 1e-6, "K": 1e-3, "M": 1.0, "G": 1e3}[prefix]
    return float(raw_value) * multiplier


def parse_data_checksum(text: str) -> str | None:
    match = CHECKSUM_PATTERN.search(text)
    return match.group(1) if match else None


def format_rate_median(values: Sequence[float]) -> str:
    positive = [value for value in values if value > 0]
    if not positive:
        return "     n/a  (too fast for busybox dd to time)"
    return (
        f"{statistics.median(positive):8.1f} MB/s  "
        f"(min {min(positive):.0f}, max {max(positive):.0f}, n={len(positive)})"
    )


def benchmark_virtfs(config: VirtfsConfig, backend: HostBackend) -> None:
    require_vm_inputs(VmConfig(config.kernel, config.initrd, config.mem), backend)
    backend.prepare_virtfs_benchmark()
    if config.runs < 1 or config.payload_mib < 1 or config.image_mib < 1:
        raise ScriptError("runs, payload MiB, and image MiB must be positive")

    ready = "until /bin/busybox true 2>/dev/null; do :; done\n"
    io_script = (
        ready
        + f"/bin/busybox dd if=/dev/zero of=/mnt/host/bench.bin bs=1M count={config.payload_mib} conv=fsync 2>&1\n"
        + "/bin/busybox sync\n"
        + "echo 3 > /proc/sys/vm/drop_caches 2>/dev/null\n"
        + "/bin/busybox dd if=/mnt/host/bench.bin of=/dev/null bs=1M 2>&1\n"
        + "/bin/busybox sync\n/bin/busybox reboot -f\n"
    )
    create_script = (
        ready
        + f"/bin/busybox dd if=/dev/zero of=/mnt/host/data.bin bs=1M count={config.payload_mib} 2>/dev/null\n"
        + "/bin/busybox cksum /mnt/host/data.bin\n/bin/busybox sync\n"
        + "/bin/busybox reboot -f\n"
    )
    verify_script = (
        ready
        + "/bin/busybox cksum /mnt/host/data.bin 2>/dev/null\n"
        + "/bin/busybox reboot -f\n"
    )

    with tempfile.TemporaryDirectory(prefix="nvx-virtfs-") as temporary:
        work = Path(temporary)
        seed = work / "seed"
        seed.mkdir()
        (seed / "README").write_text("virt-fs benchmark seed", encoding="utf-8")
        persistent_image = work / "persist.img"
        round_image = work / "round.img"

        print(
            f"virt-fs benchmark: {config.payload_mib} MiB payload, {config.mem} MiB guest, "
            f"{config.vcpus} vCPU, image {config.image_mib} MiB, median of {config.runs} runs"
        )
        print()
        print("== sequential throughput (guest dd, conv=fsync writes) ==")
        _run_io_benchmark(
            "rw ephemeral (in-memory)",
            ["--mount", seed, "--mount-rw", "--mount-size", str(config.image_mib)],
            config,
            backend,
            io_script,
        )
        _run_io_benchmark(
            "rw persistent (file-backed)",
            [
                "--mount",
                seed,
                "--mount-image",
                persistent_image,
                "--mount-size",
                str(config.image_mib),
            ],
            config,
            backend,
            io_script,
        )
        persistent_image.unlink(missing_ok=True)
        print()

        print("== persistence round-trip (rw --mount-image) ==")
        mount_args: list[str | Path] = [
            "--mount",
            seed,
            "--mount-image",
            round_image,
            "--mount-size",
            str(config.image_mib),
        ]
        started = time.perf_counter()
        create_result = _guest_run(config, backend, create_script, mount_args)
        create_ms = (time.perf_counter() - started) * 1000
        checksum = parse_data_checksum(create_result.text)
        if checksum is None:
            raise ScriptError(
                "persistent image creation did not report a checksum\n"
                + _failure_tail(create_result.text)
            )
        print(
            f"  create image + write {config.payload_mib} MiB       : {create_ms:.0f} ms"
        )
        disk_size = backend.allocated_size(round_image)
        print(
            f"  host image on disk                : {round_image.name} "
            f"({format_size(disk_size)})"
        )

        reuse: list[float] = []
        verified = 0
        for run_number in range(1, config.runs + 1):
            started = time.perf_counter()
            result = _guest_run(config, backend, verify_script, mount_args)
            reuse.append((time.perf_counter() - started) * 1000)
            actual = parse_data_checksum(result.text)
            if actual != checksum:
                raise ScriptError(
                    f"persistent image verification {run_number}/{config.runs} checksum "
                    f"mismatch ({actual} != {checksum})\n{_failure_tail(result.text)}"
                )
            verified += 1
        print(f"  reuse image + verify (cold each)  : {format_median(reuse, width=7)}")
        print(
            f"  payload survived across runs      : {verified}/{config.runs} runs "
            f"(cksum {checksum})"
        )
        _print_host_image_confirmation(round_image, backend)


def _run_io_benchmark(
    label: str,
    mount_args: Sequence[str | Path],
    config: VirtfsConfig,
    backend: HostBackend,
    script: str,
) -> None:
    write_rates: list[float] = []
    read_rates: list[float] = []
    for run_number in range(1, config.runs + 1):
        result = _guest_run(config, backend, script, mount_args)
        write_rate = parse_dd_rate(result.text, 1)
        read_rate = parse_dd_rate(result.text, 2)
        if write_rate is None or read_rate is None:
            raise ScriptError(
                f"{label} run {run_number}/{config.runs} did not report both dd rates\n"
                + _failure_tail(result.text)
            )
        write_rates.append(write_rate)
        read_rates.append(read_rate)
    print(f"  {label:<27} write {format_rate_median(write_rates)}")
    print(f"  {'':<27} read  {format_rate_median(read_rates)}")


def _print_host_image_confirmation(path: Path, backend: HostBackend) -> None:
    if backend.name != "linux-kvm" or shutil.which("debugfs") is None:
        return
    result = run_capture(["debugfs", "-R", "stat /data.bin", path])
    match = re.search(r"\bSize:\s*([0-9]+)", result.text)
    value = match.group(1) if match else "not found"
    print(f"  host sees /data.bin in image      : {value} bytes (debugfs)")


def _run_timed(
    args: Sequence[str | Path], timeout: int, *, graceful_timeout: bool = False
) -> tuple[float | None, float, CommandResult]:
    started = time.perf_counter()
    result = run_capture(args, timeout=timeout, graceful_timeout=graceful_timeout)
    wall_ms = (time.perf_counter() - started) * 1000
    require_success(result, "VM benchmark run")
    match = METRIC_PATTERN.search(result.text)
    metric = float(match.group(1)) if match else None
    return metric, wall_ms, result


def _require_hcs_benchmark(config: SnapshotConfig, backend: HostBackend) -> Path:
    if backend.name != "windows-whp":
        raise ScriptError("HCS snapshot benchmarks require a Windows host")
    executable = require_vm_inputs(
        VmConfig(config.kernel, config.initrd, config.mem), backend
    )
    preflight = run_capture(
        [executable, "--backend", "hcs", "--selftest", "--log-level", "warn"],
        timeout=30,
    )
    require_success(preflight, "HCS benchmark preflight")
    return executable


def _collect_hcs_timings(
    args: Sequence[str | Path],
    runs: int,
    label: str,
    timeout: int,
    *,
    required_text: str | None = None,
) -> tuple[list[float], list[float]]:
    guest: list[float] = []
    wall: list[float] = []
    for run_number in range(1, runs + 1):
        print(
            f"  {label}: run {run_number}/{runs} (timeout {timeout}s)...",
            flush=True,
        )
        metric, wall_ms, result = _run_timed(args, timeout, graceful_timeout=True)
        if metric is None:
            raise ScriptError(
                f"{label} run {run_number}/{runs} did not report a timing marker\n"
                + _failure_tail(result.text)
            )
        if required_text is not None and required_text not in result.text:
            raise ScriptError(
                f"{label} run {run_number}/{runs} did not emit {required_text!r}\n"
                + _failure_tail(result.text)
            )
        guest.append(metric)
        wall.append(wall_ms)
        print(
            f"  {label}: run {run_number}/{runs} complete "
            f"(guest {metric:.1f} ms, wall {wall_ms:.1f} ms)",
            flush=True,
        )
    return guest, wall


def _validate_hcs_snapshot(
    snapshot: Path, *, require_network: bool = False
) -> tuple[Path, Path]:
    manifest = snapshot / "manifest.json"
    state = snapshot / "runtime.vmrs"
    if not manifest.is_file() or not state.is_file():
        raise ScriptError(
            f"HCS snapshot is incomplete: expected {manifest} and {state}"
        )
    try:
        document = json.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ScriptError(f"could not parse HCS snapshot manifest {manifest}: {error}") from error
    if not isinstance(document, dict) or (
        document.get("format") != "NVXHCSS1"
        or document.get("version") not in (1, 2, 3)
        or document.get("backend") != "hcs"
    ):
        raise ScriptError(f"unexpected HCS snapshot manifest contract in {manifest}")
    if state.stat().st_size == 0:
        raise ScriptError(f"HCS snapshot state is empty: {state}")
    if require_network:
        network = document.get("network")
        if document.get("version") != 3 or not isinstance(network, dict):
            raise ScriptError(
                f"HCS network snapshot lacks manifest v3 external network identity in {manifest}"
            )
        required = {
            "network_id",
            "endpoint_id",
            "adapter_id",
            "guest_ip",
            "prefix",
            "gateway",
            "mac_address",
        }
        if required - network.keys() or not network.get("mac_address"):
            raise ScriptError(f"HCS network snapshot identity is incomplete in {manifest}")
    return manifest, state


def _capture_hcs_snapshot(
    args: Sequence[str | Path],
    snapshot: Path,
    timeout: int,
    *,
    require_network: bool = False,
) -> float:
    remove_tree(snapshot, label="HCS snapshot")
    print(f"  HCS snapshot capture: {snapshot}...", flush=True)
    started = time.perf_counter()
    result = run_capture(args, timeout=timeout, graceful_timeout=True)
    wall_ms = (time.perf_counter() - started) * 1000
    try:
        require_success(result, "HCS snapshot capture")
        _validate_hcs_snapshot(snapshot, require_network=require_network)
    except (OSError, ScriptError):
        remove_tree(snapshot, label="incomplete HCS snapshot")
        raise
    print(f"  HCS snapshot capture complete ({wall_ms:.1f} ms)", flush=True)
    return wall_ms


def _print_hcs_snapshot_summary(
    cold: Sequence[float],
    cold_wall: Sequence[float],
    capture_wall_ms: float,
    restored: Sequence[float],
    restore_wall: Sequence[float],
    snapshot: Path,
    backend: HostBackend,
) -> None:
    manifest, state = _validate_hcs_snapshot(snapshot)
    allocated = backend.allocated_size(state)
    print(f"  cold guest latency    : {format_median(cold)}")
    print(f"  cold process wall     : {format_median(cold_wall)}")
    print(f"  one-off capture wall  : {capture_wall_ms:.1f} ms")
    print(f"  restore guest latency : {format_median(restored)}")
    print(f"  restore process wall  : {format_median(restore_wall)}")
    print(
        f"  snapshot files        : manifest {format_size(manifest.stat().st_size)}, "
        f"VMRS {format_size(state.stat().st_size)} logical / {format_size(allocated)} allocated"
    )
    if cold and restored and statistics.median(restored) > 0:
        print(
            f"  guest-latency speedup : "
            f"{statistics.median(cold) / statistics.median(restored):.1f}x"
        )


def benchmark_hcs_snapshot_shell(
    config: SnapshotConfig, backend: HostBackend, memories: Sequence[int]
) -> None:
    if config.runs < 1:
        raise ScriptError("number of runs must be at least 1")
    if not memories or any(memory < 1 for memory in memories):
        raise ScriptError("HCS benchmark memory sizes must be positive")
    executable = _require_hcs_benchmark(config, backend)
    print(
        f"HCS shell snapshot benchmark, median of {config.runs} runs per memory size"
    )
    print(f'  marker: "{BOOT_MARKER}"')
    print(f"  kernel: {config.kernel}")
    print(f"  initrd: {config.initrd}")
    print()

    for memory_mib in memories:
        snapshot = Path(f"{config.snapshot}-{memory_mib}")
        cold_args = hcs_cold_boot_args(
            executable,
            config,
            memory_mib,
            HCS_QUIET_CMDLINE,
            BOOT_MARKER,
        )
        capture_args = hcs_snapshot_capture_args(
            executable,
            config,
            snapshot,
            memory_mib,
            f"{HCS_QUIET_CMDLINE} shellsnap",
        )
        restore_args = hcs_snapshot_restore_args(executable, snapshot, BOOT_MARKER)

        cold, cold_wall = _collect_hcs_timings(
            cold_args, config.runs, f"HCS cold boot ({memory_mib} MiB)", 90
        )
        capture_wall_ms = _capture_hcs_snapshot(capture_args, snapshot, 120)
        restored, restore_wall = _collect_hcs_timings(
            restore_args, config.runs, f"HCS restore ({memory_mib} MiB)", 90
        )
        print(f"== {memory_mib} MiB ==")
        _print_hcs_snapshot_summary(
            cold,
            cold_wall,
            capture_wall_ms,
            restored,
            restore_wall,
            snapshot,
            backend,
        )
        print()


def benchmark_hcs_snapshot_python(
    config: SnapshotConfig, backend: HostBackend
) -> None:
    if config.runs < 1:
        raise ScriptError("number of runs must be at least 1")
    executable = _require_hcs_benchmark(config, backend)
    marker = "{'x': 10, 'y': 30}"
    cmdline = f"{HCS_QUIET_CMDLINE} pyapp=hello.py"
    cold_args = hcs_cold_boot_args(
        executable, config, config.mem, cmdline, marker
    )
    capture_args = hcs_snapshot_capture_args(
        executable, config, config.snapshot, config.mem, cmdline
    )
    restore_args = hcs_snapshot_restore_args(executable, config.snapshot, marker)

    print(
        f"HCS warmed Python snapshot benchmark, median of {config.runs} runs, "
        f"{config.mem} MiB"
    )
    cold, cold_wall = _collect_hcs_timings(
        cold_args, config.runs, "HCS Python cold boot", 150
    )
    capture_wall_ms = _capture_hcs_snapshot(capture_args, config.snapshot, 180)
    restored, restore_wall = _collect_hcs_timings(
        restore_args, config.runs, "HCS Python restore", 120
    )
    _print_hcs_snapshot_summary(
        cold,
        cold_wall,
        capture_wall_ms,
        restored,
        restore_wall,
        config.snapshot,
        backend,
    )


def benchmark_hcs_network_snapshot_python(
    config: HcsNetworkSnapshotConfig, backend: HostBackend
) -> None:
    if config.runs < 1:
        raise ScriptError("number of runs must be at least 1")
    snapshot_config = SnapshotConfig(
        config.kernel, config.initrd, config.snapshot, config.mem, config.runs
    )
    if config.endpoint_config is None or not config.endpoint_config.is_file():
        raise ScriptError(
            "HCS network benchmark requires --hcn-endpoint-config from setup-hcn-endpoint.ps1"
        )
    executable = _require_hcs_benchmark(snapshot_config, backend)
    marker = "HELLOPY-NET OK"
    cold_completion_marker = "NVX-HCS-NETWORK-DONE"
    cold_cmdline = (
        f"{HCS_QUIET_CMDLINE} pyapp=net-hello.py "
        f"netbench_cold=1 netbench_port={config.port}"
    )
    capture_cmdline = (
        f"{HCS_QUIET_CMDLINE} pyapp=net-hello.py netbench_port={config.port}"
    )
    cold_args = hcs_cold_boot_args(
        executable,
        snapshot_config,
        config.mem,
        cold_cmdline,
        cold_completion_marker,
    )
    endpoint_args = ["--hcn-endpoint-config", config.endpoint_config]
    cold_args.extend(["--net", config.net, *endpoint_args])
    cold_args.remove("--quiet")
    capture_args = hcs_snapshot_capture_args(
        executable,
        snapshot_config,
        config.snapshot,
        config.mem,
        capture_cmdline,
    )
    capture_args.extend(["--net", config.net, *endpoint_args])
    capture_args.remove("--quiet")
    restore_args = hcs_snapshot_restore_args(executable, config.snapshot, marker)
    restore_args.extend(endpoint_args)
    restore_args.remove("--quiet")

    with helper_server(config.port):
        print(
            f"HCS networked Python snapshot benchmark, median of {config.runs} runs, "
            f"{config.mem} MiB, --net {config.net}"
        )
        cold, cold_wall = _collect_hcs_timings(
            cold_args,
            config.runs,
            "HCS network cold boot",
            300,
            required_text=marker,
        )
        capture_wall_ms = _capture_hcs_snapshot(
            capture_args, config.snapshot, 300, require_network=True
        )
        restored, restore_wall = _collect_hcs_timings(
            restore_args, config.runs, "HCS network restore", 300
        )
        _print_hcs_snapshot_summary(
            cold,
            cold_wall,
            capture_wall_ms,
            restored,
            restore_wall,
            config.snapshot,
            backend,
        )
        print(f"  verified marker       : {marker}")


def benchmark_network_snapshot(
    config: NetworkSnapshotConfig, backend: HostBackend
) -> None:
    if config.runs < 1:
        raise ScriptError("number of runs must be at least 1")
    executable = require_vm_inputs(
        VmConfig(config.kernel, config.initrd, config.mem), backend
    )
    print(
        f"networking + snapshot benchmark, median of {config.runs}, {config.mem} MiB, "
        f"{config.vcpus} vCPU cold / 1 vCPU restore, --net {config.net} "
        f"({backend.network_description})"
    )
    print()
    try:
        print("== cold boot -> working-network shell (kernel boot + virtio-net + ifconfig) ==")
        cold_args = cold_boot_args(
            executable,
            VmConfig(
                config.kernel,
                config.initrd,
                config.mem,
                config.vcpus,
                QUIET_CMDLINE,
            ),
            backend,
            [
                "--net",
                config.net,
                "--exit-on-boot",
                "--quiet",
                "--boot-marker",
                BOOT_MARKER,
            ],
        )
        cold = [_run_timed(cold_args, 40)[0] for _ in range(config.runs)]
        cold_samples = [value for value in cold if value is not None]
        require_samples(cold_samples, "network cold boot", config.runs)
        print(f"  cold  (guest start -> marker): {format_median(cold_samples)}")

        print("== capture a warmed, network-configured snapshot (one-off) ==")
        capture_args = cold_boot_args(
            executable,
            VmConfig(
                config.kernel,
                config.initrd,
                config.mem,
                1,
                f"{QUIET_CMDLINE} netsnap",
            ),
            backend,
            ["--net", config.net, "--snapshot", config.snapshot, "--quiet"],
        )
        capture_snapshot(capture_args, config.snapshot)
        memory = config.snapshot / "mem.bin"
        footprint = backend.allocated_size(memory) / (1024 * 1024)
        print(
            f"  snapshot: {config.snapshot} (state.bin "
            f"{(config.snapshot / 'state.bin').stat().st_size} B; "
            f"mem.bin footprint ~{footprint:.0f} MiB on disk)"
        )

        print("== restore -> working-network shell (resume + rebuild backend + verify link) ==")
        restore_args: list[str | Path] = [
            executable,
            "--restore",
            config.snapshot,
            "--mem",
            str(config.mem),
            "--exit-on-boot",
            "--quiet",
            "--boot-marker",
            "NETSNAP-RESTORE-OK",
        ]
        restore_results = [_run_timed(restore_args, 30) for _ in range(config.runs)]
        restore = [metric for metric, _, _ in restore_results if metric is not None]
        wall = [wall_ms for metric, wall_ms, _ in restore_results if metric is not None]
        require_samples(restore, "network snapshot restore", config.runs)
        require_samples(wall, "network snapshot wall clock", config.runs)
        print(f"  restore (guest resume -> marker): {format_median(restore)}")
        print("== end-to-end wall-clock (process start -> exit) ==")
        print(f"  restore wall-clock             : {format_median(wall)}")
    finally:
        backend.cleanup_network()


class _QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, format: str, *args: object) -> None:
        return None


@contextmanager
def helper_server(port: int) -> Iterator[None]:
    if not 1 <= port <= 65535:
        raise ScriptError("HTTP helper port must be between 1 and 65535")
    with tempfile.TemporaryDirectory(prefix="nvx-www-") as temporary:
        root = Path(temporary)
        (root / "index.html").write_text("HELLO-HOST", encoding="ascii")
        handler = functools.partial(_QuietHandler, directory=str(root))
        try:
            server = http.server.ThreadingHTTPServer(("0.0.0.0", port), handler)
        except OSError as error:
            raise ScriptError(
                f"host helper server could not bind port {port}: {error}"
            ) from error
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=2) as response:
                if b"HELLO-HOST" not in response.read():
                    raise ScriptError("host helper server returned unexpected content")
            yield
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


def benchmark_network_python(
    config: NetworkPythonConfig, backend: HostBackend
) -> None:
    executable = require_vm_inputs(
        VmConfig(config.kernel, config.initrd, config.mem), backend
    )
    if config.runs < 1:
        raise ScriptError("number of runs must be at least 1")
    try:
        with helper_server(config.port):
            print(
                f"networked Python snapshot benchmark, median of {config.runs}, "
                f"{config.mem} MiB, {config.vcpus} vCPU cold / 1 vCPU restore, "
                f"--net {config.net} ({backend.network_description})"
            )
            print()
            _benchmark_python_app(
                config,
                backend,
                executable,
                "net-hello.py",
                "HELLOPY-NET OK",
                "(a) hello-world Python (bare interpreter + live NIC)",
            )
            print()
            _benchmark_python_app(
                config,
                backend,
                executable,
                "net-pandas.py",
                "PANDASPY-NET OK",
                "(b) numpy + pandas app (warmed interpreter + live NIC)",
            )
    finally:
        backend.cleanup_network()


def _benchmark_python_app(
    config: NetworkPythonConfig,
    backend: HostBackend,
    executable: Path,
    app: str,
    marker: str,
    label: str,
) -> None:
    snapshot = backend.artifact(f"nspy_{app.removesuffix('.py')}")
    print(f"== {label} ==")
    try:
        cold_args = cold_boot_args(
            executable,
            VmConfig(
                config.kernel,
                config.initrd,
                config.mem,
                config.vcpus,
                f"{QUIET_CMDLINE} pyapp={app} netbench_cold=1 netbench_port={config.port}",
            ),
            backend,
            [
                "--net",
                config.net,
                "--exit-on-boot",
                "--quiet",
                "--boot-marker",
                marker,
            ],
        )
        cold: list[float] = []
        for run_number in range(1, config.runs + 1):
            metric, _, result = _run_timed(cold_args, 60)
            if metric is None:
                raise ScriptError(
                    f"cold run {run_number}/{config.runs} for {app} did not reach "
                    f"'{marker}'\n{_failure_tail(result.text)}"
                )
            cold.append(metric)
        print(f"  cold    (guest start  -> marker): {format_median(cold)}")

        capture_args = cold_boot_args(
            executable,
            VmConfig(
                config.kernel,
                config.initrd,
                config.mem,
                1,
                f"{QUIET_CMDLINE} pyapp={app} netbench_port={config.port}",
            ),
            backend,
            ["--net", config.net, "--snapshot", snapshot, "--quiet"],
        )
        capture_snapshot(capture_args, snapshot, timeout=60)
        footprint = backend.allocated_size(snapshot / "mem.bin") / (1024 * 1024)
        print(f"  snapshot: mem.bin footprint ~{footprint:.0f} MiB on disk")

        restore_args: list[str | Path] = [
            executable,
            "--restore",
            snapshot,
            "--mem",
            str(config.mem),
            "--exit-on-boot",
            "--quiet",
            "--boot-marker",
            marker,
        ]
        restored: list[float] = []
        for run_number in range(1, config.runs + 1):
            metric, _, result = _run_timed(restore_args, 40)
            if metric is None:
                raise ScriptError(
                    f"restore run {run_number}/{config.runs} for {app} did not reach "
                    f"'{marker}'\n{_failure_tail(result.text)}"
                )
            restored.append(metric)
        print(f"  restore (guest resume -> marker): {format_median(restored)}")

        verification_args: list[str | Path] = [
            executable,
            "--restore",
            snapshot,
            "--mem",
            str(config.mem),
            "--exit-on-boot",
            "--boot-marker",
            marker,
            "--log-level",
            "warn",
        ]
        _, _, verification = _run_timed(verification_args, 40)
        match = re.search(re.escape(marker) + r"[A-Za-z' :{},0-9]*", verification.text)
        if match is None:
            raise ScriptError(
                f"restore verification for {app} did not emit '{marker}'\n"
                + _failure_tail(verification.text)
            )
        print(f"  verified: {match.group(0).strip()}")
    finally:
        remove_tree(snapshot, label="temporary network snapshot")
