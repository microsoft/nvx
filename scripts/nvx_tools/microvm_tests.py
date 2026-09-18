"""Correctness tests for NVX Linux guests running on OpenVMM microVMs."""

from __future__ import annotations

import argparse
import json
import os
import queue
import secrets
import socket
import subprocess
import tempfile
import threading
import time
import uuid
from collections.abc import Sequence
from pathlib import Path
from typing import Any, cast

from .benchmark import (
    RESTORE_MARKER,
    SMP_PROBE_COMPLETION_MARKER,
    GuestCommandResult,
    measure_once,
    positive_float,
    positive_int,
    record_adversarial_openvmm_pid,
    smp_probe_script,
    snapshot_restore_command,
    whp_stable_clocksource_wait_script,
    workload_boot_command,
)
from .benchmark import (
    capture_snapshot as _capture_snapshot,
)
from .benchmark import (
    run_guest_script as _run_guest_script,
)
from .ci import OPENVMM_TEST_BACKENDS, validate_openvmm_test_backend
from .common import (
    BUILD_DIR,
    ScriptError,
    artifact_path,
    openvmm_binary_path,
    require_file,
    sha256_file,
)
from .control_session import ControlSession
from .guests import GUEST_NAMES, GuestDescriptor, guest_descriptor
from .openvmm_process import OpenvmmProcess, TcpConsole

MICROVM_TEST_SCENARIOS = (
    "console-exit",
    "console-snapshot",
    "directional-network-policy",
    "denied-filesystem-paths",
    "endpoint-policy-snapshot",
    "filesystem-snapshot",
    "guest-boot",
    "guest-identity",
    "host-loopback-policy",
    "lifecycle",
    "l3-l4-egress-policy",
    "managed-lifecycle",
    "network-snapshot",
    "restore-memory",
    "restore-processors",
    "restore-tsc-sync",
    "sandbox-blocks",
    "scratch-snapshot",
    "smp",
    "smp-lapic",
    "smp-snapshot",
    "snapshot-core",
    "snapshot-tiers",
    "structured-outcome",
    "virtio-net",
    "workload-identity",
)
UBUNTU_UNSUPPORTED_SCENARIOS = frozenset(("sandbox-blocks", "scratch-snapshot"))
MICROVM_PROCESSOR_COUNTS = (1, 2, 4, 8)
MICROVM_TEST_SCRIPTS_DIR = Path(__file__).with_name("microvm_test_scripts")
LIFECYCLE_COMPLETION_MARKER = b"NVX-LIFECYCLE-OK"
SANDBOX_BLOCKS_COMPLETION_MARKER = b"NVX-SANDBOX-BLOCKS-OK"
VIRTIO_NET_COMPLETION_MARKER = b"NVX-VIRTIO-NET-OK"
DIRECTIONAL_NETWORK_ALLOW_MARKER = b"NVX-DIRECTIONAL-NETWORK-ALLOW-OK"
DIRECTIONAL_NETWORK_DENY_MARKER = b"NVX-DIRECTIONAL-NETWORK-DENY-OK"
DIRECTIONAL_NETWORK_INGRESS_READY_MARKER = b"NVX-DIRECTIONAL-INGRESS-READY"
DIRECTIONAL_NETWORK_GUEST_IPV4 = "192.0.2.2"
DIRECTIONAL_NETWORK_GATEWAY_IPV4 = "192.0.2.1"
DIRECTIONAL_NETWORK_CIDR = f"{DIRECTIONAL_NETWORK_GUEST_IPV4}/24"
DIRECTIONAL_NETWORK_INGRESS_PORT = 18080
L3_L4_EGRESS_COMPLETION_MARKER = b"NVX-L3-L4-EGRESS-OK"
HOST_LOOPBACK_DENY_MARKER = b"NVX-HOST-LOOPBACK-DENY-OK"
HOST_LOOPBACK_UDP_CONTROL_MARKER = b"NVX-HOST-LOOPBACK-UDP-CONTROL-OK"
HOST_LOOPBACK_ALLOW_MARKER = b"NVX-HOST-LOOPBACK-ALLOW-OK"
HOST_LOOPBACK_INGRESS_READY_MARKER = b"NVX-HOST-LOOPBACK-INGRESS-READY"
SANDBOX_BLOCK_SIZE = 8 * 1024 * 1024
SNAPSHOT_CORE_CONTINUED_MARKER = b"NVX-SNAPSHOT-CORE-CONTINUED"
SNAPSHOT_CORE_COMPLETION_MARKER = b"NVX-SNAPSHOT-CORE-OK"
CONSOLE_BINARY_MARKER = b"\0\r\n\x7f\xffNVX-CONSOLE-BINARY"
CONSOLE_RX_READY_MARKER = b"NVX-CONSOLE-RX-READY"
CONSOLE_RX_QUEUED_MARKER = b"NVX-CONSOLE-RX-QUEUED"
CONSOLE_RX_RESTORED_MARKER = b"NVX-CONSOLE-RX-RESTORED"
CONSOLE_TX_DONE_MARKER = b"NVX-CONSOLE-TX-DONE"
CONSOLE_EXIT_COMPLETION_MARKER = b"NVX-CONSOLE-EXIT-OK"
CONSOLE_EXIT_PAYLOAD_BYTES = 64 * 1024
CONSOLE_EXIT_READ_DELAY_SECONDS = 2.0
ENDPOINT_POLICY = ("10.0.0.9:8443", "192.0.2.7:443", "10.0.0.9:443")
ENDPOINT_POLICY_BEFORE_MARKER = b"NVX-ENDPOINT-POLICY-BEFORE"
ENDPOINT_POLICY_AFTER_MARKER = b"NVX-ENDPOINT-POLICY-AFTER"
FILESYSTEM_READ_ONLY_MARKER = b"NVX-FILESYSTEM-READ-ONLY-OK"
FILESYSTEM_DENIED_MARKER = b"NVX-DENIED-PATHS-OK"
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
WORKLOAD_IDENTITY_MARKER = b"NVX-WORKLOAD-IDENTITY-OK uid=65534 gid=65534"
BOOT_MARKER = b"NVX-GUEST-BOOT-OK:"
GUEST_BOOT_COMPLETION_MARKER = b"NVX-GUEST-BOOT-CHECK-OK"
GUEST_IDENTITY_COMPLETION_MARKER = b"NVX-GUEST-IDENTITY-OK"
OUTCOME_TOP_LEVEL_FIELDS = frozenset(
    {
        "schema_version",
        "instance_id",
        "backend",
        "outcome",
        "network_policy",
        "teardown",
    }
)
OUTCOME_TEARDOWN_FIELDS = frozenset(
    {
        "guest_workload_stopped",
        "vm_stopped",
        "openvmm_process_terminated",
        "virtiofs_released",
        "network_released",
        "temporary_storage_removed",
        "control_channels_closed",
    }
)


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
    parser.add_argument("--guest", choices=GUEST_NAMES, default="alpine")
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
    parser.add_argument(
        "--memory-mib",
        type=positive_int,
        help="guest RAM; defaults to the selected guest profile",
    )
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


