"""Cross-platform device and networking benchmark workflows."""

from __future__ import annotations

import base64
import functools
import http.server
import ipaddress
import json
import os
import re
import shlex
import socket
import statistics
import tempfile
import threading
import time
import urllib.request
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterator, Sequence

from .backends.base import HostBackend
from .common import (
    CommandResult,
    ScriptError,
    diagnostic_tail,
    remove_tree,
    require_file,
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
OUTPUT_MARKER = "NVX-EXEC-START"
OUTPUT_ORDER_MARKER = "NVX-ORDER-A-B-C-D-END"
DEFAULT_OUTPUT_SIZES = (1024, 4096, 16384, 65536)


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


@dataclass(frozen=True)
class ConsoleOutputConfig:
    microvm: Path
    snapshot: Path
    portb_snapshot: Path
    mem: int = 512
    runs: int = 5
    sizes: tuple[int, ...] = DEFAULT_OUTPUT_SIZES
    timeout: int = 60
    nanvix_command: str | None = None
    nanvix_wxc: Path | None = None


@dataclass(frozen=True)
class ConsoleSnapshotConfig:
    kernel: Path
    initrd: Path
    snapshot: Path
    portb_snapshot: Path
    mem: int = 512
    timeout: int = 60


def _run_nvx_capture(
    args: Sequence[str | Path],
    *,
    input_text: str = "",
    input_bytes: bytes | None = None,
    timeout: float | None = None,
    graceful_timeout: bool = False,
) -> CommandResult:
    return run_capture(
        args,
        input_text=input_text,
        input_bytes=input_bytes,
        timeout=timeout,
        graceful_timeout=graceful_timeout,
        collect_rss=True,
    )


def format_rss_median(values: Sequence[int | None], *, details: bool = True) -> str:
    samples = [value / (1024 * 1024) for value in values if value is not None]
    if not samples:
        return "n/a"
    median = statistics.median(samples)
    if not details:
        return f"{median:.1f} MiB"
    return (
        f"{median:7.1f} MiB  "
        f"(min {min(samples):.1f}, max {max(samples):.1f}, n={len(samples)})"
    )


def parse_output_sizes(spec: str) -> tuple[int, ...]:
    try:
        sizes = tuple(int(value) for value in spec.split())
    except ValueError as error:
        raise ScriptError("--sizes must contain integer byte counts") from error
    if not sizes or any(size < 1 or size > 64 * 1024 for size in sizes):
        raise ScriptError("--sizes must contain values in the range 1..65536")
    if len(set(sizes)) != len(sizes):
        raise ScriptError("--sizes must not contain duplicates")
    return sizes


def _console_output_script(size: int) -> str:
    return (
        "import os\n"
        f"size = {size}\n"
        "if size == 65536:\n"
        "    os.write(1, bytes((index * 31 + 7) & 255 for index in range(size)))\n"
        f"elif size >= {len(OUTPUT_ORDER_MARKER)}:\n"
        f"    remaining = size - {len(OUTPUT_ORDER_MARKER)}\n"
        "    stderr_size = remaining // 2\n"
        "    stdout_size = remaining - stderr_size\n"
        "    stderr = bytes((index * 17 + 11) & 255 for index in range(stderr_size))\n"
        "    stdout = bytes((index * 31 + 7) & 255 for index in range(stdout_size))\n"
        "    os.write(2, b'NVX-ORDER-A')\n"
        "    os.write(1, b'-B')\n"
        "    os.write(2, b'-C')\n"
        "    os.write(1, b'-D-END')\n"
        "    os.write(2, stderr)\n"
        "    os.write(1, stdout)\n"
        "else:\n"
        "    stdout_size = size // 2\n"
        "    stdout = bytes((index * 31 + 7) & 255 for index in range(stdout_size))\n"
        "    stderr = bytes((index * 17 + 11) & 255 for index in range(size - stdout_size))\n"
        "    os.write(2, stderr)\n"
        "    os.write(1, stdout)\n"
    )


def expected_console_output(size: int) -> tuple[bytes, bytes, bytes]:
    if not 1 <= size <= 64 * 1024:
        raise ScriptError("console output payload must be in the range 1..65536 bytes")
    if size == 64 * 1024:
        stdout = bytes((index * 31 + 7) & 255 for index in range(size))
        return stdout, b"", stdout
    if size >= len(OUTPUT_ORDER_MARKER):
        remaining = size - len(OUTPUT_ORDER_MARKER)
        stderr_size = remaining // 2
        stdout_size = remaining - stderr_size
        stderr_data = bytes((index * 17 + 11) & 255 for index in range(stderr_size))
        stdout_data = bytes((index * 31 + 7) & 255 for index in range(stdout_size))
        stderr = b"NVX-ORDER-A-C" + stderr_data
        stdout = b"-B-D-END" + stdout_data
        return (
            stdout,
            stderr,
            OUTPUT_ORDER_MARKER.encode("ascii") + stderr_data + stdout_data,
        )
    stdout_size = size // 2
    stdout = bytes((index * 31 + 7) & 255 for index in range(stdout_size))
    stderr = bytes((index * 17 + 11) & 255 for index in range(size - stdout_size))
    return stdout, stderr, stderr + stdout


def build_mxc_output_request(size: int) -> tuple[bytes, bytes, bytes, bytes]:
    expected_stdout, expected_stderr, expected_merged = expected_console_output(size)
    document = {
        "env": [],
        "entropy": [0] * 32,
        "unixTimeNs": time.time_ns(),
        "script": _console_output_script(size),
    }
    body = json.dumps(document, separators=(",", ":")).encode("utf-8")
    return (
        len(body).to_bytes(8, "big") + body,
        expected_stdout,
        expected_stderr,
        expected_merged,
    )


def _console_output_args(
    executable: Path, snapshot: Path, mem: int, *, validate_order: bool = False
) -> list[str | Path]:
    args: list[str | Path] = [
        executable,
        "--restore",
        snapshot,
        "--mem",
        str(mem),
        "--console",
        "auto",
        "--output-after-marker",
        OUTPUT_MARKER,
        "--log-level",
        "off",
    ]
    if validate_order:
        args.extend(["--timing-marker", f"console_order={OUTPUT_ORDER_MARKER}"])
    if os.name == "nt":
        args.extend(["--snapshot-prefetch", "off"])
    return args


def capture_console_snapshots(
    config: ConsoleSnapshotConfig,
    backend: HostBackend,
    *,
    runner: Callable[..., CommandResult] = run_capture,
) -> None:
    boot = VmConfig(
        config.kernel,
        config.initrd,
        config.mem,
        1,
        f"{QUIET_CMDLINE} nvx_mode=agent",
    )
    executable = require_vm_inputs(boot, backend)
    for transport, snapshot in (
        ("virtio", config.snapshot),
        ("portb", config.portb_snapshot),
    ):
        args = cold_boot_args(
            executable,
            boot,
            backend,
            [
                "--snapshot",
                snapshot,
                "--console",
                transport,
                "--quiet",
                "--log-level",
                "warn",
            ],
        )
        capture_snapshot(args, snapshot, timeout=config.timeout, runner=runner)
        print(f"captured {transport} console snapshot: {snapshot}")


def _nanvix_args(template: str, size: int) -> list[str]:
    if "{bytes}" not in template:
        raise ScriptError("--nanvix-command must contain a {bytes} placeholder")
    return [
        argument.replace("{bytes}", str(size))
        for argument in shlex.split(template, posix=os.name != "nt")
    ]


def _nanvix_wxc_args(executable: Path, size: int, timeout: int) -> list[str | Path]:
    document = {
        "process": {
            "commandLine": _console_output_script(size),
            "timeout": timeout * 1000,
        },
        "containment": "microvm",
    }
    encoded = base64.b64encode(
        json.dumps(document, separators=(",", ":")).encode("utf-8")
    ).decode("ascii")
    return [executable, "--experimental", "--config-base64", encoded]


def _measure_console_output(
    args: Sequence[str | Path],
    size: int,
    *,
    timeout: int,
    runner: Callable[..., CommandResult],
    clock: Callable[[], float],
    request: bytes | None,
    require_order_marker: bool,
) -> tuple[float, int | None]:
    _, expected_stdout, expected_stderr, expected_merged = build_mxc_output_request(
        size
    )
    started = clock()
    if request is None:
        result = runner(args, timeout=timeout)
    else:
        result = runner(args, input_bytes=request, timeout=timeout)
    elapsed = clock() - started
    require_success(result, "console output benchmark run")
    stderr = result.stderr
    if require_order_marker:
        marker = re.compile(
            rb"timing-marker: console_order [0-9]+(?:\.[0-9]+)? ms\r?\n"
        )
        matches = list(marker.finditer(stderr))
        if len(matches) != 1:
            raise ScriptError(
                "console output benchmark did not observe the ordered cross-channel marker"
            )
        match = matches[0]
        stderr = stderr[: match.start()] + stderr[match.end() :]
    actual_stdout = result.stdout
    actual_stderr = stderr
    expected_stdout = expected_merged
    expected_stderr = b""
    if actual_stdout != expected_stdout or actual_stderr != expected_stderr:
        raise ScriptError(
            "console output benchmark was not byte-faithful "
            f"(stdout expected/got {len(expected_stdout)}/{len(actual_stdout)} bytes; "
            f"stderr expected/got {len(expected_stderr)}/{len(actual_stderr)} bytes)"
        )
    return elapsed, result.peak_rss_bytes


def benchmark_console_output(
    config: ConsoleOutputConfig,
    *,
    runner: Callable[..., CommandResult] = _run_nvx_capture,
    clock: Callable[[], float] = time.perf_counter,
) -> None:
    if config.runs < 1:
        raise ScriptError("runs must be positive")
    require_file(config.microvm, f"missing VMM: {config.microvm}")
    for snapshot in (config.snapshot, config.portb_snapshot):
        require_file(
            snapshot / "state.bin", f"snapshot is missing {snapshot / 'state.bin'}"
        )
        require_file(
            snapshot / "mem.bin", f"snapshot is missing {snapshot / 'mem.bin'}"
        )
    if config.nanvix_command is not None and config.nanvix_wxc is not None:
        raise ScriptError("--nanvix-command and --nanvix-wxc are mutually exclusive")
    if config.nanvix_wxc is not None:
        require_file(config.nanvix_wxc, f"missing wxc-exec: {config.nanvix_wxc}")

    transports: list[tuple[str, bool, Callable[[int], Sequence[str | Path]]]] = [
        (
            "virtio-console",
            True,
            lambda _size: _console_output_args(
                config.microvm,
                config.snapshot,
                config.mem,
                validate_order=_size != 64 * 1024 and _size >= len(OUTPUT_ORDER_MARKER),
            ),
        ),
        (
            "portb",
            True,
            lambda _size: _console_output_args(
                config.microvm, config.portb_snapshot, config.mem
            ),
        ),
    ]
    nanvix_command = config.nanvix_command
    if nanvix_command is not None:
        _nanvix_args(nanvix_command, config.sizes[0])
        transports.append(
            (
                "nanvix-ikc",
                False,
                lambda size: _nanvix_args(nanvix_command, size),
            )
        )
    nanvix_wxc = config.nanvix_wxc
    if nanvix_wxc is not None:
        transports.append(
            (
                "nanvix-ikc",
                False,
                lambda size: _nanvix_wxc_args(nanvix_wxc, size, config.timeout),
            )
        )

    print(
        "console output benchmark: restored workload to process completion, "
        f"median of {config.runs} runs"
    )
    print("  transport       bytes   latency p50   effective throughput   peak RSS p50")
    for label, sends_request, args_for_size in transports:
        for size in config.sizes:
            request, _, _, _ = build_mxc_output_request(size)
            args = args_for_size(size)
            require_order_marker = (
                label == "virtio-console"
                and size != 64 * 1024
                and size >= len(OUTPUT_ORDER_MARKER)
            )
            measurements = [
                _measure_console_output(
                    args,
                    size,
                    timeout=config.timeout,
                    runner=runner,
                    clock=clock,
                    request=request if sends_request else None,
                    require_order_marker=require_order_marker,
                )
                for _ in range(config.runs)
            ]
            median = statistics.median(elapsed for elapsed, _peak_rss in measurements)
            peak_rss = format_rss_median(
                [rss for _elapsed, rss in measurements], details=False
            )
            throughput = size / median / (1024 * 1024) if median > 0 else float("inf")
            print(
                f"  {label:<14} {size:>7}   {median * 1000:>9.3f} ms   "
                f"{throughput:>9.3f} MiB/s   {peak_rss:>12}"
            )


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
    result = _run_nvx_capture(args, input_text=script, timeout=300)
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
        + 'while [ "$(cat /mnt/host/host-visible 2>/dev/null)" != host-to-guest ] && '
        + '[ "$tries" -lt 1200 ]; do sleep 0.05; tries=$((tries + 1)); done\n'
        + '[ "$(cat /mnt/host/host-visible 2>/dev/null)" = host-to-guest ] && '
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
        exchange_rss: list[int | None] = []
        verified = 0
        for run_number in range(1, config.runs + 1):
            elapsed_ms, peak_rss = _run_live_exchange(
                config, backend, seed, exchange_script, run_number
            )
            exchange_times.append(elapsed_ms)
            exchange_rss.append(peak_rss)
            verified += 1
        print(
            f"  live exchange (cold each)         : {format_median(exchange_times, width=7)}"
        )
        print(
            f"  nvx peak RSS                      : {format_rss_median(exchange_rss)}"
        )
        print(f"  bidirectional visibility verified : {verified}/{config.runs} runs")


def _run_live_exchange(
    config: VirtfsConfig,
    backend: HostBackend,
    directory: Path,
    script: str,
    run_number: int,
) -> tuple[float, int | None]:
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
    return elapsed_ms, result.peak_rss_bytes


def _run_io_benchmark(
    label: str,
    mount_args: Sequence[str | Path],
    config: VirtfsConfig,
    backend: HostBackend,
    script: str,
) -> None:
    write_rates: list[float] = []
    read_rates: list[float] = []
    peak_rss: list[int | None] = []
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
        peak_rss.append(result.peak_rss_bytes)
    print(f"  {label:<27} write {format_rate_median(write_rates)}")
    print(f"  {'':<27} read  {format_rate_median(read_rates)}")
    print(f"  nvx peak RSS                      : {format_rss_median(peak_rss)}")


def _run_timed(
    args: Sequence[str | Path], timeout: int, *, graceful_timeout: bool = False
) -> tuple[float | None, float, CommandResult]:
    started = time.perf_counter()
    result = _run_nvx_capture(args, timeout=timeout, graceful_timeout=graceful_timeout)
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
        cold_results = [_run_timed(cold_args, 40) for _ in range(config.runs)]
        cold_samples = [metric for metric, _, _ in cold_results if metric is not None]
        cold_rss = [
            result.peak_rss_bytes
            for metric, _, result in cold_results
            if metric is not None
        ]
        require_samples(cold_samples, "network cold boot", config.runs)
        print(f"  cold  (guest start -> marker): {format_median(cold_samples)}")
        print(f"  cold nvx peak RSS            : {format_rss_median(cold_rss)}")

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
        restore_rss = [
            result.peak_rss_bytes
            for metric, _, result in restore_results
            if metric is not None
        ]
        require_samples(restore, "network snapshot restore", config.runs)
        require_samples(wall, "network snapshot wall clock", config.runs)
        print(f"  restore (guest resume -> marker): {format_median(restore)}")
        print(f"  restore nvx peak RSS            : {format_rss_median(restore_rss)}")
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
            with urllib.request.urlopen(
                f"http://127.0.0.1:{port}/", timeout=2
            ) as response:
                if b"HELLO-HOST" not in response.read():
                    raise ScriptError("host helper server returned unexpected content")
            yield default_route_ipv4()
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


def benchmark_network_python(config: NetworkPythonConfig, backend: HostBackend) -> None:
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
                f"{QUIET_CMDLINE} nvx_mode=app pyapp={app} "
                f"netbench_cold=1 netbench_port={config.port}",
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
        cold_rss: list[int | None] = []
        for run_number in range(1, config.runs + 1):
            metric, _, result = _run_timed(cold_args, 60)
            if metric is None:
                raise ScriptError(
                    f"cold run {run_number}/{config.runs} for {app} did not reach "
                    f"'{marker}'\n{_failure_tail(result.text)}"
                )
            cold.append(metric)
            cold_rss.append(result.peak_rss_bytes)
        print(f"  cold    (guest start  -> marker): {format_median(cold)}")
        print(f"  cold nvx peak RSS               : {format_rss_median(cold_rss)}")

        capture_args = cold_boot_args(
            executable,
            VmConfig(
                config.kernel,
                config.initrd,
                config.mem,
                1,
                f"{QUIET_CMDLINE} nvx_mode=app pyapp={app} netbench_port={config.port}",
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
        restored_rss: list[int | None] = []
        for run_number in range(1, config.runs + 1):
            metric, _, result = _run_timed(restore_args, 40)
            if metric is None:
                raise ScriptError(
                    f"restore run {run_number}/{config.runs} for {app} did not reach "
                    f"'{marker}'\n{_failure_tail(result.text)}"
                )
            restored.append(metric)
            restored_rss.append(result.peak_rss_bytes)
        print(f"  restore (guest resume -> marker): {format_median(restored)}")
        print(f"  restore nvx peak RSS            : {format_rss_median(restored_rss)}")

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
