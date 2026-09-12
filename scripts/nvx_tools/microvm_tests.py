"""Correctness tests for NVX Linux guests running on OpenVMM microVMs."""

from __future__ import annotations

import argparse
import os
import queue
import socket
import tempfile
import threading
import time
import uuid
from pathlib import Path

from .benchmark import (
    BOOT_MARKER,
    RESTORE_MARKER,
    SMP_PROBE_COMPLETION_MARKER,
    capture_snapshot,
    measure_once,
    positive_float,
    positive_int,
    run_guest_script,
    smp_probe_script,
    snapshot_restore_command,
    whp_stable_clocksource_wait_script,
    workload_boot_command,
)
from .ci import OPENVMM_TEST_BACKENDS, validate_openvmm_test_backend
from .common import (
    BUILD_DIR,
    artifact_path,
    openvmm_binary_path,
    require_file,
    sha256_file,
)
from .openvmm_process import OpenvmmProcess, TcpConsole

MICROVM_TEST_SCENARIOS = (
    "console-snapshot",
    "endpoint-policy-snapshot",
    "filesystem-snapshot",
    "lifecycle",
    "network-snapshot",
    "restore-memory",
    "restore-processors",
    "sandbox-blocks",
    "scratch-snapshot",
    "smp",
    "smp-lapic",
    "smp-snapshot",
    "snapshot-core",
    "snapshot-tiers",
    "virtio-net",
)
MICROVM_PROCESSOR_COUNTS = (1, 2, 4, 8)
MICROVM_TEST_SCRIPTS_DIR = Path(__file__).with_name("microvm_test_scripts")
LIFECYCLE_COMPLETION_MARKER = b"NVX-LIFECYCLE-OK"
SANDBOX_BLOCKS_COMPLETION_MARKER = b"NVX-SANDBOX-BLOCKS-OK"
VIRTIO_NET_COMPLETION_MARKER = b"NVX-VIRTIO-NET-OK"
SANDBOX_BLOCK_SIZE = 8 * 1024 * 1024
SNAPSHOT_CORE_CONTINUED_MARKER = b"NVX-SNAPSHOT-CORE-CONTINUED"
SNAPSHOT_CORE_COMPLETION_MARKER = b"NVX-SNAPSHOT-CORE-OK"
CONSOLE_BINARY_MARKER = b"\0\r\n\x7f\xffNVX-CONSOLE-BINARY"
CONSOLE_RX_READY_MARKER = b"NVX-CONSOLE-RX-READY"
CONSOLE_RX_QUEUED_MARKER = b"NVX-CONSOLE-RX-QUEUED"
CONSOLE_RX_RESTORED_MARKER = b"NVX-CONSOLE-RX-RESTORED"
CONSOLE_TX_DONE_MARKER = b"NVX-CONSOLE-TX-DONE"
ENDPOINT_POLICY = ("10.0.0.9:8443", "192.0.2.7:443", "10.0.0.9:443")
ENDPOINT_POLICY_BEFORE_MARKER = b"NVX-ENDPOINT-POLICY-BEFORE"
ENDPOINT_POLICY_AFTER_MARKER = b"NVX-ENDPOINT-POLICY-AFTER"
FILESYSTEM_READ_ONLY_MARKER = b"NVX-FILESYSTEM-READ-ONLY-OK"
FILESYSTEM_DORMANT_BEFORE_MARKER = b"NVX-FILESYSTEM-DORMANT-BEFORE"
FILESYSTEM_DORMANT_ATTACHED_MARKER = b"NVX-FILESYSTEM-DORMANT-ATTACHED"
FILESYSTEM_LIVE_BEFORE_MARKER = b"NVX-FILESYSTEM-LIVE-BEFORE"
FILESYSTEM_LIVE_AFTER_MARKER = b"NVX-FILESYSTEM-LIVE-AFTER"
NETWORK_BEFORE_MARKER = b"NVX-NETWORK-BEFORE"
NETWORK_INVALIDATED_MARKER = b"NVX-NETWORK-OLD-FLOW-INVALIDATED"
NETWORK_AFTER_MARKER = b"NVX-NETWORK-AFTER"
SCRATCH_PAIRED_POST_MARKER = b"NVX-SCRATCH-PAIRED-POST-OUT"
SCRATCH_PAIRED_RESTORED_MARKER = b"NVX-SCRATCH-PAIRED-RESTORED"
SCRATCH_FRESH_POST_MARKER = b"NVX-SCRATCH-FRESH-POST-OUT"


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Run NVX-owned Linux and device correctness tests against the public "
        "OpenVMM microVM CLI."
    )
    parser.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        required=True,
    )
    parser.add_argument(
        "--scenario",
        action="append",
        choices=MICROVM_TEST_SCENARIOS,
        help="scenario to run; repeat to select multiple (default: all)",
    )
    parser.add_argument(
        "--processors",
        type=int,
        choices=MICROVM_PROCESSOR_COUNTS,
        nargs="+",
        default=list(MICROVM_PROCESSOR_COUNTS),
        metavar="COUNT",
        help="processor counts for SMP and restore tests (default: 1 2 4 8)",
    )
    parser.add_argument("--memory-mib", type=positive_int, default=128)
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=60.0,
        help="seconds allowed for each scenario phase (default: 60)",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=BUILD_DIR / "test-results" / "microvm",
        help="directory for complete per-scenario OpenVMM logs",
    )
    parser.set_defaults(handler=run)


def _read_script(name: str) -> str:
    script = (MICROVM_TEST_SCRIPTS_DIR / name).read_text(encoding="utf-8")
    if not script.endswith("\n"):
        raise ValueError(f"microVM test script must end with a newline: {name}")
    return script


def _render_script(name: str, **values: str) -> str:
    script = _read_script(name)
    for key, value in values.items():
        script = script.replace(f"@{key}@", value)
    return script


def _stage_script(
    process: OpenvmmProcess,
    path: str,
    delimiter: str,
    script: str,
) -> None:
    process.send_bytes(
        f"cat >{path} <<'{delimiter}'\n".encode()
        + script.encode()
        + f"{delimiter}\nsh {path}\n".encode()
    )


def _snapshot_core_script(backend: str) -> str:
    if backend == "kvm":
        select_clocksource = (
            "clock_tries=0\n"
            "while ! grep -qw kvm-clock "
            "/sys/devices/system/clocksource/clocksource0/available_clocksource "
            "&& [ $clock_tries -lt 100 ]; do\n"
            "    sleep 0.05\n"
            "    clock_tries=$((clock_tries + 1))\n"
            "done\n"
            "grep -qw kvm-clock "
            "/sys/devices/system/clocksource/clocksource0/available_clocksource "
            "|| fail 46\n"
            "echo kvm-clock >"
            "/sys/devices/system/clocksource/clocksource0/current_clocksource"
        )
        validate_clocksource = (
            '[ "$(cat /sys/devices/system/clocksource/clocksource0/'
            'current_clocksource)" = kvm-clock ] || fail 46'
        )
    elif backend == "whp":
        select_clocksource = whp_stable_clocksource_wait_script()
        validate_clocksource = (
            '[ "$(cat /sys/devices/system/clocksource/clocksource0/'
            'current_clocksource)" != tsc-early ] || fail 46'
        )
    elif backend == "mshv":
        select_clocksource = ":"
        validate_clocksource = ":"
    else:
        raise ValueError(f"unsupported snapshot-core backend {backend!r}")
    return (
        _read_script("snapshot-core.sh.in")
        .replace("@SELECT_CLOCKSOURCE@", select_clocksource)
        .replace("@VALIDATE_CLOCKSOURCE@", validate_clocksource)
    )


def _output_lines(output: bytes) -> list[bytes]:
    return [line.removesuffix(b"\r") for line in output.splitlines()]


def _count_line_suffix(output: bytes, marker: bytes) -> int:
    return sum(line.endswith(marker) for line in _output_lines(output))


def _single_marker_value(output: bytes, prefix: bytes) -> bytes:
    values = [
        line[len(prefix) :] for line in _output_lines(output) if line.startswith(prefix)
    ]
    if len(values) != 1:
        raise RuntimeError(
            f"expected exactly one {prefix!r} marker, found {len(values)}"
        )
    return values[0]


def _parse_marker_pair(output: bytes, prefix: bytes) -> tuple[int, int]:
    value = _single_marker_value(output, prefix)
    parts = value.split(b"-")
    if len(parts) != 2:
        raise RuntimeError(f"malformed {prefix!r} marker: {value!r}")
    try:
        return int(parts[0]), int(parts[1])
    except ValueError as error:
        raise RuntimeError(f"malformed {prefix!r} marker: {value!r}") from error


def _available_tcp_address() -> tuple[str, int]:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        host, port = listener.getsockname()
        return str(host), int(port)