def run_guest_script(
    command: Sequence[str],
    script: str,
    completion_marker: bytes,
    *,
    timeout: float,
    windows_cpus: set[int] | None = None,
    teardown_mode: str = "guest-exit",
    log_path: Path | None = None,
) -> GuestCommandResult:
    return _run_guest_script(
        command,
        script,
        completion_marker,
        timeout=timeout,
        windows_cpus=windows_cpus,
        teardown_mode=teardown_mode,
        log_path=log_path,
        boot_marker=BOOT_MARKER,
    )


def capture_snapshot(
    command: Sequence[str],
    snapshot_path: Path,
    *,
    backend: str,
    timeout: float,
    windows_cpus: set[int] | None = None,
    processors: int | None = None,
    teardown_mode: str = "guest-exit",
    smp_network_gateway: str | None = None,
    smp_ioapic_irq: int | None = None,
    snapshot_profile: bool = False,
    profile_sink: list[dict[str, object]] | None = None,
    post_restore_script: str | None = None,
    log_path: Path | None = None,
) -> tuple[float, float, float, int]:
    return _capture_snapshot(
        command,
        snapshot_path,
        backend=backend,
        timeout=timeout,
        windows_cpus=windows_cpus,
        processors=processors,
        teardown_mode=teardown_mode,
        smp_network_gateway=smp_network_gateway,
        smp_ioapic_irq=smp_ioapic_irq,
        snapshot_profile=snapshot_profile,
        profile_sink=profile_sink,
        post_restore_script=post_restore_script,
        log_path=log_path,
        boot_marker=BOOT_MARKER,
    )


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


