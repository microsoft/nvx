"""Cross-platform device and networking benchmark workflows."""

from __future__ import annotations

import functools
import http.server
import ipaddress
import re
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
METRIC_PATTERN = re.compile(r"(?:cold-start|restore):\s*([0-9.]+)")


@dataclass(frozen=True)
class VirtfsConfig:
    kernel: Path
    initrd: Path
    mem: int = 512
    runs: int = 5
    vcpus: int = 1
    payload_mib: int = 64


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


def network_gateway(spec: str) -> str:
    try:
        interface = ipaddress.IPv4Interface(spec)
    except ValueError as error:
        raise ScriptError(f"invalid --net IPv4 CIDR {spec!r}: {error}") from error
    if not 1 <= interface.network.prefixlen <= 30:
        raise ScriptError(f"--net prefix must be in 1..=30, got {spec!r}")
    return str(interface.network.network_address + 1)


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
    if config.runs < 1 or config.payload_mib < 1:
        raise ScriptError("runs and payload MiB must be positive")

    ready = "until /bin/busybox true 2>/dev/null; do :; done\n"
    io_script = (
        ready
        + f"/bin/busybox dd if=/dev/zero of=/mnt/host/bench.bin bs=1M count={config.payload_mib} conv=fsync 2>&1\n"
        + "/bin/busybox sync\n"
        + "echo 3 > /proc/sys/vm/drop_caches 2>/dev/null\n"
        + "/bin/busybox dd if=/mnt/host/bench.bin of=/dev/null bs=1M 2>&1\n"
        + "/bin/busybox sync\n/bin/busybox reboot -f\n"
    )
    exchange_script = (
        ready
        + "printf 'guest-to-host\\n' > /mnt/host/guest-visible\n"
        + "tries=0\n"
        + "while [ \"$(cat /mnt/host/host-visible 2>/dev/null)\" != host-to-guest ] && "
        + "[ \"$tries\" -lt 1200 ]; do sleep 0.05; tries=$((tries + 1)); done\n"
        + "[ \"$(cat /mnt/host/host-visible 2>/dev/null)\" = host-to-guest ] && "
        + "echo VIRTFS-LIVE-ROUNDTRIP-OK\n"
        + "/bin/busybox reboot -f\n"
    )

    with tempfile.TemporaryDirectory(prefix="nvx-virtfs-") as temporary:
        work = Path(temporary)
        seed = work / "seed"
        seed.mkdir()
        (seed / "README").write_text("virt-fs benchmark seed", encoding="utf-8")

        print(
            f"virt-fs benchmark: {config.payload_mib} MiB payload, {config.mem} MiB guest, "
            f"{config.vcpus} vCPU, live host directory, median of {config.runs} runs"
        )
        print()
        print("== sequential throughput (guest dd, conv=fsync writes) ==")
        _run_io_benchmark(
            "rw live host directory",
            ["--mount", seed, "--mount-rw"],
            config,
            backend,
            io_script,
        )
        print()

        print("== live host <-> guest visibility (same running VM) ==")
        exchange_times: list[float] = []
        verified = 0
        for run_number in range(1, config.runs + 1):
            exchange_times.append(
                _run_live_exchange(config, backend, seed, exchange_script, run_number)
            )
            verified += 1
        print(
            f"  live exchange (cold each)         : {format_median(exchange_times, width=7)}"
        )
        print(
            f"  bidirectional visibility verified : {verified}/{config.runs} runs"
        )


def _run_live_exchange(
    config: VirtfsConfig,
    backend: HostBackend,
    directory: Path,
    script: str,
    run_number: int,
) -> float:
    guest_visible = directory / "guest-visible"
    host_visible = directory / "host-visible"
    guest_visible.unlink(missing_ok=True)
    host_visible.write_bytes(b"waiting\n")
    errors: list[str] = []

    def exchange() -> None:
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            try:
                if guest_visible.read_text(encoding="utf-8") == "guest-to-host\n":
                    host_visible.write_bytes(b"host-to-guest\n")
                    return
            except (FileNotFoundError, PermissionError, UnicodeError):
                pass
            time.sleep(0.01)
        errors.append("host did not observe the guest-created marker")

    worker = threading.Thread(target=exchange, name="virtfs-live-exchange", daemon=True)
    worker.start()
    started = time.perf_counter()
    result = _guest_run(
        config,
        backend,
        script,
        ["--mount", directory, "--mount-rw"],
    )
    elapsed_ms = (time.perf_counter() - started) * 1000
    worker.join(timeout=1)
    if worker.is_alive():
        errors.append("host exchange worker did not finish")
    if errors or "VIRTFS-LIVE-ROUNDTRIP-OK" not in result.text:
        detail = "; ".join(errors) or "guest did not observe the host update"
        raise ScriptError(
            f"live virt-fs exchange {run_number}/{config.runs} failed: {detail}\n"
            + _failure_tail(result.text)
        )
    return elapsed_ms


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
        gateway = network_gateway(config.net)
        probe_cmdline = f"{QUIET_CMDLINE} virtnet_probe={gateway}"
        cold_marker = f"VIRTNET-PROBE-OK: {gateway}"
        restore_marker = f"NETSNAP-RESTORE-PROBE-OK: {gateway}"
        print("== cold boot -> verified gateway connectivity ==")
        cold_args = cold_boot_args(
            executable,
            VmConfig(
                config.kernel,
                config.initrd,
                config.mem,
                config.vcpus,
                probe_cmdline,
            ),
            backend,
            [
                "--net",
                config.net,
                "--exit-on-boot",
                "--quiet",
                "--boot-marker",
                cold_marker,
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
                f"{probe_cmdline} netsnap",
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

        print("== restore -> verified gateway connectivity ==")
        restore_args: list[str | Path] = [
            executable,
            "--restore",
            config.snapshot,
            "--mem",
            str(config.mem),
            "--exit-on-boot",
            "--quiet",
            "--boot-marker",
            restore_marker,
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