def _persist_console_log(
    console: TcpConsole | None,
    output: bytes,
    log_path: Path,
) -> bytes:
    if console is not None:
        output = console.finish()
    log_path.write_bytes(output)
    return output


def _send_console_rx_and_wait_until_queued(
    console: TcpConsole,
    data: bytes,
    timeout: float,
) -> None:
    console.send_bytes(data)
    console.wait_for_line(CONSOLE_RX_QUEUED_MARKER, timeout)


def _console_snapshot_script(backend: str) -> tuple[str, int, bytes]:
    if backend == "mshv":
        tx_count = 100
        receive = (
            'IFS= read -r console_rx\n[ "$console_rx" = NVX-CONSOLE-RX ] || fail 53'
        )
        restored_marker = ""
        completion = "wait $tx_pid\nnvx-exit 0"
        queued_rx = b"NVX-CONSOLE-RX\n"
    else:
        tx_count = 1_000 if backend == "whp" else 10_000
        receive = (
            "console_rx=$(dd bs=1 count=5 2>/dev/null | od -An -tx1 | "
            "tr -d ' \\n')\n"
            '[ "$console_rx" = 0001027fff ] || fail 53'
        )
        restored_marker = "echo NVX-CONSOLE-RX-RESTORED"
        completion = "wait $tx_pid\necho NVX-CONSOLE-TX-DONE\nnvx-exit 0"
        queued_rx = bytes((0, 1, 2, 127, 255))
    script = (
        _read_script("console-snapshot.sh.in")
        .replace("@TX_COUNT@", str(tx_count))
        .replace("@RX_COUNT@", str(len(queued_rx)))
        .replace("@RECEIVE@", receive)
        .replace("@RESTORED_MARKER@", restored_marker)
        .replace("@COMPLETION@", completion)
    )
    return script, tx_count, queued_rx


def _append_endpoint_policy(command: list[str], endpoints: tuple[str, ...]) -> None:
    for endpoint in endpoints:
        command.extend(("--allow-endpoint", endpoint))


def _snapshot_fingerprint(snapshot_path: Path) -> tuple[str, str, str]:
    return (
        sha256_file(
            require_file(snapshot_path / "manifest.bin", "snapshot manifest.bin")
        ),
        sha256_file(require_file(snapshot_path / "state.bin", "snapshot state.bin")),
        sha256_file(require_file(snapshot_path / "memory.bin", "snapshot memory.bin")),
    )


def _scratch_snapshot_fingerprint(
    snapshot_path: Path,
) -> tuple[str, str, str, str | None]:
    base = _snapshot_fingerprint(snapshot_path)
    scratch = snapshot_path / "scratch.img"
    return (*base, sha256_file(scratch) if scratch.is_file() else None)


def _write_pattern(path: Path, size: int, value: int) -> None:
    path.write_bytes(bytes((value,)) * size)


def _block_arg(role: str, path: Path, *, read_only: bool) -> str:
    return f"{role}:file:{path}{',ro' if read_only else ''}"


def _restore_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    return environment


def run_lifecycle(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    log_path: Path,
) -> None:
    command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0",
    )
    run_guest_script(
        command,
        _read_script("lifecycle.sh"),
        LIFECYCLE_COMPLETION_MARKER,
        timeout=timeout,
        log_path=log_path,
    )


def run_smp(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    processors: int,
    *,
    memory_mib: int,
    timeout: float,
    log_path: Path,
    force_lapic_timer: bool = False,
) -> None:
    command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0" + (" lapic=notscdeadline" if force_lapic_timer else ""),
        processors=processors,
    )
    run_guest_script(
        command,
        smp_probe_script(processors),
        SMP_PROBE_COMPLETION_MARKER,
        timeout=timeout,
        log_path=log_path,
    )


def run_virtio_net(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    log_path: Path,
) -> None:
    command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0",
        network="10.0.0.2/24",
    )
    _append_endpoint_policy(command, ENDPOINT_POLICY)
    run_guest_script(
        command,
        _read_script("virtio-net.sh"),
        VIRTIO_NET_COMPLETION_MARKER,
        timeout=timeout,
        log_path=log_path,
    )


def run_sandbox_blocks(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    log_path: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-sandbox-blocks-") as temporary:
        root = Path(temporary)
        command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
        )
        for role in ("distro", "runtime", "custom", "scratch"):
            path = root / f"{role}.img"
            with path.open("wb") as disk:
                disk.truncate(SANDBOX_BLOCK_SIZE)
            read_only = ",ro" if role != "scratch" else ""
            command.extend(
                ("--microvm-sandbox-block", f"{role}:file:{path}{read_only}")
            )
        run_guest_script(
            command,
            _read_script("sandbox-blocks.sh"),
            SANDBOX_BLOCKS_COMPLETION_MARKER,
            timeout=timeout,
            log_path=log_path,
        )