def _read_outcome_report(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(
            f"failed to read structured outcome report {path}"
        ) from error
    if not isinstance(value, dict):
        raise RuntimeError("structured outcome report is not an object")
    raw = cast(dict[str, object], value)
    if set(raw) != set(OUTCOME_TOP_LEVEL_FIELDS):
        raise RuntimeError("structured outcome report has unexpected top-level fields")
    if raw["schema_version"] != 1:
        raise RuntimeError("structured outcome report has an unsupported version")
    instance_id = raw["instance_id"]
    if (
        not isinstance(instance_id, str)
        or len(instance_id) != 32
        or any(character not in "0123456789abcdef" for character in instance_id)
    ):
        raise RuntimeError("structured outcome report has an invalid instance ID")
    if raw["backend"] not in ("auto", "kvm", "mshv", "whp"):
        raise RuntimeError("structured outcome report has an invalid backend")
    outcome_value = raw["outcome"]
    policy_value = raw["network_policy"]
    teardown_value = raw["teardown"]
    if not isinstance(outcome_value, dict):
        raise RuntimeError("structured outcome report has an invalid outcome")
    outcome = cast(dict[str, object], outcome_value)
    if set(outcome) != {
        "operation",
        "category",
        "status_code",
    }:
        raise RuntimeError("structured outcome report has an invalid outcome")
    if not isinstance(policy_value, dict):
        raise RuntimeError("structured outcome report has an invalid network policy")
    policy = cast(dict[str, object], policy_value)
    if set(policy) != {
        "status",
        "status_code",
        "mode",
        "allow_rule_count",
        "deny_rule_count",
        "host_loopback",
    }:
        raise RuntimeError("structured outcome report has an invalid network policy")
    if not isinstance(teardown_value, dict):
        raise RuntimeError("structured outcome report has an invalid teardown outcome")
    teardown = cast(dict[str, object], teardown_value)
    if set(teardown) != set(OUTCOME_TEARDOWN_FIELDS):
        raise RuntimeError("structured outcome report has an invalid teardown outcome")
    if not all(isinstance(value, bool) for value in teardown.values()):
        raise RuntimeError("structured teardown outcomes must be booleans")
    return cast(dict[str, Any], raw)


def _preserve_outcome_report(path: Path, report: dict[str, Any]) -> None:
    path.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


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


def run_guest_boot(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    descriptor: GuestDescriptor,
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
        (
            f"grep -Fqx 'ID={descriptor.os_release_id}' /etc/os-release\n"
            f"grep -Fq '{descriptor.release}' /etc/os-release\n"
            "echo NVX-GUEST-BOOT-CHECK-OK\n"
            "nvx-exit 0\n"
        ),
        GUEST_BOOT_COMPLETION_MARKER,
        timeout=timeout,
        log_path=log_path,
    )


def run_guest_identity(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    descriptor: GuestDescriptor,
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
        (
            f"grep -Fqx 'ID={descriptor.os_release_id}' /etc/os-release\n"
            f"grep -Fq '{descriptor.release}' /etc/os-release\n"
            "echo NVX-GUEST-IDENTITY-OK\n"
            "nvx-exit 0\n"
        ),
        GUEST_IDENTITY_COMPLETION_MARKER,
        timeout=timeout,
        log_path=log_path,
    )


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


def run_workload_identity(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    base = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0 nvx_exec=/sbin/nvx-identity-probe",
    )
    accepted = [*base, "--microvm-workload-identity", "65534:65534"]
    with OpenvmmProcess(
        accepted,
        output_dir / "workload-identity.log",
    ) as process:
        result = process.wait(timeout)
    if result.returncode != 0 or WORKLOAD_IDENTITY_MARKER not in _output_lines(
        result.output
    ):
        raise RuntimeError("fixed non-root workload identity was not enforced")

    unavailable = [*base, "--microvm-workload-identity", "12345:12345"]
    with OpenvmmProcess(
        unavailable,
        output_dir / "workload-identity-unavailable.log",
    ) as process:
        result = process.wait(timeout)
    if (
        result.returncode == 0
        or WORKLOAD_IDENTITY_MARKER in result.output
        or b"configured workload UID is unavailable" not in result.output
    ):
        raise RuntimeError("unavailable workload identity did not fail closed")

    root = [*base, "--microvm-workload-identity", "0:0"]
    with OpenvmmProcess(
        root,
        output_dir / "workload-identity-root.log",
    ) as process:
        result = process.wait(timeout)
    if result.returncode == 0 or BOOT_MARKER in result.output:
        raise RuntimeError("root workload identity was not rejected before boot")


def run_managed_lifecycle(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-managed-lifecycle-") as temporary:
        root = Path(temporary)
        endpoint_value = (
            str(root / "control.sock"),
            f"//./pipe/openvmm-microvm-{uuid.uuid4().hex}",
        )[os.name == "nt"]
        boot_console_address = _available_tcp_address()
        capability = secrets.token_bytes(32)
        report_path = root / "managed-outcome.json"
        command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
        )
        command.extend(
            (
                "--microvm-workload-identity",
                "65534:65534",
                "--microvm-lifecycle",
                "managed",
                "--virtio-console",
                f"listen=tcp:{boot_console_address[0]}:{boot_console_address[1]}",
                "--microvm-control-console",
                f"listen={endpoint_value}",
                "--microvm-control-auth-stdin",
                "--microvm-report",
                str(report_path),
            )
        )
        log_path = output_dir / "managed-lifecycle.log"
        process: subprocess.Popen[bytes] | None = None
        boot_console: TcpConsole | None = None
        with log_path.open("wb") as log:
            try:
                environment = os.environ.copy()
                environment["OPENVMM_LOG"] = "off"
                process = subprocess.Popen(
                    command,
                    stdin=subprocess.PIPE,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    env=environment,
                )
                record_adversarial_openvmm_pid(process.pid, environment)
                if process.stdin is None:
                    raise RuntimeError("failed to create control capability pipe")
                process.stdin.write(capability)
                process.stdin.close()
                boot_console = TcpConsole.connect(boot_console_address, timeout)
                with ControlSession.connect(
                    Path(endpoint_value), capability, timeout
                ) as session:
                    session.ping(timeout)
                    first = session.exec(
                        (
                            "/bin/sh",
                            "-c",
                            "printf managed-state >/tmp/nvx-managed-state; "
                            "printf first-exec",
                        ),
                        timeout_ms=5_000,
                        response_timeout=timeout,
                    )
                if (
                    first.returncode != 0
                    or first.category != "exit"
                    or first.stdout != b"first-exec"
                    or first.stderr
                ):
                    raise RuntimeError(
                        "first managed workload returned an invalid result"
                    )

                with ControlSession.connect(
                    Path(endpoint_value), capability, timeout
                ) as session:
                    second = session.exec(
                        (
                            "/bin/sh",
                            "-c",
                            "cat /tmp/nvx-managed-state; printf second-exec",
                        ),
                        timeout_ms=5_000,
                        response_timeout=timeout,
                    )
                    timed_out = session.exec(
                        ("/bin/sleep", "5"),
                        timeout_ms=100,
                        response_timeout=timeout,
                    )
                    session.stop(timeout)
                if (
                    second.returncode != 0
                    or second.category != "exit"
                    or second.stdout != b"managed-statesecond-exec"
                    or second.stderr
                ):
                    raise RuntimeError(
                        "managed workload state did not survive across exec requests"
                    )
                if timed_out.returncode != 124 or timed_out.category != "timeout":
                    raise RuntimeError("managed workload timeout was not reported")
                result = process.wait(timeout=timeout)
                if result != 0:
                    raise RuntimeError(
                        f"managed OpenVMM process exited with status {result}"
                    )
                report = _read_outcome_report(report_path)
                _preserve_outcome_report(
                    output_dir / "managed-outcome.json",
                    report,
                )
                if report["backend"] != backend or report["outcome"] != {
                    "operation": "managed",
                    "category": "success",
                    "status_code": 0,
                }:
                    raise RuntimeError("managed lifecycle outcome report was invalid")
                if report["network_policy"]["status"] != "not-requested":
                    raise RuntimeError(
                        "managed lifecycle reported an unexpected network policy"
                    )
                if not all(report["teardown"].values()):
                    raise RuntimeError("managed lifecycle reported incomplete teardown")
            finally:
                if boot_console is not None:
                    (output_dir / "managed-lifecycle-guest.log").write_bytes(
                        boot_console.finish()
                    )
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)

        invalid = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
        )
        invalid.extend(
            (
                "--microvm-workload-identity",
                "65534:65534",
                "--microvm-lifecycle",
                "managed",
            )
        )
        with OpenvmmProcess(
            invalid,
            output_dir / "managed-lifecycle-invalid-transition.log",
        ) as rejected:
            result = rejected.wait(timeout)
        if result.returncode == 0 or BOOT_MARKER in result.output:
            raise RuntimeError(
                "managed lifecycle without a control endpoint was not rejected before boot"
            )


