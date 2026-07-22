"""Cross-platform device and networking benchmark workflows."""

from __future__ import annotations

import functools
import http.server
import re
import shutil
import socket
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
    VmConfig,
    capture_snapshot,
    cold_boot_args,
    format_median,
    require_samples,
    require_vm_inputs,
)


QUIET_CMDLINE = "earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1"
VIRTFS_CMDLINE = "console=hvc0 quiet loglevel=0 reboot=t panic=-1"
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
    result = run_capture(args, input_text=script, timeout=300)
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
    if result.timed_out or result.returncode != 0:
        diagnostic = _failure_tail(result.text, 80)
        if diagnostic:
            print(diagnostic, flush=True)
    require_success(result, "VM benchmark run")
    match = METRIC_PATTERN.search(result.text)
    metric = float(match.group(1)) if match else None
    return metric, wall_ms, result


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


def default_route_ipv4() -> str:
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.connect(("1.1.1.1", 53))
            address = probe.getsockname()[0]
    except OSError as error:
        raise ScriptError(
            f"could not resolve the host default-route IPv4 address: {error}"
        ) from error
    if address == "0.0.0.0" or address.startswith("127."):
        raise ScriptError(f"host default route resolved to unusable address {address}")
    return address


@contextmanager
def helper_server(port: int) -> Iterator[str]:
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
            yield default_route_ipv4()
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