def run_smp_snapshot(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    processors = 2
    with tempfile.TemporaryDirectory(prefix="nvx-smp-snapshot-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        boot_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            processors=processors,
        )
        capture_snapshot(
            [*boot_command, "--snapshot-destination", str(snapshot_path)],
            snapshot_path,
            backend=backend,
            timeout=timeout,
            processors=processors,
            post_restore_script=smp_probe_script(processors, exit_guest=False),
            log_path=output_dir / "smp-snapshot-capture.log",
        )
        fingerprint = _snapshot_fingerprint(snapshot_path)
        for restore_index in range(2):
            measure_once(
                snapshot_restore_command(
                    executable,
                    backend,
                    snapshot_path,
                    processors=processors,
                ),
                environment=_restore_environment(),
                timeout=timeout,
                marker=RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                log_path=output_dir / f"smp-snapshot-restore-{restore_index}.log",
            )
            if _snapshot_fingerprint(snapshot_path) != fingerprint:
                raise RuntimeError(
                    f"SMP restore {restore_index} modified snapshot artifacts"
                )


def run_restore_processors(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    processor_counts: list[int],
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    capacity = 8
    with tempfile.TemporaryDirectory(prefix="nvx-restore-processors-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        boot_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0 maxcpus=1",
            processors=capacity,
        )
        capture_snapshot(
            [*boot_command, "--snapshot-destination", str(snapshot_path)],
            snapshot_path,
            backend=backend,
            timeout=timeout,
            processors=1,
            post_restore_script=_read_script("restore-processors.sh"),
            log_path=output_dir / "restore-processors-capture.log",
        )
        fingerprint = _snapshot_fingerprint(snapshot_path)
        for target in dict.fromkeys(processor_counts):
            marker = f"NVX-RESTORE-PROCESSORS-OK count={target}".encode()
            measure_once(
                snapshot_restore_command(
                    executable,
                    backend,
                    snapshot_path,
                    processors=capacity,
                    restore_processors=target,
                ),
                environment=_restore_environment(),
                timeout=timeout,
                marker=marker,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                log_path=output_dir / f"restore-processors-{target}.log",
            )
            if _snapshot_fingerprint(snapshot_path) != fingerprint:
                raise RuntimeError(
                    f"restore target {target} modified snapshot artifacts"
                )


def run_restore_memory(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    timeout: float,
    output_dir: Path,
) -> None:
    base_mib = 512
    capacity_mib = 2048
    targets_mib = (base_mib, 1024, capacity_mib)
    with tempfile.TemporaryDirectory(prefix="nvx-restore-memory-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        boot_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            base_mib,
            "quiet loglevel=0",
        )
        capture_snapshot(
            [
                *boot_command,
                "--memory-capacity",
                f"{capacity_mib}M",
                "--snapshot-destination",
                str(snapshot_path),
            ],
            snapshot_path,
            backend=backend,
            timeout=timeout,
            processors=1,
            post_restore_script=_read_script("restore-memory.sh"),
            log_path=output_dir / "restore-memory-capture.log",
        )
        fingerprint = _snapshot_fingerprint(snapshot_path)
        memory_path = require_file(snapshot_path / "memory.bin", "snapshot memory.bin")
        if memory_path.stat().st_size != base_mib * 1024 * 1024:
            raise RuntimeError(
                "memory expansion snapshot does not retain exact base RAM"
            )

        for target_mib in targets_mib:
            log_path = output_dir / f"restore-memory-{target_mib}.log"
            measure_once(
                snapshot_restore_command(
                    executable,
                    backend,
                    snapshot_path,
                    restore_memory_mib=target_mib,
                ),
                environment=_restore_environment(),
                timeout=timeout,
                marker=b"NVX-RESTORE-MEMORY-WORKLOAD-OK",
                guest_exit_prequeued=True,
                log_path=log_path,
            )
            expected_added = (target_mib - base_mib) * 1024 * 1024
            output = log_path.read_bytes()
            marker = f"NVX-MEMORY-ONLINE-OK: added_bytes={expected_added} ".encode()
            if marker not in output:
                raise RuntimeError(
                    f"restore target {target_mib} MiB did not online the expected memory"
                )
            if _snapshot_fingerprint(snapshot_path) != fingerprint:
                raise RuntimeError(
                    f"restore memory target {target_mib} modified snapshot artifacts"
                )


def run_snapshot_core(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    no_destination_command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0",
    )
    no_destination_marker = b"NVX-SNAPSHOT-NO-DESTINATION-OK"
    with OpenvmmProcess(
        no_destination_command,
        output_dir / "snapshot-core-no-destination.log",
    ) as process:
        process.wait_for(BOOT_MARKER, timeout)
        process.send_line("nvx-snapshot; echo NVX-SNAPSHOT-NO-DESTINATION-OK")
        process.wait_for(no_destination_marker, timeout)
        process.send_line("nvx-exit 0")
        result = process.wait(timeout)
    if result.returncode != 0:
        raise RuntimeError(f"no-destination OpenVMM exited with {result.returncode}")
    if _output_lines(result.output).count(no_destination_marker) != 1:
        raise RuntimeError(
            "snapshot request without a destination did not continue once"
        )

    with tempfile.TemporaryDirectory(prefix="nvx-snapshot-core-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        capture_command = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
            ),
            "--snapshot-destination",
            str(snapshot_path),
        ]
        with OpenvmmProcess(
            capture_command,
            output_dir / "snapshot-core-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-snapshot-core",
                "NVX_SNAPSHOT_CORE",
                _snapshot_core_script(backend),
            )
            source = process.wait(timeout)
        if source.returncode != 0:
            raise RuntimeError(
                f"snapshot source OpenVMM exited with {source.returncode}"
            )
        if SNAPSHOT_CORE_CONTINUED_MARKER in _output_lines(source.output):
            raise RuntimeError("snapshot source crossed its terminal capture boundary")
        fingerprint = _snapshot_fingerprint(snapshot_path)
        capture_wall_time = time.time()
        rng_hashes: list[bytes] = []
        generation_ids: list[bytes] = []
        uuids: list[bytes] = []
        temp_ids: list[bytes] = []

        for restore_index in range(2):
            time.sleep(5)
            minimum_downtime_centiseconds = int((time.time() - capture_wall_time) * 100)
            with OpenvmmProcess(
                snapshot_restore_command(executable, backend, snapshot_path),
                output_dir / f"snapshot-core-restore-{restore_index}.log",
            ) as process:
                process.wait_for(SNAPSHOT_CORE_COMPLETION_MARKER, timeout)
                restored = process.wait(timeout)
            if restored.returncode != 0:
                raise RuntimeError(
                    f"snapshot restore {restore_index} exited with {restored.returncode}"
                )
            lines = _output_lines(restored.output)
            if lines.count(SNAPSHOT_CORE_CONTINUED_MARKER) != 1:
                raise RuntimeError(
                    f"snapshot restore {restore_index} did not continue exactly once"
                )
            timer_wait = int(
                _single_marker_value(restored.output, b"NVX-SNAPSHOT-TIMER-WAIT-")
            )
            if timer_wait > 1:
                raise RuntimeError(
                    f"snapshot restore {restore_index} waited {timer_wait}s for an expired timer"
                )
            wall_delta, uptime_delta = _parse_marker_pair(
                restored.output,
                b"NVX-SNAPSHOT-DOWNTIME-",
            )
            if max(wall_delta, uptime_delta) < 4 or abs(wall_delta - uptime_delta) > 1:
                raise RuntimeError(
                    f"snapshot restore {restore_index} clock mismatch: "
                    f"wall={wall_delta}s uptime={uptime_delta}s"
                )
            uptime_centiseconds = int(
                _single_marker_value(restored.output, b"NVX-SNAPSHOT-UPTIME-CS-")
            )
            if uptime_centiseconds + 5 < minimum_downtime_centiseconds:
                raise RuntimeError(
                    f"snapshot restore {restore_index} discarded captured uptime: "
                    f"guest={uptime_centiseconds}cs host={minimum_downtime_centiseconds}cs"
                )
            process_cpu, thread_cpu = _parse_marker_pair(
                restored.output,
                b"NVX-SNAPSHOT-CPU-",
            )
            if process_cpu != 0 or thread_cpu != 0:
                raise RuntimeError(
                    f"snapshot restore {restore_index} advanced sleeping-task CPU clocks"
                )
            rng_hash = _single_marker_value(restored.output, b"NVX-SNAPSHOT-RNG-")
            if len(rng_hash) != 64 or not all(
                byte in b"0123456789abcdefABCDEF" for byte in rng_hash
            ):
                raise RuntimeError(
                    f"snapshot restore {restore_index} emitted a malformed RNG digest"
                )
            rng_hashes.append(rng_hash)
            generation_id = _single_marker_value(
                restored.output, b"NVX-SNAPSHOT-GENERATION-ID-"
            )
            if len(generation_id) != 32 or not all(
                byte in b"0123456789abcdefABCDEF" for byte in generation_id
            ):
                raise RuntimeError(
                    f"snapshot restore {restore_index} emitted a malformed generation ID"
                )
            generation_ids.append(generation_id)
            restored_uuid = _single_marker_value(restored.output, b"NVX-SNAPSHOT-UUID-")
            try:
                uuid.UUID(restored_uuid.decode("ascii"))
            except (UnicodeDecodeError, ValueError) as error:
                raise RuntimeError(
                    f"snapshot restore {restore_index} emitted a malformed UUID"
                ) from error
            uuids.append(restored_uuid)
            temp_id = _single_marker_value(restored.output, b"NVX-SNAPSHOT-TEMP-ID-")
            if not temp_id or any(byte in b" \t\r\n/" for byte in temp_id):
                raise RuntimeError(
                    f"snapshot restore {restore_index} emitted a malformed temp ID"
                )
            temp_ids.append(temp_id)
            if _snapshot_fingerprint(snapshot_path) != fingerprint:
                raise RuntimeError(
                    f"snapshot restore {restore_index} modified snapshot artifacts"
                )
        if rng_hashes[0] == rng_hashes[1]:
            raise RuntimeError(
                "fresh restore entropy did not diversify guest RNG output"
            )
        if generation_ids[0] == generation_ids[1]:
            raise RuntimeError("restored clones reused the VM generation ID")
        if uuids[0] == uuids[1]:
            raise RuntimeError("restored clones reused a kernel UUID")
        if temp_ids[0] == temp_ids[1]:
            raise RuntimeError("restored clones reused a temporary-file ID")


def run_console_snapshot(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    address = _available_tcp_address()
    script, tx_count, queued_rx = _console_snapshot_script(backend)
    with tempfile.TemporaryDirectory(prefix="nvx-console-snapshot-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        capture_command = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
            ),
            "--snapshot-destination",
            str(snapshot_path),
            "--virtio-console",
            f"listen=tcp:{address[0]}:{address[1]}",
        ]
        capture_console_log = output_dir / "console-snapshot-capture-console.log"
        source_console = b""
        console = None
        with OpenvmmProcess(
            capture_command,
            output_dir / "console-snapshot-capture-process.log",
        ) as process:
            try:
                console = TcpConsole.connect(address, timeout)
                console.wait_for(BOOT_MARKER, timeout)
                console.wait_for(b"/ # ", timeout)
                console.send_bytes(
                    b"cat >/tmp/nvx-console-snapshot <<'NVX_CONSOLE_SNAPSHOT'\n"
                    + script.encode()
                    + b"NVX_CONSOLE_SNAPSHOT\nsh /tmp/nvx-console-snapshot\n"
                )
                console.wait_for_line(CONSOLE_RX_READY_MARKER, timeout)
                _send_console_rx_and_wait_until_queued(console, queued_rx, timeout)
                process.send_bytes(b"\x01")
                source = process.wait(timeout)
                source_console = console.finish()
                console = None
            finally:
                source_console = _persist_console_log(
                    console,
                    source_console,
                    capture_console_log,
                )
        if source.returncode != 0:
            raise RuntimeError(
                f"console snapshot source exited with {source.returncode}"
            )
        for index in range(100):
            marker = f"NVX-CONSOLE-TX-{index:05}".encode()
            if _count_line_suffix(source_console, marker) != 1:
                raise RuntimeError(
                    f"console source did not emit TX record {index} exactly once"
                )
        if _count_line_suffix(source_console, b"NVX-CONSOLE-TX-00100") != 0:
            raise RuntimeError(
                "console source crossed the deterministic snapshot boundary"
            )
        fingerprint = _snapshot_fingerprint(snapshot_path)

        for restore_index in range(2):
            restore_console_log = (
                output_dir / f"console-snapshot-restore-{restore_index}-console.log"
            )
            restored_console = b""
            console = None
            with OpenvmmProcess(
                snapshot_restore_command(executable, backend, snapshot_path),
                output_dir / f"console-snapshot-restore-{restore_index}-process.log",
            ) as process:
                try:
                    console = TcpConsole.connect(address, timeout)
                    if backend != "mshv":
                        console.wait_for_line(CONSOLE_RX_RESTORED_MARKER, timeout)
                        console.wait_for_line(CONSOLE_TX_DONE_MARKER, timeout)
                    restored = process.wait(timeout)
                    restored_console = console.finish()
                    console = None
                finally:
                    restored_console = _persist_console_log(
                        console,
                        restored_console,
                        restore_console_log,
                    )
            if restored.returncode != 0:
                raise RuntimeError(
                    f"console restore {restore_index} exited with {restored.returncode}"
                )
            combined = source_console + restored_console
            if CONSOLE_BINARY_MARKER not in combined:
                raise RuntimeError(
                    f"console restore {restore_index} lost the binary TX marker"
                )
            for index in range(tx_count):
                marker = f"NVX-CONSOLE-TX-{index:05}".encode()
                if _count_line_suffix(combined, marker) != 1:
                    raise RuntimeError(
                        f"console restore {restore_index} lost or duplicated TX record {index}"
                    )
            if backend != "mshv" and (
                _output_lines(restored_console).count(CONSOLE_RX_RESTORED_MARKER) != 1
            ):
                raise RuntimeError(
                    f"console restore {restore_index} did not consume queued binary RX once"
                )
            if _snapshot_fingerprint(snapshot_path) != fingerprint:
                raise RuntimeError(
                    f"console restore {restore_index} modified snapshot artifacts"
                )


def run_endpoint_policy_snapshot(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-endpoint-policy-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        capture_command = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                network="10.0.0.2/24",
            ),
            "--snapshot-destination",
            str(snapshot_path),
        ]
        _append_endpoint_policy(capture_command, ENDPOINT_POLICY)
        with OpenvmmProcess(
            capture_command,
            output_dir / "endpoint-policy-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-endpoint-policy",
                "NVX_ENDPOINT_POLICY",
                _read_script("endpoint-policy-snapshot.sh"),
            )
            source = process.wait(timeout)
        if source.returncode != 0:
            raise RuntimeError(
                f"endpoint-policy capture source exited with {source.returncode}"
            )
        source_lines = _output_lines(source.output)
        if source_lines.count(ENDPOINT_POLICY_BEFORE_MARKER) != 1:
            raise RuntimeError("endpoint-policy source did not reach capture once")
        if ENDPOINT_POLICY_AFTER_MARKER in source_lines:
            raise RuntimeError("endpoint-policy source crossed the capture boundary")
        fingerprint = _snapshot_fingerprint(snapshot_path)

        missing_policy_command = snapshot_restore_command(
            executable,
            backend,
            snapshot_path,
            network_profile="portable",
        )
        with OpenvmmProcess(
            missing_policy_command,
            output_dir / "endpoint-policy-missing.log",
        ) as process:
            missing_policy = process.wait(timeout)
        if missing_policy.returncode == 0 or (
            b"restore-time egress policy does not match" not in missing_policy.output
        ):
            raise RuntimeError(
                "endpoint-policy restore without policy was not rejected"
            )

        changed_policy_command = snapshot_restore_command(
            executable,
            backend,
            snapshot_path,
            network_profile="portable",
        )
        _append_endpoint_policy(
            changed_policy_command,
            ("10.0.0.9:9443", "192.0.2.7:443", "10.0.0.9:443"),
        )
        with OpenvmmProcess(
            changed_policy_command,
            output_dir / "endpoint-policy-changed.log",
        ) as process:
            changed_policy = process.wait(timeout)
        if changed_policy.returncode == 0 or (
            b"restore-time egress policy does not match" not in changed_policy.output
        ):
            raise RuntimeError(
                "endpoint-policy restore with a changed port was not rejected"
            )

        for restore_index in range(2):
            restore_command = snapshot_restore_command(
                executable,
                backend,
                snapshot_path,
                network_profile="portable",
            )
            _append_endpoint_policy(restore_command, tuple(reversed(ENDPOINT_POLICY)))
            with OpenvmmProcess(
                restore_command,
                output_dir / f"endpoint-policy-restore-{restore_index}.log",
            ) as process:
                process.wait_for(ENDPOINT_POLICY_AFTER_MARKER, timeout)
                restored = process.wait(timeout)
            if restored.returncode != 0:
                raise RuntimeError(
                    f"endpoint-policy restore {restore_index} exited with "
                    f"{restored.returncode}"
                )
            if _output_lines(restored.output).count(ENDPOINT_POLICY_AFTER_MARKER) != 1:
                raise RuntimeError(
                    f"endpoint-policy restore {restore_index} did not re-resolve once"
                )
            if _snapshot_fingerprint(snapshot_path) != fingerprint:
                raise RuntimeError(
                    f"endpoint-policy restore {restore_index} modified snapshot artifacts"
                )