def run_structured_outcome(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-structured-outcome-") as temporary:
        root = Path(temporary)
        sensitive_report_name = "sensitive-report-destination.json"
        report_path = root / sensitive_report_name
        command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0 nvx_report_secret=sensitive-command-value",
            network=DIRECTIONAL_NETWORK_CIDR,
        )
        command.extend(
            (
                "--network-egress",
                "deny",
                "--network-egress-allow",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:tcp:443",
                "--network-egress-allow",
                "198.51.100.0/24",
                "--network-egress-deny",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:udp:53",
                "--host-loopback",
                "deny",
                "--network-proxy",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:54321",
                "--microvm-report",
                str(report_path),
            )
        )
        with OpenvmmProcess(
            command,
            output_dir / "structured-outcome.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            process.send_line("printf 'sensitive-output-value\\n'; /sbin/nvx-exit 37")
            result = process.wait(timeout)
        if result.returncode != 37 or b"sensitive-output-value" not in result.output:
            raise RuntimeError("structured outcome run lost the guest exit result")

        report = _read_outcome_report(report_path)
        _preserve_outcome_report(
            output_dir / "structured-outcome.json",
            report,
        )
        if report["backend"] != backend or report["outcome"] != {
            "operation": "run",
            "category": "guest-exit",
            "status_code": 37,
        }:
            raise RuntimeError("structured guest outcome was invalid")
        if report["network_policy"] != {
            "status": "applied",
            "status_code": 0,
            "mode": "rules",
            "allow_rule_count": 2,
            "deny_rule_count": 1,
            "host_loopback": "deny",
        }:
            raise RuntimeError("structured network-policy outcome was invalid")
        if not all(report["teardown"].values()):
            raise RuntimeError("structured outcome reported incomplete teardown")
        encoded = json.dumps(report, sort_keys=True)
        for forbidden in (
            "sensitive-command-value",
            "sensitive-output-value",
            sensitive_report_name,
            DIRECTIONAL_NETWORK_GATEWAY_IPV4,
            "54321",
        ):
            if forbidden in encoded:
                raise RuntimeError(
                    f"structured outcome exposed sensitive value {forbidden!r}"
                )
        rejected_path = root / "rejected.json"
        rejected = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            network=DIRECTIONAL_NETWORK_CIDR,
        )
        rejected.extend(
            (
                "--network-egress-allow",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:tcp:443",
                "--microvm-report",
                str(rejected_path),
            )
        )
        with OpenvmmProcess(
            rejected,
            output_dir / "structured-outcome-rejected.log",
        ) as process:
            rejected_result = process.wait(timeout)
        if rejected_result.returncode == 0 or BOOT_MARKER in rejected_result.output:
            raise RuntimeError(
                "structured policy rejection did not fail before guest boot"
            )
        rejected_report = _read_outcome_report(rejected_path)
        _preserve_outcome_report(
            output_dir / "structured-outcome-rejected.json",
            rejected_report,
        )
        if rejected_report["outcome"] != {
            "operation": "run",
            "category": "vmm-failure",
            "status_code": 1,
        }:
            raise RuntimeError("configuration rejection outcome was invalid")
        if rejected_report["network_policy"] != {
            "status": "failed",
            "status_code": 1,
            "mode": "rules",
            "allow_rule_count": 1,
            "deny_rule_count": 0,
            "host_loopback": "allow",
        }:
            raise RuntimeError("configuration rejection policy outcome was invalid")
        if not all(rejected_report["teardown"].values()):
            raise RuntimeError("configuration rejection leaked host resources")


def run_console_exit(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    processors: int,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    expected = (
        b"x" * CONSOLE_EXIT_PAYLOAD_BYTES
        + b"\n"
        + CONSOLE_EXIT_COMPLETION_MARKER
        + b"\n"
    )
    with tempfile.TemporaryDirectory(prefix="nvx-console-exit-") as temporary:
        for exit_code in (0, 37):
            name = f"console-exit-{processors}vcpu-status-{exit_code}"
            snapshot_path = Path(temporary) / f"snapshot-{exit_code}"
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
                post_restore_script=_render_script(
                    "console-exit.sh.in",
                    PAYLOAD_BYTES=str(CONSOLE_EXIT_PAYLOAD_BYTES),
                    COMPLETION_MARKER=CONSOLE_EXIT_COMPLETION_MARKER.decode(),
                    EXIT_CODE=str(exit_code),
                ),
                log_path=output_dir / f"{name}-capture.log",
            )
            with OpenvmmProcess(
                snapshot_restore_command(
                    executable, backend, snapshot_path, processors=processors
                ),
                output_dir / f"{name}-restore.log",
                output_read_delay=CONSOLE_EXIT_READ_DELAY_SECONDS,
            ) as process:
                result = process.wait(timeout)
            if result.returncode != exit_code:
                raise RuntimeError(
                    f"{name}: expected exit status {exit_code}, got {result.returncode}"
                )
            output = result.output.replace(b"\r\n", b"\n")
            if output != expected:
                raise RuntimeError(
                    f"{name}: truncated or corrupt console output; "
                    f"expected {len(expected)} bytes, got {len(output)}"
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


def _directional_network_command(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    memory_mib: int,
    *,
    egress: str,
    ingress: str,
) -> list[str]:
    if egress not in ("allow", "deny") or ingress not in ("allow", "deny"):
        raise ValueError("directional network actions must be 'allow' or 'deny'")
    command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0",
        network=DIRECTIONAL_NETWORK_CIDR,
    )
    command.extend(("--network-egress", egress, "--network-ingress", ingress))
    return command