def run_network_snapshot(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    tcp_listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    tcp_listener.bind(("0.0.0.0", 0))
    tcp_listener.listen(3)
    tcp_listener.settimeout(timeout)
    http_port = int(tcp_listener.getsockname()[1])
    udp_socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp_socket.bind(("127.0.0.1", 0))
    udp_socket.settimeout(timeout)
    udp_port = int(udp_socket.getsockname()[1])
    events: queue.Queue[str] = queue.Queue()
    server_errors: list[Exception] = []

    def serve() -> None:
        try:
            connection, _ = tcp_listener.accept()
            with connection:
                connection.settimeout(timeout)
                request = connection.recv(4096)
                if not request.startswith(b"GET /hold HTTP/1."):
                    raise RuntimeError(f"unexpected held HTTP request: {request!r}")
                events.put("held")
                try:
                    replay = connection.recv(4096)
                except TimeoutError as error:
                    raise RuntimeError(
                        "pre-capture HTTP flow remained live after capture"
                    ) from error
                if replay:
                    raise RuntimeError("pre-capture HTTP request was replayed")
                events.put("old-flow-closed")
            for _ in range(2):
                connection, _ = tcp_listener.accept()
                with connection:
                    connection.settimeout(timeout)
                    request = connection.recv(4096)
                    if not request.startswith(b"GET /fresh HTTP/1."):
                        raise RuntimeError(
                            f"unexpected fresh HTTP request: {request!r}"
                        )
                    connection.sendall(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 19\r\n"
                        b"Connection: close\r\n\r\nNVX-NETWORK-HTTP-OK"
                    )
                    events.put("fresh")
        except Exception as error:
            server_errors.append(error)

    server = threading.Thread(target=serve, name="nvx-network-test", daemon=True)
    server.start()
    try:
        with tempfile.TemporaryDirectory(prefix="nvx-network-snapshot-") as temporary:
            snapshot_path = Path(temporary) / "snapshot"
            capture_command = [
                *workload_boot_command(
                    executable,
                    backend,
                    kernel,
                    initrd,
                    memory_mib,
                    "quiet loglevel=0",
                    network="10.0.0.2/24",
                ),
                "--snapshot-destination",
                str(snapshot_path),
                "--allow-host",
                "10.0.0.1",
            ]
            with OpenvmmProcess(
                capture_command,
                output_dir / "network-snapshot-capture.log",
            ) as process:
                process.wait_for(BOOT_MARKER, timeout)
                _stage_script(
                    process,
                    "/tmp/nvx-network-snapshot",
                    "NVX_NETWORK_SNAPSHOT",
                    _render_script(
                        "network-snapshot.sh.in",
                        HTTP_PORT=str(http_port),
                        UDP_PORT=str(udp_port),
                    ),
                )
                source = process.wait(timeout)
            if source.returncode != 0:
                raise RuntimeError(
                    f"network capture source exited with {source.returncode}"
                )
            source_lines = _output_lines(source.output)
            if source_lines.count(NETWORK_BEFORE_MARKER) != 1:
                raise RuntimeError("network source did not reach capture exactly once")
            if NETWORK_AFTER_MARKER in source_lines:
                raise RuntimeError("network source crossed the capture boundary")
            if events.get(timeout=timeout) != "held":
                raise RuntimeError("network server observed an unexpected first event")
            if events.get(timeout=timeout) != "old-flow-closed":
                raise RuntimeError("network server did not observe held-flow closure")
            udp_payload, _ = udp_socket.recvfrom(64)
            if udp_payload != b"NVX-NETWORK-UDP":
                raise RuntimeError(f"unexpected guest UDP payload: {udp_payload!r}")
            fingerprint = _snapshot_fingerprint(snapshot_path)

            missing_policy_command = snapshot_restore_command(
                executable,
                backend,
                snapshot_path,
                network_profile="portable",
            )
            with OpenvmmProcess(
                missing_policy_command,
                output_dir / "network-snapshot-missing-policy.log",
            ) as process:
                missing_policy = process.wait(timeout)
            if missing_policy.returncode == 0 or (
                b"restore-time egress policy does not match"
                not in missing_policy.output
            ):
                raise RuntimeError("network restore without policy was not rejected")

            for restore_index in range(2):
                restore_command = snapshot_restore_command(
                    executable,
                    backend,
                    snapshot_path,
                    network_profile="portable",
                )
                restore_command.extend(("--allow-host", "10.0.0.1"))
                with OpenvmmProcess(
                    restore_command,
                    output_dir / f"network-snapshot-restore-{restore_index}.log",
                ) as process:
                    process.wait_for(NETWORK_INVALIDATED_MARKER, timeout)
                    process.wait_for(NETWORK_AFTER_MARKER, timeout)
                    restored = process.wait(timeout)
                if restored.returncode != 0:
                    raise RuntimeError(
                        f"network restore {restore_index} exited with "
                        f"{restored.returncode}"
                    )
                restored_lines = _output_lines(restored.output)
                if restored_lines.count(NETWORK_INVALIDATED_MARKER) != 1 or (
                    restored_lines.count(NETWORK_AFTER_MARKER) != 1
                ):
                    raise RuntimeError(
                        f"network restore {restore_index} did not invalidate and refresh"
                    )
                if events.get(timeout=timeout) != "fresh":
                    raise RuntimeError(
                        f"network restore {restore_index} did not open a fresh flow"
                    )
                if _snapshot_fingerprint(snapshot_path) != fingerprint:
                    raise RuntimeError(
                        f"network restore {restore_index} modified snapshot artifacts"
                    )
    finally:
        tcp_listener.close()
        udp_socket.close()
        server.join(timeout=1)
    if server.is_alive():
        raise RuntimeError("network test server did not stop")
    if server_errors:
        raise RuntimeError(f"network test server failed: {server_errors[0]}")


def _expect_process_failure(
    command: list[str],
    log_path: Path,
    timeout: float,
    expected: bytes | None = None,
    forbidden_markers: tuple[bytes, ...] = (),
) -> None:
    with OpenvmmProcess(command, log_path) as process:
        result = process.wait(timeout)
    if result.returncode == 0:
        raise RuntimeError(f"OpenVMM unexpectedly accepted invalid restore: {command}")
    if expected is not None and expected not in result.output:
        raise RuntimeError(f"OpenVMM failure did not contain {expected!r}")
    lines = _output_lines(result.output)
    if any(marker in lines for marker in forbidden_markers):
        raise RuntimeError("invalid restore entered the guest before failing")


def run_filesystem_snapshot(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-filesystem-snapshot-") as temporary:
        root = Path(temporary)

        read_only_root = root / "read-only"
        read_only_root.mkdir()
        (read_only_root / "seed").write_bytes(b"NVX-FILESYSTEM-READ-ONLY")
        run_guest_script(
            workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                mount=f"/mnt/share,{read_only_root},ro",
            ),
            _read_script("filesystem-read-only.sh"),
            FILESYSTEM_READ_ONLY_MARKER,
            timeout=timeout,
            log_path=output_dir / "filesystem-read-only.log",
        )
        if (read_only_root / "mutation").exists():
            raise RuntimeError("read-only virtio-fs mount accepted a host mutation")

        dormant_snapshot = root / "dormant-snapshot"
        dormant_capture = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
            ),
            "--snapshot-destination",
            str(dormant_snapshot),
        ]
        with OpenvmmProcess(
            dormant_capture,
            output_dir / "filesystem-dormant-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-filesystem-dormant",
                "NVX_FILESYSTEM_DORMANT",
                _read_script("filesystem-dormant.sh"),
            )
            dormant_source = process.wait(timeout)
        if dormant_source.returncode != 0 or (
            _output_lines(dormant_source.output).count(FILESYSTEM_DORMANT_BEFORE_MARKER)
            != 1
        ):
            raise RuntimeError("dormant filesystem snapshot capture failed")
        dormant_fingerprint = _snapshot_fingerprint(dormant_snapshot)

        with OpenvmmProcess(
            snapshot_restore_command(executable, backend, dormant_snapshot),
            output_dir / "filesystem-dormant-no-mount.log",
        ) as process:
            process.send_line("echo NVX-FILESYSTEM-DORMANT-NO-MOUNT; nvx-exit 0")
            process.wait_for(b"NVX-FILESYSTEM-DORMANT-NO-MOUNT", timeout)
            dormant_detached = process.wait(timeout)
        if dormant_detached.returncode != 0:
            raise RuntimeError("dormant filesystem slot did not restore detached")

        late_root = root / "late-root"
        late_root.mkdir()
        (late_root / "host-seed").write_bytes(b"NVX-HOST-TO-GUEST")
        attached_restore = snapshot_restore_command(
            executable, backend, dormant_snapshot
        )
        attached_restore.extend(("--mount", f"/mnt/late,{late_root},rw"))
        with OpenvmmProcess(
            attached_restore,
            output_dir / "filesystem-dormant-attached.log",
        ) as process:
            process.send_line(
                "set -eu; mkdir -p /mnt/late; "
                "mount -t virtiofs microvm /mnt/late; "
                '[ "$(cat /mnt/late/host-seed)" = NVX-HOST-TO-GUEST ]; '
                "printf NVX-GUEST-TO-HOST >/mnt/late/guest-result; "
                "echo NVX-FILESYSTEM-DORMANT-ATTACHED; nvx-exit 0"
            )
            process.wait_for(FILESYSTEM_DORMANT_ATTACHED_MARKER, timeout)
            dormant_attached = process.wait(timeout)
        if dormant_attached.returncode != 0 or (
            (late_root / "guest-result").read_bytes() != b"NVX-GUEST-TO-HOST"
        ):
            raise RuntimeError("dormant filesystem late attachment failed")
        if _snapshot_fingerprint(dormant_snapshot) != dormant_fingerprint:
            raise RuntimeError(
                "dormant filesystem restores modified snapshot artifacts"
            )

        live_root = root / "live-root"
        live_root.mkdir()
        open_handle = live_root / "open-handle"
        open_handle.write_bytes(b"")
        live_snapshot = root / "live-snapshot"
        live_capture = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                mount=f"/mnt/share,{live_root},rw",
            ),
            "--snapshot-destination",
            str(live_snapshot),
        ]
        with OpenvmmProcess(
            live_capture,
            output_dir / "filesystem-live-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-filesystem-live",
                "NVX_FILESYSTEM_LIVE",
                _read_script("filesystem-live.sh"),
            )
            live_source = process.wait(timeout)
        live_source_lines = _output_lines(live_source.output)
        if live_source.returncode != 0 or (
            live_source_lines.count(FILESYSTEM_LIVE_BEFORE_MARKER) != 1
        ):
            raise RuntimeError("live filesystem snapshot capture failed")
        if FILESYSTEM_LIVE_AFTER_MARKER in live_source_lines:
            raise RuntimeError("live filesystem source crossed the capture boundary")
        if open_handle.read_bytes() != b"NVX-HANDLE-BEFORE":
            raise RuntimeError("live filesystem source completed a post-capture write")
        live_fingerprint = _snapshot_fingerprint(live_snapshot)

        _expect_process_failure(
            snapshot_restore_command(executable, backend, live_snapshot),
            output_dir / "filesystem-live-missing-mount.log",
            timeout,
            b"requires a fresh --mount attachment",
        )

        saved_root = root / "saved-live-root"
        live_root.rename(saved_root)
        live_root.mkdir()
        try:
            replacement_command = snapshot_restore_command(
                executable, backend, live_snapshot
            )
            replacement_command.extend(("--mount", f"/mnt/share,{live_root},rw"))
            _expect_process_failure(
                replacement_command,
                output_dir / "filesystem-live-replacement-root.log",
                timeout,
                b"root identity does not match",
            )
        finally:
            live_root.rmdir()
            saved_root.rename(live_root)

        moved_root = root / "moved-live-root"
        live_root.rename(moved_root)
        try:
            moved_command = snapshot_restore_command(executable, backend, live_snapshot)
            moved_command.extend(("--mount", f"/mnt/share,{moved_root},rw"))
            _expect_process_failure(
                moved_command,
                output_dir / "filesystem-live-moved-root.log",
                timeout,
                b"canonical host path does not match",
            )
        finally:
            moved_root.rename(live_root)

        saved_handle = live_root / "saved-open-handle"
        open_handle.rename(saved_handle)
        open_handle.write_bytes(b"replacement")
        try:
            replaced_command = snapshot_restore_command(
                executable, backend, live_snapshot
            )
            replaced_command.extend(("--mount", f"/mnt/share,{live_root},rw"))
            _expect_process_failure(
                replaced_command,
                output_dir / "filesystem-live-replaced-object.log",
                timeout,
            )
        finally:
            open_handle.unlink()
            saved_handle.rename(open_handle)

        for restore_index in range(2):
            restore_command = snapshot_restore_command(
                executable, backend, live_snapshot
            )
            restore_command.extend(("--mount", f"/mnt/share,{live_root},rw"))
            with OpenvmmProcess(
                restore_command,
                output_dir / f"filesystem-live-restore-{restore_index}.log",
            ) as process:
                process.wait_for(FILESYSTEM_LIVE_AFTER_MARKER, timeout)
                restored = process.wait(timeout)
            if restored.returncode != 0:
                raise RuntimeError(
                    f"live filesystem restore {restore_index} exited with "
                    f"{restored.returncode}"
                )
            if _snapshot_fingerprint(live_snapshot) != live_fingerprint:
                raise RuntimeError(
                    f"live filesystem restore {restore_index} modified snapshot artifacts"
                )
        if open_handle.read_bytes() != b"NVX-HANDLE-BEFORENVX-HANDLE-AFTER":
            raise RuntimeError(
                "live filesystem handle did not resume at its captured offset"
            )


def run_scratch_snapshot(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-scratch-snapshot-") as temporary:
        root = Path(temporary)
        layer = root / "distro.erofs"
        wrong_layer = root / "wrong-distro.erofs"
        source_scratch = root / "source-scratch.raw"
        _write_pattern(layer, 1024 * 1024, 0x3C)
        _write_pattern(wrong_layer, 1024 * 1024, 0xC3)
        _write_pattern(source_scratch, 8 * 1024 * 1024, 0xA5)

        paired_snapshot = root / "paired-snapshot"
        paired_capture = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
            ),
            "--snapshot-destination",
            str(paired_snapshot),
            "--snapshot-tier",
            "workload-start",
            "--microvm-sandbox-block",
            _block_arg("distro", layer, read_only=True),
            "--microvm-sandbox-block",
            f"scratch:delay:250:file:{source_scratch}",
        ]
        with OpenvmmProcess(
            paired_capture,
            output_dir / "scratch-paired-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-scratch-paired",
                "NVX_SCRATCH_PAIRED",
                _read_script("scratch-paired.sh"),
            )
            paired_source = process.wait(timeout)
        if paired_source.returncode != 0:
            raise RuntimeError(
                f"paired scratch capture exited with {paired_source.returncode}"
            )
        paired_source_lines = _output_lines(paired_source.output)
        if SCRATCH_PAIRED_POST_MARKER in paired_source_lines or (
            SCRATCH_PAIRED_RESTORED_MARKER in paired_source_lines
        ):
            raise RuntimeError("paired scratch source crossed its capture boundary")
        published_scratch = require_file(
            paired_snapshot / "scratch.img",
            "paired scratch snapshot artifact",
        )
        paired_fingerprint = _scratch_snapshot_fingerprint(paired_snapshot)

        def paired_restore_command(selected_layer: Path) -> list[str]:
            command = snapshot_restore_command(executable, backend, paired_snapshot)
            command.extend(
                (
                    "--microvm-sandbox-block",
                    _block_arg("distro", selected_layer, read_only=True),
                )
            )
            return command

        for restore_index in range(2):
            with OpenvmmProcess(
                paired_restore_command(layer),
                output_dir / f"scratch-paired-restore-{restore_index}.log",
            ) as process:
                process.wait_for(SCRATCH_PAIRED_RESTORED_MARKER, timeout)
                restored = process.wait(timeout)
            restored_lines = _output_lines(restored.output)
            if restored.returncode != 0 or (
                restored_lines.count(SCRATCH_PAIRED_POST_MARKER) != 1
                or restored_lines.count(SCRATCH_PAIRED_RESTORED_MARKER) != 1
            ):
                raise RuntimeError(
                    f"paired scratch restore {restore_index} was not coherent"
                )
            if _scratch_snapshot_fingerprint(paired_snapshot) != paired_fingerprint:
                raise RuntimeError(
                    f"paired scratch restore {restore_index} modified snapshot artifacts"
                )

        forbidden_paired = (
            SCRATCH_PAIRED_POST_MARKER,
            SCRATCH_PAIRED_RESTORED_MARKER,
        )
        _expect_process_failure(
            paired_restore_command(wrong_layer),
            output_dir / "scratch-paired-wrong-layer.log",
            timeout,
            forbidden_markers=forbidden_paired,
        )
        scratch_bytes = published_scratch.read_bytes()
        published_scratch.unlink()
        try:
            _expect_process_failure(
                paired_restore_command(layer),
                output_dir / "scratch-paired-missing.log",
                timeout,
                forbidden_markers=forbidden_paired,
            )
        finally:
            published_scratch.write_bytes(scratch_bytes)
        corrupt = bytearray(scratch_bytes)
        corrupt[0] ^= 0xFF
        published_scratch.write_bytes(corrupt)
        try:
            _expect_process_failure(
                paired_restore_command(layer),
                output_dir / "scratch-paired-corrupt.log",
                timeout,
                forbidden_markers=forbidden_paired,
            )
        finally:
            published_scratch.write_bytes(scratch_bytes)
        with published_scratch.open("r+b") as scratch_file:
            scratch_file.truncate(len(scratch_bytes) - 512)
        try:
            _expect_process_failure(
                paired_restore_command(layer),
                output_dir / "scratch-paired-truncated.log",
                timeout,
                forbidden_markers=forbidden_paired,
            )
        finally:
            published_scratch.write_bytes(scratch_bytes)

        capture_scratch = root / "capture-scratch.raw"
        _write_pattern(capture_scratch, 1024 * 1024, 0xA5)
        fresh_snapshot = root / "fresh-snapshot"
        fresh_capture = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "",
            ),
            "--snapshot-destination",
            str(fresh_snapshot),
            "--snapshot-tier",
            "platform",
            "--microvm-sandbox-block",
            _block_arg("distro", layer, read_only=True),
            "--microvm-sandbox-block",
            _block_arg("scratch", capture_scratch, read_only=False),
        ]
        with OpenvmmProcess(
            fresh_capture,
            output_dir / "scratch-fresh-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-scratch-fresh",
                "NVX_SCRATCH_FRESH",
                _read_script("scratch-fresh.sh"),
            )
            fresh_source = process.wait(timeout)
        fresh_source_lines = _output_lines(fresh_source.output)
        if (
            fresh_source.returncode != 0
            or SCRATCH_FRESH_POST_MARKER in fresh_source_lines
        ):
            raise RuntimeError("fresh scratch source crossed its capture boundary")
        if (fresh_snapshot / "scratch.img").exists():
            raise RuntimeError("fresh scratch snapshot published paired state")
        fresh_fingerprint = _scratch_snapshot_fingerprint(fresh_snapshot)

        def fresh_restore_command(scratch: Path | None) -> list[str]:
            command = snapshot_restore_command(executable, backend, fresh_snapshot)
            command.extend(
                (
                    "--microvm-sandbox-block",
                    _block_arg("distro", layer, read_only=True),
                )
            )
            if scratch is not None:
                command.extend(
                    (
                        "--microvm-sandbox-block",
                        _block_arg("scratch", scratch, read_only=False),
                    )
                )
            return command

        for restore_index, value in enumerate((17, 34)):
            scratch = root / f"fresh-scratch-{restore_index}.raw"
            _write_pattern(scratch, 1024 * 1024, value)
            marker = f"NVX-SCRATCH-FRESH-VALUE-{value}".encode()
            with OpenvmmProcess(
                fresh_restore_command(scratch),
                output_dir / f"scratch-fresh-restore-{restore_index}.log",
            ) as process:
                process.wait_for(marker, timeout)
                restored = process.wait(timeout)
            restored_lines = _output_lines(restored.output)
            if restored.returncode != 0 or (
                restored_lines.count(SCRATCH_FRESH_POST_MARKER) != 1
                or restored_lines.count(marker) != 1
            ):
                raise RuntimeError(
                    f"fresh scratch restore {restore_index} used the wrong backing"
                )
            if _scratch_snapshot_fingerprint(fresh_snapshot) != fresh_fingerprint:
                raise RuntimeError(
                    f"fresh scratch restore {restore_index} modified snapshot artifacts"
                )

        _expect_process_failure(
            fresh_restore_command(None),
            output_dir / "scratch-fresh-missing.log",
            timeout,
            forbidden_markers=(SCRATCH_FRESH_POST_MARKER,),
        )
        wrong_geometry = root / "wrong-geometry.raw"
        _write_pattern(wrong_geometry, 512 * 1024, 0)
        _expect_process_failure(
            fresh_restore_command(wrong_geometry),
            output_dir / "scratch-fresh-wrong-geometry.log",
            timeout,
            forbidden_markers=(SCRATCH_FRESH_POST_MARKER,),
        )