def run_directional_network_policy(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("0.0.0.0", 0))
    listener.listen(2)
    listener.settimeout(timeout)
    http_port = int(listener.getsockname()[1])
    server_errors: list[Exception] = []

    def serve_allowed_request() -> None:
        try:
            connection, _ = listener.accept()
            with connection:
                connection.settimeout(timeout)
                request = connection.recv(4096)
                if not request.startswith(b"GET /directional HTTP/1."):
                    raise RuntimeError(
                        f"unexpected directional-policy HTTP request: {request!r}"
                    )
                body = b"NVX-DIRECTIONAL-RESPONSE-OK"
                connection.sendall(
                    b"HTTP/1.1 200 OK\r\nContent-Length: "
                    + str(len(body)).encode("ascii")
                    + b"\r\nConnection: close\r\n\r\n"
                    + body
                )
        except Exception as error:
            server_errors.append(error)

    server = threading.Thread(
        target=serve_allowed_request,
        name="nvx-directional-network-test",
        daemon=True,
    )
    server.start()
    try:
        allow_command = _directional_network_command(
            executable,
            kernel,
            initrd,
            backend,
            memory_mib,
            egress="allow",
            ingress="deny",
        )
        with OpenvmmProcess(
            allow_command,
            output_dir / "directional-network-allow.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-directional-network-policy",
                "NVX_DIRECTIONAL_NETWORK_POLICY",
                _render_script(
                    "directional-network-policy.sh.in",
                    EGRESS="allow",
                    GUEST_IPV4=DIRECTIONAL_NETWORK_GUEST_IPV4,
                    GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                    HTTP_PORT=str(http_port),
                    INGRESS_PORT=str(DIRECTIONAL_NETWORK_INGRESS_PORT),
                    COMPLETION_MARKER=DIRECTIONAL_NETWORK_ALLOW_MARKER.decode(),
                ),
            )
            process.wait_for(DIRECTIONAL_NETWORK_INGRESS_READY_MARKER, timeout)
            try:
                inbound = socket.create_connection(
                    (DIRECTIONAL_NETWORK_GUEST_IPV4, DIRECTIONAL_NETWORK_INGRESS_PORT),
                    timeout=min(timeout, 2.0),
                )
            except OSError:
                pass
            else:
                inbound.close()
                raise RuntimeError("host initiated a new connection toward the guest")
            process.send_line("NVX-DIRECTIONAL-INGRESS-CHECKED")
            process.wait_for(DIRECTIONAL_NETWORK_ALLOW_MARKER, timeout)
            allowed = process.wait(timeout)
        if allowed.returncode != 0:
            raise RuntimeError(
                f"directional-policy allow guest exited with {allowed.returncode}"
            )
        server.join(timeout)
        if server.is_alive():
            raise TimeoutError("directional-policy HTTP server did not finish")
        if server_errors:
            raise RuntimeError(
                "directional-policy HTTP server failed"
            ) from server_errors[0]

        deny_command = _directional_network_command(
            executable,
            kernel,
            initrd,
            backend,
            memory_mib,
            egress="deny",
            ingress="deny",
        )
        run_guest_script(
            deny_command,
            _render_script(
                "directional-network-policy.sh.in",
                EGRESS="deny",
                GUEST_IPV4=DIRECTIONAL_NETWORK_GUEST_IPV4,
                GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                HTTP_PORT=str(http_port),
                INGRESS_PORT=str(DIRECTIONAL_NETWORK_INGRESS_PORT),
                COMPLETION_MARKER=DIRECTIONAL_NETWORK_DENY_MARKER.decode(),
            ),
            DIRECTIONAL_NETWORK_DENY_MARKER,
            timeout=timeout,
            log_path=output_dir / "directional-network-deny.log",
        )
        listener.settimeout(0.25)
        try:
            unexpected, _ = listener.accept()
        except TimeoutError:
            pass
        else:
            unexpected.close()
            raise RuntimeError("deny-all egress reached the host TCP listener")

        unsupported_command = _directional_network_command(
            executable,
            kernel,
            initrd,
            backend,
            memory_mib,
            egress="allow",
            ingress="allow",
        )
        with OpenvmmProcess(
            unsupported_command,
            output_dir / "directional-network-unsupported-ingress.log",
        ) as process:
            unsupported = process.wait(timeout)
        if (
            unsupported.returncode == 0
            or b"--network-ingress allow is unsupported" not in unsupported.output
            or BOOT_MARKER in unsupported.output
        ):
            raise RuntimeError(
                "unsupported ingress policy was not rejected before boot"
            )
    finally:
        listener.close()
        server.join(timeout=1)