def _snapshot_tier_script(tier: str) -> str:
    platform = tier == "platform"
    workload_start = tier == "workload-start"
    instance_checkpoint = tier == "instance-checkpoint"
    if not (platform or workload_start or instance_checkpoint):
        raise ValueError(f"unsupported snapshot tier {tier!r}")

    prefix = f"NVX-TIER-{tier.upper()}"
    workload_marker = f"{prefix}-WORKLOAD-RAN"
    paired_setup = ""
    if not platform:
        paired_setup = f"""mkdir -p /run/nvx/scratch /sys/fs/cgroup
mountpoint -q /sys/fs/cgroup || mount -t cgroup2 none /sys/fs/cgroup
mkdir -p /sys/fs/cgroup/container
mkfs.ext4 -F /dev/vdb >/dev/null
mount -t ext4 /dev/vdb /run/nvx/scratch
printf 'captured-workload-id\\n' >/run/nvx/workload-machine-id
barrier=/run/nvx/test-container-start
mkfifo "$barrier"
(IFS= read -r start <"$barrier"
[ "$start" = start ]
exec unshare --mount --uts --fork --kill-child sh -c 'mount --make-rprivate /; mkdir -p /etc; : >/etc/machine-id; mount --bind /run/nvx/workload-machine-id /etc/machine-id; hostname captured-workload; while [ ! -e /run/nvx/restore-active ]; do sleep 0.01; done; echo {workload_marker}; : >/run/nvx/workload-ran; while :; do sleep 60; done') &
workload_pid=$!
echo "$workload_pid" >/sys/fs/cgroup/container/cgroup.procs
printf 'start\\n' >"$barrier"
rm -f "$barrier"
echo "$workload_pid" >/run/nvx/container.pid
tries=0
while [ "$(nsenter -t "$workload_pid" -u hostname 2>/dev/null || true)" != captured-workload ] && [ "$tries" -lt 200 ]; do
    sleep 0.01
    tries=$((tries + 1))
done
[ "$tries" -lt 200 ] || {{ nvx-exit 67; exit 67; }}"""

    repair_marker = f"{prefix}-REPAIR"
    premature_marker = f"{prefix}-PREMATURE-INPUT"
    runtime_hook = ":"
    if platform:
        runtime_hook = f"""cat >/run/nvx/runtime-post-restore <<'NVX_TIER_HOOK'
#!/bin/sh
set -eu
case "${{NVX_VM_GENERATION_ID:-}}" in
    '' | *[!0-9a-f]*) exit 71 ;;
esac
[ "${{#NVX_VM_GENERATION_ID}}" -eq 32 ] || exit 71
echo {repair_marker}
sleep 1
[ "$(date -u +%Y)" -ge 2025 ]
grep -Eq '^[0-9a-f]{{32}}$' /etc/machine-id
[ "$(hostname)" = nvx-sandbox ]
if ! kill -0 "$(cat /run/nvx/gate-reader.pid)" 2>/dev/null; then
    echo {premature_marker}
    exit 1
fi
NVX_TIER_HOOK
chmod +x /run/nvx/runtime-post-restore"""
    elif workload_start:
        runtime_hook = f"""cat >/run/nvx/runtime-post-restore <<'NVX_TIER_HOOK'
#!/bin/sh
set -eu
case "${{NVX_VM_GENERATION_ID:-}}" in
    '' | *[!0-9a-f]*) exit 71 ;;
esac
[ "${{#NVX_VM_GENERATION_ID}}" -eq 32 ] || exit 71
: >/run/nvx/restore-active
echo {repair_marker}
sleep 1
grep -Eq '^[0-9a-f]{{32}}$' /etc/machine-id
[ "$(cat /run/nvx/workload-machine-id)" = "$(cat /etc/machine-id)" ]
[ "$(nsenter -t "$(cat /run/nvx/container.pid)" -m -r cat /etc/machine-id)" = "$(cat /etc/machine-id)" ]
[ "$(nsenter -t "$(cat /run/nvx/container.pid)" -u hostname)" = restored-workload-start ]
if ! kill -0 "$(cat /run/nvx/gate-reader.pid)" 2>/dev/null; then
    echo {premature_marker}
    exit 1
fi
NVX_TIER_HOOK
chmod +x /run/nvx/runtime-post-restore"""

    repair_action = ":"
    if workload_start:
        repair_action = """tries=0
while [ ! -e /run/nvx/workload-ran ] && [ "$tries" -lt 200 ]; do
    sleep 0.01
    tries=$((tries + 1))
done
[ -e /run/nvx/workload-ran ] || { nvx-exit 70; exit 70; }"""
    elif instance_checkpoint:
        repair_action = f"""[ "$(cat /etc/machine-id)" = captured-machine-id ]
[ "$(cat /run/nvx/workload-machine-id)" = captured-workload-id ]
[ "$(nsenter -t "$(cat /run/nvx/container.pid)" -m -r cat /etc/machine-id)" = captured-workload-id ]
[ "$(nsenter -t "$(cat /run/nvx/container.pid)" -u hostname)" = captured-workload ]
echo {repair_marker}"""

    pre_capture_action = (
        "date -u -s 200001010000.00 >/dev/null\n"
        "printf 'captured-machine-id\\n' >/etc/machine-id"
        if platform
        else "hostname captured-host\nprintf 'captured-machine-id\\n' >/etc/machine-id"
    )
    capture_action = (
        "/sbin/nvx-snapshot"
        if instance_checkpoint
        else f"/sbin/nvx-snapshot --tier {tier}"
    )
    return _render_script(
        "snapshot-tier.sh.in",
        MISMATCHED_TIER="workload-start" if platform else "platform",
        MISMATCH_MARKER=f"{prefix}-MISMATCH-REJECTED",
        PAIRED_SETUP=paired_setup,
        SETUP_MARKER=f"{prefix}-SETUP-READY",
        RUNTIME_HOOK=runtime_hook,
        HOOK_MARKER=f"{prefix}-HOOK-READY",
        PRE_CAPTURE_ACTION=pre_capture_action,
        INPUT_MARKER=f"{prefix}-INPUT",
        CAPTURE_MARKER=f"{prefix}-CAPTURE",
        CAPTURE_ACTION=capture_action,
        REPAIR_ACTION=repair_action,
        RELEASED_MARKER=f"{prefix}-RELEASED",
        LAYER_MARKER=f"{prefix}-LAYER-",
    )