def run_l3_l4_egress_policy(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    allowed_tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    denied_tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    allowed_udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    denied_udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    for listener in (allowed_tcp, denied_tcp):
        listener.bind(("0.0.0.0", 0))
        listener.listen(1)
        listener.settimeout(timeout)
    for endpoint in (allowed_udp, denied_udp):
        endpoint.bind(("127.0.0.1", 0))
        endpoint.settimeout(timeout)
    allowed_tcp_port = int(allowed_tcp.getsockname()[1])
    denied_tcp_port = int(denied_tcp.getsockname()[1])
    allowed_udp_port = int(allowed_udp.getsockname()[1])
    denied_udp_port = int(denied_udp.getsockname()[1])
    server_errors: list[Exception] = []

    def serve_allowed() -> None:
        try:
            connection, _ = allowed_tcp.accept()
            with connection:
                connection.settimeout(timeout)
                request = connection.recv(4096)
                if not request.startswith(b"GET /allowed HTTP/1."):
                    raise RuntimeError(f"unexpected L3/L4 HTTP request: {request!r}")
                body = b"NVX-L3-L4-TCP-ALLOW"
                connection.sendall(
                    b"HTTP/1.1 200 OK\r\nContent-Length: "
                    + str(len(body)).encode("ascii")
                    + b"\r\nConnection: close\r\n\r\n"
                    + body
                )
            payload, _ = allowed_udp.recvfrom(128)
            if payload != b"NVX-L3-L4-UDP-ALLOW":
                raise RuntimeError(f"unexpected allowed UDP payload: {payload!r}")
        except Exception as error:
            server_errors.append(error)

    server = threading.Thread(
        target=serve_allowed,
        name="nvx-l3-l4-egress-test",
        daemon=True,
    )
    server.start()
    try:
        command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            network=DIRECTIONAL_NETWORK_CIDR,
        )
        command.extend(("--network-egress", "deny"))
        for rule in (
            f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:tcp:{allowed_tcp_port}",
            f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:udp:{allowed_udp_port}",
            f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:tcp:{denied_tcp_port}",
            f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:udp:{denied_udp_port}",
        ):
            command.extend(("--network-egress-allow", rule))
        for rule in (
            f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:tcp:{denied_tcp_port}",
            f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:udp:{denied_udp_port}",
        ):
            command.extend(("--network-egress-deny", rule))

        run_guest_script(
            command,
            _render_script(
                "l3-l4-egress-policy.sh.in",
                GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                ALLOWED_TCP_PORT=str(allowed_tcp_port),
                DENIED_TCP_PORT=str(denied_tcp_port),
                ALLOWED_UDP_PORT=str(allowed_udp_port),
                DENIED_UDP_PORT=str(denied_udp_port),
            ),
            L3_L4_EGRESS_COMPLETION_MARKER,
            timeout=timeout,
            log_path=output_dir / "l3-l4-egress-policy.log",
        )
        server.join(timeout)
        if server.is_alive():
            raise TimeoutError("L3/L4 allowed endpoints were not reached")
        if server_errors:
            raise RuntimeError(
                "L3/L4 allowed endpoint server failed"
            ) from server_errors[0]

        denied_tcp.settimeout(0.25)
        try:
            unexpected, _ = denied_tcp.accept()
        except TimeoutError:
            pass
        else:
            unexpected.close()
            raise RuntimeError("deny rule did not override the TCP allow rule")
        denied_udp.settimeout(0.25)
        try:
            unexpected, _ = denied_udp.recvfrom(128)
        except TimeoutError:
            pass
        else:
            raise RuntimeError(
                f"deny rule did not override the UDP allow rule: {unexpected!r}"
            )

        for name, extra, expected in (
            (
                "missing-default",
                ("--network-egress-allow", "192.0.2.1:tcp:443"),
                b"--network-egress is required",
            ),
            (
                "invalid-protocol",
                (
                    "--network-egress",
                    "deny",
                    "--network-egress-allow",
                    "192.0.2.1:icmp:443",
                ),
                b"invalid egress transport",
            ),
        ):
            invalid = workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                network=DIRECTIONAL_NETWORK_CIDR,
            )
            invalid.extend(extra)
            with OpenvmmProcess(
                invalid,
                output_dir / f"l3-l4-egress-{name}.log",
            ) as process:
                result = process.wait(timeout)
            if (
                result.returncode == 0
                or expected not in result.output
                or BOOT_MARKER in result.output
            ):
                raise RuntimeError(
                    f"invalid L3/L4 policy {name} was not rejected before boot"
                )
    finally:
        for endpoint in (allowed_tcp, denied_tcp, allowed_udp, denied_udp):
            endpoint.close()
        server.join(timeout=1)


def _http_server(
    listener: socket.socket,
    expected_path: bytes,
    body: bytes,
    timeout: float,
    errors: list[Exception],
) -> None:
    try:
        connection, _ = listener.accept()
        with connection:
            connection.settimeout(timeout)
            request = connection.recv(4096)
            if not request.startswith(b"GET " + expected_path + b" HTTP/1."):
                raise RuntimeError(
                    f"unexpected host-loopback HTTP request: {request!r}"
                )
            connection.sendall(
                b"HTTP/1.1 200 OK\r\nContent-Length: "
                + str(len(body)).encode("ascii")
                + b"\r\nConnection: close\r\n\r\n"
                + body
            )
    except Exception as error:
        errors.append(error)


def run_host_loopback_policy(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    denied_general = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    proxy = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    for listener in (denied_general, proxy):
        listener.bind(("0.0.0.0", 0))
        listener.listen(1)
        listener.settimeout(timeout)
    denied_general_port = int(denied_general.getsockname()[1])
    proxy_port = int(proxy.getsockname()[1])
    proxy_errors: list[Exception] = []
    proxy_server = threading.Thread(
        target=_http_server,
        args=(
            proxy,
            b"/proxy",
            b"NVX-HOST-LOOPBACK-PROXY",
            timeout,
            proxy_errors,
        ),
        name="nvx-host-loopback-proxy",
        daemon=True,
    )
    denied_udp: list[socket.socket] = []
    try:
        for port in (proxy_port, denied_general_port):
            listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            denied_udp.append(listener)
            listener.bind(("127.0.0.1", port))
            listener.settimeout(0.25)
        control_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            network=DIRECTIONAL_NETWORK_CIDR,
        )
        control_command.extend(
            ("--network-egress", "allow", "--network-ingress", "deny")
        )
        # Prove the guest sender and both host UDP observers work before testing silence.
        run_guest_script(
            control_command,
            _render_script(
                "host-loopback-policy.sh.in",
                MODE="udp-control",
                GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                GENERAL_PORT=str(denied_general_port),
                PROXY_PORT=str(proxy_port),
                GUEST_PORT="0",
            ),
            HOST_LOOPBACK_UDP_CONTROL_MARKER,
            timeout=timeout,
            log_path=output_dir / "host-loopback-udp-control.log",
        )
        for listener in denied_udp:
            try:
                payload = listener.recv(4096)
            except TimeoutError as error:
                raise RuntimeError(
                    "host-loopback UDP positive control was not observed "
                    f"on port {listener.getsockname()[1]}"
                ) from error
            if payload != b"NVX-HOST-LOOPBACK-UDP-CONTROL":
                raise RuntimeError(
                    f"unexpected host-loopback UDP positive control: {payload!r}"
                )
        proxy_server.start()
        deny_command = control_command + [
            "--host-loopback",
            "deny",
            "--network-proxy",
            f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:{proxy_port}",
        ]
        run_guest_script(
            deny_command,
            _render_script(
                "host-loopback-policy.sh.in",
                MODE="deny",
                GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                GENERAL_PORT=str(denied_general_port),
                PROXY_PORT=str(proxy_port),
                GUEST_PORT="0",
            ),
            HOST_LOOPBACK_DENY_MARKER,
            timeout=timeout,
            log_path=output_dir / "host-loopback-deny.log",
        )
        proxy_server.join(timeout)
        if proxy_server.is_alive():
            raise TimeoutError("host-loopback proxy endpoint was not reached")
        if proxy_errors:
            raise RuntimeError("host-loopback proxy server failed") from proxy_errors[0]
        denied_general.settimeout(0.25)
        try:
            unexpected, _ = denied_general.accept()
        except TimeoutError:
            pass
        else:
            unexpected.close()
            raise RuntimeError("host-loopback deny reached a general host service")
        for listener in denied_udp:
            try:
                unexpected_udp = listener.recv(4096)
            except TimeoutError:
                continue
            raise RuntimeError(
                "host-loopback deny reached a host UDP service "
                f"on port {listener.getsockname()[1]}: {unexpected_udp!r}"
            )
    finally:
        for listener in denied_udp:
            listener.close()
        denied_general.close()
        proxy.close()
        if proxy_server.ident is not None:
            proxy_server.join(timeout=1)

    allowed_general = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    allowed_general.bind(("0.0.0.0", 0))
    allowed_general.listen(1)
    allowed_general.settimeout(timeout)
    allowed_general_port = int(allowed_general.getsockname()[1])
    _, host_forward_port = _available_tcp_address()
    guest_forward_port = 18081
    allow_errors: list[Exception] = []
    allow_server = threading.Thread(
        target=_http_server,
        args=(
            allowed_general,
            b"/general",
            b"NVX-HOST-LOOPBACK-GENERAL",
            timeout,
            allow_errors,
        ),
        name="nvx-host-loopback-general",
        daemon=True,
    )
    allow_server.start()
    try:
        allow_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            network=DIRECTIONAL_NETWORK_CIDR,
        )
        allow_command.extend(
            (
                "--host-loopback",
                "allow",
                "--network-ingress",
                "deny",
                "--host-loopback-forward",
                f"tcp:{host_forward_port}:{guest_forward_port}",
            )
        )
        with OpenvmmProcess(
            allow_command,
            output_dir / "host-loopback-allow.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-host-loopback-policy",
                "NVX_HOST_LOOPBACK_POLICY",
                _render_script(
                    "host-loopback-policy.sh.in",
                    MODE="allow",
                    GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                    GENERAL_PORT=str(allowed_general_port),
                    PROXY_PORT="0",
                    GUEST_PORT=str(guest_forward_port),
                ),
            )
            process.wait_for_line(HOST_LOOPBACK_INGRESS_READY_MARKER, timeout)
            with socket.create_connection(
                ("127.0.0.1", host_forward_port), timeout=min(timeout, 5)
            ) as inbound:
                inbound.sendall(b"NVX-HOST-LOOPBACK-INBOUND\n")
                time.sleep(1)
                inbound.shutdown(socket.SHUT_WR)
            process.wait_for_line(HOST_LOOPBACK_ALLOW_MARKER, timeout)
            allowed = process.wait(timeout)
        if allowed.returncode != 0:
            raise RuntimeError(
                f"host-loopback allow guest exited with {allowed.returncode}"
            )
        allow_server.join(timeout)
        if allow_server.is_alive():
            raise TimeoutError("general host-loopback service was not reached")
        if allow_errors:
            raise RuntimeError("general host-loopback server failed") from allow_errors[
                0
            ]
    finally:
        allowed_general.close()
        allow_server.join(timeout=1)

    run_host_loopback_rejections(
        executable,
        kernel,
        initrd,
        backend,
        memory_mib=memory_mib,
        timeout=timeout,
        output_dir=output_dir,
    )


def run_host_loopback_rejections(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    for name, extra, expected in (
        (
            "generic-allow",
            ("--host-loopback", "allow"),
            b"does not support generic host-loopback connectivity",
        ),
        (
            "generic-allow-with-proxy",
            (
                "--host-loopback",
                "allow",
                "--network-proxy",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:3128",
            ),
            b"does not support generic host-loopback connectivity",
        ),
        (
            "deny-forward",
            (
                "--host-loopback",
                "deny",
                "--host-loopback-forward",
                "tcp:3000:8080",
            ),
            b"--host-loopback-forward requires explicit --host-loopback allow",
        ),
        (
            "wrong-proxy-address",
            (
                "--host-loopback",
                "deny",
                "--network-proxy",
                "198.51.100.1:3128",
            ),
            b"must match the guest gateway",
        ),
    ):
        invalid = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            network=DIRECTIONAL_NETWORK_CIDR,
        )
        invalid.extend(extra)
        with tempfile.TemporaryDirectory(prefix="nvx-loopback-rejection-") as temporary:
            pidfile = Path(temporary) / "absent" / "openvmm.pid"
            if name.startswith("generic-allow"):
                invalid.extend(("--pidfile", str(pidfile)))
            with OpenvmmProcess(
                invalid,
                output_dir / f"host-loopback-{name}.log",
            ) as process:
                result = process.wait(timeout)
            if pidfile.parent.exists():
                raise RuntimeError(
                    "rejected host-loopback policy created host resources"
                )
        if (
            result.returncode == 0
            or expected not in result.output
            or BOOT_MARKER in result.output
        ):
            raise RuntimeError(
                f"invalid host-loopback policy {name} was not rejected before boot"
            )