def _run_snapshot_tier(
    tier: str,
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    platform = tier == "platform"
    workload_start = tier == "workload-start"
    instance_checkpoint = tier == "instance-checkpoint"
    if not (platform or workload_start or instance_checkpoint):
        raise ValueError(f"unsupported snapshot tier {tier!r}")

    prefix = f"NVX-TIER-{tier.upper()}"
    capture_marker = f"{prefix}-CAPTURE".encode()
    repair_marker = f"{prefix}-REPAIR".encode()
    input_marker = f"{prefix}-INPUT".encode()
    released_marker = f"{prefix}-RELEASED".encode()
    premature_marker = f"{prefix}-PREMATURE-INPUT".encode()
    mismatch_marker = f"{prefix}-MISMATCH-REJECTED".encode()
    workload_marker = f"{prefix}-WORKLOAD-RAN".encode()
    layer_marker = f"{prefix}-LAYER-".encode()

    with tempfile.TemporaryDirectory(prefix=f"nvx-snapshot-tier-{tier}-") as temporary:
        root = Path(temporary)
        snapshot = root / "snapshot"
        source_layer = root / "source.erofs"
        replacement_layer = root / "replacement.erofs"
        source_scratch = root / "source-scratch.raw"
        restore_scratch = root / "restore-scratch.raw"
        _write_pattern(source_layer, 1024 * 1024, 0x3C)
        _write_pattern(replacement_layer, 1024 * 1024, 0xC3)
        _write_pattern(source_scratch, 1024 * 1024, 0xA5)
        _write_pattern(restore_scratch, 1024 * 1024, 0x5A)
        address = _available_tcp_address()

        capture_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "" if platform else f"nvx_hostname=restored-{tier}",
            network="10.0.0.2/24",
        )
        capture_command.extend(
            (
                "--allow-host",
                "10.0.0.1",
                "--virtio-console",
                f"listen=tcp:{address[0]}:{address[1]}",
                "--snapshot-destination",
                str(snapshot),
                "--snapshot-tier",
                tier,
                "--microvm-sandbox-block",
                _block_arg("distro", source_layer, read_only=True),
                "--microvm-sandbox-block",
                _block_arg("scratch", source_scratch, read_only=False),
            )
        )

        source_console_path = output_dir / f"snapshot-tier-{tier}-capture-console.log"
        source_console = b""
        console: TcpConsole | None = None
        with OpenvmmProcess(
            capture_command,
            output_dir / f"snapshot-tier-{tier}-capture-process.log",
        ) as process:
            try:
                console = TcpConsole.connect(address, timeout)
                console.wait_for(BOOT_MARKER, timeout)
                script = _snapshot_tier_script(tier)
                console.send_bytes(
                    b"cat >/tmp/nvx-snapshot-tier <<'NVX_SNAPSHOT_TIER'\n"
                    + script.encode()
                    + b"NVX_SNAPSHOT_TIER\nsh /tmp/nvx-snapshot-tier\n"
                )
                console.wait_for(capture_marker, timeout)
                source = process.wait(timeout)
                source_console = console.finish()
                console = None
            finally:
                source_console = _persist_console_log(
                    console,
                    source_console,
                    source_console_path,
                )
        if source.returncode != 0:
            raise RuntimeError(
                f"{tier} snapshot capture exited with {source.returncode}"
            )
        source_lines = _output_lines(source_console)
        if source_lines.count(mismatch_marker) != 1 or any(
            marker in source_lines
            for marker in (repair_marker, input_marker, released_marker)
        ):
            raise RuntimeError(f"{tier} source crossed its terminal capture boundary")
        fingerprint = _snapshot_fingerprint(snapshot)

        restore_layer = replacement_layer if platform else source_layer
        restore_command = snapshot_restore_command(
            executable,
            backend,
            snapshot,
            network_profile="portable",
        )
        restore_command.extend(
            (
                "--allow-host",
                "10.0.0.1",
                "--microvm-sandbox-block",
                _block_arg("distro", restore_layer, read_only=True),
            )
        )
        if platform:
            restore_command.extend(
                (
                    "--microvm-sandbox-block",
                    _block_arg("scratch", restore_scratch, read_only=False),
                )
            )

        if instance_checkpoint:
            wrong_profile = [
                str(executable),
                "--single-process",
                "--hypervisor",
                backend,
                "--restore-snapshot",
                str(snapshot),
            ]
            _expect_process_failure(
                wrong_profile,
                output_dir / f"snapshot-tier-{tier}-wrong-profile.log",
                timeout,
                b"microVM snapshot restore requires --machine microvm",
            )
            if (snapshot / "resume.claim").exists():
                raise RuntimeError("wrong-profile restore consumed instance checkpoint")

        expected_layer = layer_marker + (b"195" if platform else b"60")
        restore_console_path = output_dir / f"snapshot-tier-{tier}-restore-console.log"
        restore_console = b""
        console = None
        with OpenvmmProcess(
            restore_command,
            output_dir / f"snapshot-tier-{tier}-restore-process.log",
        ) as process:
            try:
                console = TcpConsole.connect(address, timeout)
                console.send_bytes(b"Z")
                console.wait_for(repair_marker, timeout)
                if workload_start:
                    console.wait_for(workload_marker, timeout)
                console.wait_for(released_marker, timeout)
                console.wait_for(expected_layer, timeout)
                restored = process.wait(timeout)
                restore_console = console.finish()
                console = None
            finally:
                restore_console = _persist_console_log(
                    console,
                    restore_console,
                    restore_console_path,
                )
        if restored.returncode != 0:
            raise RuntimeError(f"{tier} restore exited with {restored.returncode}")
        restore_lines = _output_lines(restore_console)
        if restore_lines.count(input_marker) != 1 or premature_marker in restore_lines:
            raise RuntimeError(f"{tier} input crossed the restore gate")
        if restore_lines.count(expected_layer) != 1:
            raise RuntimeError(f"{tier} restore observed the wrong layer binding")
        if _snapshot_fingerprint(snapshot) != fingerprint:
            raise RuntimeError(f"{tier} restore modified snapshot payloads")

        if workload_start:
            timeout_command = [
                *restore_command,
                "--restore-gate-timeout-ms",
                "500",
            ]
            timeout_console: TcpConsole | None = None
            timeout_console_path = (
                output_dir / f"snapshot-tier-{tier}-timeout-console.log"
            )
            timeout_output = b""
            with OpenvmmProcess(
                timeout_command,
                output_dir / f"snapshot-tier-{tier}-timeout-process.log",
            ) as process:
                try:
                    timeout_console = TcpConsole.connect(address, timeout)
                    timeout_console.send_bytes(b"Z")
                    timed_out = process.wait(timeout)
                    timeout_output = timeout_console.finish()
                    timeout_console = None
                finally:
                    timeout_output = _persist_console_log(
                        timeout_console,
                        timeout_output,
                        timeout_console_path,
                    )
            timeout_lines = _output_lines(timeout_output)
            if timed_out.returncode == 0 or any(
                marker in timeout_lines
                for marker in (input_marker, released_marker, workload_marker)
            ):
                raise RuntimeError("workload-start timeout released gated guest state")

        if instance_checkpoint:
            _expect_process_failure(
                restore_command,
                output_dir / f"snapshot-tier-{tier}-duplicate.log",
                timeout,
                b"resume snapshot has already been claimed",
            )


def run_snapshot_tiers(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
    tiers: tuple[str, ...] = ("platform", "workload-start", "instance-checkpoint"),
) -> None:
    for tier in tiers:
        _run_snapshot_tier(
            tier,
            executable,
            kernel,
            initrd,
            backend,
            memory_mib=memory_mib,
            timeout=timeout,
            output_dir=output_dir,
        )


def run(args: argparse.Namespace) -> int:
    validate_openvmm_test_backend(args.backend)
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(artifact_path("vmlinux"), "microVM PVH kernel")
    initrd = require_file(
        artifact_path("initramfs.cpio.gz"),
        "microVM Alpine initramfs",
    )
    output_dir: Path = args.output_dir
    output_dir.mkdir(parents=True, exist_ok=True)
    scenarios = tuple(dict.fromkeys(args.scenario or MICROVM_TEST_SCENARIOS))

    if "console-snapshot" in scenarios:
        print(f"Running microVM console snapshot correctness on OpenVMM/{args.backend}")
        run_console_snapshot(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "endpoint-policy-snapshot" in scenarios:
        print(
            "Running microVM endpoint-policy snapshot correctness on "
            f"OpenVMM/{args.backend}"
        )
        run_endpoint_policy_snapshot(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "filesystem-snapshot" in scenarios:
        print(
            f"Running microVM filesystem snapshot correctness on OpenVMM/{args.backend}"
        )
        run_filesystem_snapshot(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "lifecycle" in scenarios:
        print(f"Running microVM lifecycle correctness on OpenVMM/{args.backend}")
        run_lifecycle(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            log_path=output_dir / "lifecycle.log",
        )
    if "network-snapshot" in scenarios:
        print(f"Running microVM network snapshot correctness on OpenVMM/{args.backend}")
        run_network_snapshot(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    for scenario, force_lapic_timer in (("smp", False), ("smp-lapic", True)):
        if scenario not in scenarios:
            continue
        for processors in dict.fromkeys(args.processors):
            print(
                f"Running microVM {scenario} correctness ({processors} vCPU) "
                f"on OpenVMM/{args.backend}"
            )
            run_smp(
                executable,
                kernel,
                initrd,
                args.backend,
                processors,
                memory_mib=args.memory_mib,
                timeout=args.timeout,
                log_path=output_dir / f"{scenario}-{processors}.log",
                force_lapic_timer=force_lapic_timer,
            )
    if "sandbox-blocks" in scenarios:
        print(f"Running microVM sandbox-block correctness on OpenVMM/{args.backend}")
        run_sandbox_blocks(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            log_path=output_dir / "sandbox-blocks.log",
        )
    if "scratch-snapshot" in scenarios:
        print(f"Running microVM scratch snapshot correctness on OpenVMM/{args.backend}")
        run_scratch_snapshot(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "smp-snapshot" in scenarios:
        print(f"Running microVM SMP snapshot correctness on OpenVMM/{args.backend}")
        run_smp_snapshot(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "restore-processors" in scenarios:
        print(
            f"Running microVM restore-processor correctness on OpenVMM/{args.backend}"
        )
        run_restore_processors(
            executable,
            kernel,
            initrd,
            args.backend,
            args.processors,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "restore-memory" in scenarios:
        print(f"Running microVM restore-memory correctness on OpenVMM/{args.backend}")
        run_restore_memory(
            executable,
            kernel,
            initrd,
            args.backend,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "snapshot-core" in scenarios:
        print(f"Running microVM snapshot-core correctness on OpenVMM/{args.backend}")
        run_snapshot_core(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "snapshot-tiers" in scenarios:
        print(f"Running microVM snapshot-tier correctness on OpenVMM/{args.backend}")
        run_snapshot_tiers(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "virtio-net" in scenarios:
        print(f"Running microVM virtio-net correctness on OpenVMM/{args.backend}")
        run_virtio_net(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            log_path=output_dir / "virtio-net.log",
        )

    print(f"Wrote microVM correctness logs to {output_dir}")
    return 0