def run_denied_filesystem_paths(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-denied-paths-") as temporary:
        root = Path(temporary) / "share"
        allowed = root / "allowed"
        secrets = root / "secrets"
        allowed.mkdir(parents=True)
        secrets.mkdir()
        (allowed / "seed").write_bytes(b"NVX-ALLOWED\n")
        secret = secrets / "token"
        secret.write_bytes(b"NVX-SECRET\n")
        alias = root / "alias"
        try:
            os.symlink("secrets", alias, target_is_directory=True)
        except OSError as error:
            if os.name != "nt":
                raise
            result = subprocess.run(
                ["cmd", "/c", "mklink", "/J", str(alias), str(secrets)],
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode != 0:
                raise RuntimeError(
                    f"failed to create denied-path junction: {result.stderr.strip()}"
                ) from error
        command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            mount=f"/mnt/share,{root},rw",
        )
        command.extend(("--mount-deny", str(secrets)))
        run_guest_script(
            command,
            _read_script("denied-filesystem-paths.sh"),
            FILESYSTEM_DENIED_MARKER,
            timeout=timeout,
            log_path=output_dir / "denied-filesystem-paths.log",
        )
        if (allowed / "from-guest").read_bytes() != b"NVX-GUEST-WRITE\n":
            raise RuntimeError("allowed filesystem path did not remain writable")
        if secret.read_bytes() != b"NVX-SECRET\n":
            raise RuntimeError("denied filesystem path was modified")

        outside = Path(temporary) / "outside"
        outside.mkdir()
        for name, denied_paths, expected in (
            ("outside", (outside,), b"outside the filesystem export root"),
            ("duplicate", (secrets, secrets), b"unique and non-overlapping"),
            ("root", (root,), b"cannot hide the complete filesystem export"),
        ):
            invalid = workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                mount=f"/mnt/share,{root},rw",
            )
            for path in denied_paths:
                invalid.extend(("--mount-deny", str(path)))
            with OpenvmmProcess(
                invalid,
                output_dir / f"denied-filesystem-{name}.log",
            ) as process:
                result = process.wait(timeout)
            if (
                result.returncode == 0
                or expected not in result.output
                or BOOT_MARKER in result.output
            ):
                raise RuntimeError(
                    f"unsafe denied filesystem policy {name} was not rejected before boot"
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
    check_tsc_sync: bool = False,
) -> None:
    capacity = 8
    cmdline = "quiet loglevel=0 maxcpus=1"
    script = _read_script("restore-processors.sh")
    if check_tsc_sync:
        cmdline += " clearcpuid=tsc_adjust"
        script = _read_script("restore-tsc-sync.sh") + script
    with tempfile.TemporaryDirectory(prefix="nvx-restore-processors-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        boot_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            cmdline,
            processors=capacity,
        )
        capture_snapshot(
            [*boot_command, "--snapshot-destination", str(snapshot_path)],
            snapshot_path,
            backend=backend,
            timeout=timeout,
            processors=1,
            post_restore_script=script,
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
    descriptor = guest_descriptor(args.guest)
    if args.memory_mib is None:
        args.memory_mib = descriptor.default_memory_mib
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(artifact_path("vmlinux"), "microVM PVH kernel")
    initrd = require_file(
        artifact_path(descriptor.initramfs_name),
        f"microVM {descriptor.distribution} initramfs",
    )
    output_dir: Path = args.output_dir
    output_dir.mkdir(parents=True, exist_ok=True)
    if args.scenario is None:
        scenarios = tuple(
            scenario
            for scenario in MICROVM_TEST_SCENARIOS
            if descriptor.name != "ubuntu"
            or scenario not in UBUNTU_UNSUPPORTED_SCENARIOS
        )
    else:
        scenarios = tuple(dict.fromkeys(args.scenario))
        unsupported: set[str] = set()
        if descriptor.name == "ubuntu":
            for scenario in UBUNTU_UNSUPPORTED_SCENARIOS:
                if scenario in scenarios:
                    unsupported.add(scenario)
        if unsupported:
            raise ScriptError(
                "Ubuntu guest does not support correctness scenario(s): "
                + ", ".join(sorted(unsupported))
            )

    if "guest-boot" in scenarios:
        print(
            f"Running {descriptor.distribution} initramfs boot correctness "
            f"on OpenVMM/{args.backend}"
        )
        run_guest_boot(
            executable,
            kernel,
            initrd,
            args.backend,
            descriptor,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            log_path=output_dir / f"{descriptor.name}-guest-boot.log",
        )
    if "guest-identity" in scenarios:
        print(
            f"Running {descriptor.distribution} identity correctness "
            f"on OpenVMM/{args.backend}"
        )
        run_guest_identity(
            executable,
            kernel,
            initrd,
            args.backend,
            descriptor,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            log_path=output_dir / f"{descriptor.name}-guest-identity.log",
        )
    if "console-exit" in scenarios:
        for processors in dict.fromkeys(args.processors):
            print(
                f"Running microVM console exit correctness ({processors} vCPU) "
                f"on OpenVMM/{args.backend}"
            )
            run_console_exit(
                executable,
                kernel,
                initrd,
                args.backend,
                processors,
                memory_mib=args.memory_mib,
                timeout=args.timeout,
                output_dir=output_dir,
            )
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
    if "workload-identity" in scenarios:
        print(
            f"Running microVM workload identity correctness on OpenVMM/{args.backend}"
        )
        run_workload_identity(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "managed-lifecycle" in scenarios:
        print(f"Running managed microVM lifecycle on OpenVMM/{args.backend}")
        run_managed_lifecycle(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "structured-outcome" in scenarios:
        print(f"Running structured microVM outcomes on OpenVMM/{args.backend}")
        run_structured_outcome(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "l3-l4-egress-policy" in scenarios:
        print(f"Running microVM L3/L4 egress policy on OpenVMM/{args.backend}")
        run_l3_l4_egress_policy(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "host-loopback-policy" in scenarios:
        print(f"Running microVM host-loopback policy on OpenVMM/{args.backend}")
        run_host_loopback_policy(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "denied-filesystem-paths" in scenarios:
        print(f"Running microVM denied filesystem paths on OpenVMM/{args.backend}")
        run_denied_filesystem_paths(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
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
    if "restore-tsc-sync" in scenarios:
        print(f"Running microVM restore TSC synchronization on OpenVMM/{args.backend}")
        tsc_output_dir = output_dir / "restore-tsc-sync"
        tsc_output_dir.mkdir(parents=True, exist_ok=True)
        run_restore_processors(
            executable,
            kernel,
            initrd,
            args.backend,
            args.processors,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=tsc_output_dir,
            check_tsc_sync=True,
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
    if "directional-network-policy" in scenarios:
        print(f"Running microVM directional network policy on OpenVMM/{args.backend}")
        run_directional_network_policy(
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
