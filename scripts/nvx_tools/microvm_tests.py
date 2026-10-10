"""Correctness tests for NVX Linux guests running on OpenVMM microVMs."""

from __future__ import annotations

import argparse
import ipaddress
import json
import os
import queue
import random
import re
import secrets
import selectors
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from collections.abc import Sequence
from contextlib import ExitStack
from dataclasses import dataclass
from pathlib import Path
from stat import S_ISUID
from typing import Any, cast

from .benchmark import (
    CAPTURE_LOG_FILTER,
    RESTORE_MARKER,
    SMP_PROBE_COMPLETION_MARKER,
    SNAPSHOT_PROFILE_ENV,
    GuestCommandResult,
    GuestFailureReported,
    contains_output_line,
    measure_once,
    parse_snapshot_profile_line,
    positive_float,
    positive_int,
    record_adversarial_openvmm_pid,
    smp_probe_script,
    snapshot_restore_command,
    workload_boot_command,
)
from .benchmark import (
    capture_snapshot as _capture_snapshot,
)
from .benchmark import (
    run_guest_script as _run_guest_script,
)
from .build_constants import (
    BuildConstants,
    KernelBuildConstants,
)
from .ci import OPENVMM_TEST_BACKENDS, validate_openvmm_test_backend
from .common import (
    ScriptError,
    artifact_path,
    openvmm_binary_path,
    require_file,
    sha256_file,
)
from .control_session import ControlSession, ManagedExecRefused, capability_pipe
from .egress_policy import CompiledEgressPolicy, compile_policy_file
from .guests import GUEST_NAMES, GuestDescriptor, guest_descriptor
from .managed_exec_tests import run_managed_exec_configuration
from .openvmm_process import OpenvmmProcess, TcpConsole
from .sandbox_lifecycle_tests import run_sandbox_lifecycle
from .time_abi import (
    ABI_VERSION,
    CHECK_CPU_BUDGET_US,
    CPU_PROFILE_FALLBACK_MARKER,
    CPU_PROFILE_REQUEST_URL,
    STATUS_TIMEOUT_SECONDS,
    WARP_PROBE_COMPLETION_MARKER,
    WARP_PROBE_FAILURE_MARKER,
    WARP_SUMMARY_PREFIX,
    TimeAbiFailure,
    TimeAbiMonitor,
    check_cpu_budget_us,
    check_warp_probe,
    describe_exit_status,
    parse_cpu_profile_fallback,
    parse_fields,
    parse_marker,
    status_script,
    warp_probe_script,
    warp_rounds,
)

MICROVM_TEST_SCENARIOS = (
    "console-exit",
    "console-snapshot",
    "cpu-profile-fallback",
    "directional-network-policy",
    "denied-filesystem-paths",
    "endpoint-policy-snapshot",
    "filesystem-owner",
    "filesystem-policy",
    "filesystem-shares",
    "filesystem-snapshot",
    "guest-boot",
    "guest-identity",
    "host-loopback-policy",
    "ipv6-egress-policy",
    "lifecycle",
    "l3-l4-egress-policy",
    "l3-l4-egress-port-ranges",
    "l3-l4-egress-protocols",
    "managed-lifecycle",
    "managed-exec-config",
    "network-snapshot",
    "restore-downtime",
    "restore-memory",
    "restore-processors",
    "sandbox-blocks",
    "sandbox-lifecycle",
    "scratch-snapshot",
    "smp",
    "smp-snapshot",
    "snapshot-core",
    "snapshot-tiers",
    "structured-outcome",
    "time-abi-conformance",
    "virtio-net",
    "workload-identity",
)
UBUNTU_UNSUPPORTED_SCENARIOS = frozenset(("console-snapshot",))
SANDBOX_CONTROL_SCENARIOS = frozenset(
    (
        "managed-exec-config",
        "sandbox-blocks",
        "sandbox-lifecycle",
        "scratch-snapshot",
        "snapshot-tiers",
    )
)
# The public `nvx.py sandbox` scenarios need the Ubuntu scratch template that
# the guest-artifact build produces and packages omit, so they run only when
# named.
PUBLIC_SANDBOX_SCENARIOS = frozenset(("managed-exec-config", "sandbox-lifecycle"))
# The spec runs the same-host restore cases on the CI debug kernel, whose
# soft-lockup and hung-task detectors the guest's time ABI watcher reports.
DEBUG_KERNEL_SCENARIOS = (
    "smp",
    "smp-snapshot",
    "restore-processors",
    "restore-downtime",
    "snapshot-tiers",
)
# Scenarios that run only when named, never in a default suite. The time ABI
# hides TSC-deadline on every backend, so `smp` already runs on the one-shot
# counting LAPIC; `smp-lapic` repeats it and asserts the counting-LAPIC facts,
# for explicit local use (#286).
MICROVM_EXPLICIT_SCENARIOS = ("smp-lapic",)
SMP_LAPIC_COUNTING_MARKER = b"NVX-SMP-LAPIC-COUNTING-OK"
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
NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS = 0.25
NETWORK_SERVER_JOIN_TIMEOUT_SECONDS = 1.0
L3_L4_EGRESS_COMPLETION_MARKER = b"NVX-L3-L4-EGRESS-OK"
L3_L4_EGRESS_PROTOCOLS_COMPLETION_MARKER = b"NVX-L3-L4-EGRESS-PROTOCOLS-OK"
L3_L4_EGRESS_PORT_RANGES_COMPLETION_MARKER = b"NVX-L3-L4-EGRESS-PORT-RANGES-OK"
L3_L4_EGRESS_PROBE_PREFIX = "NVX-L3-L4-PROBE "
# The protocol scenario's guest probes toward the gateway: TCP and UDP to a
# named port and to the next port, and ICMP echo.
L3_L4_EGRESS_PROBES = ("tcp-named", "tcp-other", "udp-named", "udp-other", "icmp")
# The port-range scenario reserves five consecutive ports and selects the
# middle three as a range. Its guest probes the gateway with TCP and UDP below
# the range, at the range's first port, inside it, at its last port, and above.
L3_L4_EGRESS_RANGE_POSITIONS = ("below", "first", "inside", "last", "above")
L3_L4_EGRESS_RANGE_PROBES = tuple(
    f"{protocol}-{position}"
    for protocol in ("tcp", "udp")
    for position in L3_L4_EGRESS_RANGE_POSITIONS
)
# The portable profile embeds the NIC's IPv4 addresses in fd00::/96, so the
# directional network's guest is fd00::c000:202/120 behind fd00::c000:201.
DIRECTIONAL_NETWORK_GUEST_IPV6 = "fd00::c000:202"
DIRECTIONAL_NETWORK_GATEWAY_IPV6 = "fd00::c000:201"
DIRECTIONAL_NETWORK_IPV6_PREFIX_LENGTH = 120
IPV6_EGRESS_COMPLETION_MARKER = b"NVX-IPV6-EGRESS-OK"
IPV6_EGRESS_BEFORE_MARKER = b"NVX-IPV6-EGRESS-BEFORE"
IPV6_EGRESS_AFTER_MARKER = b"NVX-IPV6-EGRESS-AFTER"
IPV6_EGRESS_DNS_PREFIX = "NVX-IPV6-EGRESS-DNS "
# The IPv6 scenario's guest probes toward the gateway's IPv6 address: TCP to a
# named port and to the next port, UDP to the named port, and ICMPv6 echo,
# plus TCP to the named port over IPv4.
IPV6_EGRESS_PROBES = ("tcp6-named", "tcp6-other", "udp6-named", "icmp6", "tcp4-named")
HOST_LOOPBACK_DENY_MARKER = b"NVX-HOST-LOOPBACK-DENY-OK"
HOST_LOOPBACK_UDP_CONTROL_MARKER = b"NVX-HOST-LOOPBACK-UDP-CONTROL-OK"
HOST_LOOPBACK_ALLOW_MARKER = b"NVX-HOST-LOOPBACK-ALLOW-OK"
HOST_LOOPBACK_INGRESS_READY_MARKER = b"NVX-HOST-LOOPBACK-INGRESS-READY"
HOST_LOOPBACK_PORT_BIND_ATTEMPTS = 16
# IANA's dynamic port range, which is also Windows' default. The host-loopback
# scenario draws the port of each TCP and UDP listener pair from it.
HOST_LOOPBACK_PORT_RANGE = (49152, 65535)
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
FILESYSTEM_OWNER_MARKER = b"NVX-FILESYSTEM-OWNER-OK"
# A guest caller that the filesystem-owner scenario expects to differ from the
# share owner and from OpenVMM's identity.
FILESYSTEM_OWNER_FOREIGN_IDENTITY = (4242, 4242)
# The share owner when the tests run as root, which caller ownership refuses.
FILESYSTEM_OWNER_ROOT_RUN_OWNER = 65533
CAP_SETGID = 6
CAP_SETUID = 7
FILESYSTEM_DORMANT_BEFORE_MARKER = b"NVX-FILESYSTEM-DORMANT-BEFORE"
FILESYSTEM_DORMANT_ATTACHED_MARKER = b"NVX-FILESYSTEM-DORMANT-ATTACHED"
FILESYSTEM_LIVE_BEFORE_MARKER = b"NVX-FILESYSTEM-LIVE-BEFORE"
FILESYSTEM_LIVE_AFTER_MARKER = b"NVX-FILESYSTEM-LIVE-AFTER"
FILESYSTEM_SHARES_MARKER = b"NVX-FILESYSTEM-SHARES-OK"
FILESYSTEM_SHARES_BEFORE_MARKER = b"NVX-FILESYSTEM-SHARES-BEFORE"
FILESYSTEM_SHARES_AFTER_MARKER = b"NVX-FILESYSTEM-SHARES-AFTER"
FILESYSTEM_POLICY_MARKER = b"NVX-FILESYSTEM-POLICY-OK"
FILESYSTEM_POLICY_BEFORE_MARKER = b"NVX-FILESYSTEM-POLICY-BEFORE"
FILESYSTEM_POLICY_AFTER_MARKER = b"NVX-FILESYSTEM-POLICY-AFTER"
NETWORK_BEFORE_MARKER = b"NVX-NETWORK-BEFORE"
NETWORK_INVALIDATED_MARKER = b"NVX-NETWORK-OLD-FLOW-INVALIDATED"
NETWORK_AFTER_MARKER = b"NVX-NETWORK-AFTER"
SCRATCH_PAIRED_POST_MARKER = b"NVX-SCRATCH-PAIRED-POST-OUT"
SCRATCH_PAIRED_RESTORED_MARKER = b"NVX-SCRATCH-PAIRED-RESTORED"
SCRATCH_FRESH_POST_MARKER = b"NVX-SCRATCH-FRESH-POST-OUT"
SCRATCH_FRESH_VALUE_PREFIX = b"NVX-SCRATCH-FRESH-VALUE-"
SCRATCH_FRESH_VALUE_SUFFIX = b"-END"
WORKLOAD_IDENTITY_MARKER = b"NVX-WORKLOAD-IDENTITY-OK uid=65534 gid=65534"
# A direct-mode guest refuses a working directory that the workload cannot
# enter with the Linux error number, which does not depend on the host.
MANAGED_REFUSED_CWDS = (
    ("missing", "/does-not-exist", 2),  # ENOENT
    ("file", "/etc/passwd", 20),  # ENOTDIR
    ("inaccessible", "/root", 13),  # EACCES
)
MANAGED_REFUSED_CWD_MARKER = "/tmp/nvx-refused-cwd-ran"
BOOT_MARKER = b"NVX-GUEST-BOOT-OK:"
GUEST_BOOT_COMPLETION_MARKER = b"NVX-GUEST-BOOT-CHECK-OK"
GUEST_IDENTITY_COMPLETION_MARKER = b"NVX-GUEST-IDENTITY-OK"
RESTORE_PROCESSORS_FAILURE_MARKER = b"NVX-RESTORE-PROCESSORS-FAIL"
# Records the time ABI rates and restore clock report of each restore. These
# records are written before the restored VPs run, so they never split a guest
# console line; targets that log while the guest runs, such as virt_kvm's
# hidden-MSR #GPs, could split a marker in the merged console stream.
TIME_ABI_RESTORE_LOG_FILTER = "off,openvmm_core::worker::dispatch::time_abi=info"
# The test hook that makes OpenVMM's --cpu-profile auto treat the host CPU as
# one that no built-in CPU profile serves, so that it falls back to a host
# profile on CI's runners, which built-in profiles serve.
CPU_PROFILE_FALLBACK_HOOK = ("--x-time-abi-test-hook", "host-cpu-unknown")
# The fallback capture's OpenVMM log: the capture's records, and the time ABI's
# fallback warning and declared rates, which name the CPU profile.
CPU_PROFILE_FALLBACK_LOG_FILTER = (
    f"{CAPTURE_LOG_FILTER},openvmm_core::worker::dispatch::time_abi=info"
)
# The CPU profile of each partition, in the time ABI's declared rates record.
DECLARED_CPU_PROFILE = re.compile(
    rb'time ABI rates declared [^\r\n]*\bcpu_profile="([a-z0-9.-]+)"'
)
# The guest finishes a restore after the RCU grace-period release, which gives
# up after rcu_cpu_stall_timeout (21 s), and the deferred C7 check.
TIME_ABI_RESTORE_FINISH_SECONDS = 30.0
# Every scenario captures a cold-booted guest (generation 0), and OpenVMM
# cannot capture a restored one, so every restore carries generation 1.
RESTORED_GENERATION = 1
# The spec's long-downtime case: longer than the 21 s RCU stall timeout, at 1
# and 8 vCPUs, with and without expedited grace periods.
RESTORE_DOWNTIME_SECONDS = 30.0
RESTORE_DOWNTIME_PROCESSORS = (1, 8)
RESTORE_DOWNTIME_COMPLETION_MARKER = b"NVX-RESTORE-DOWNTIME-OK"
RESTORE_DOWNTIME_PATH = "/tmp/nvx-restore-downtime"
# Gives the RCU stall detector time to report a stall that the release of
# the stall suppression exposed.
RESTORE_DOWNTIME_SETTLE_SECONDS = 2
# The guest's exhaustive CI check reports each check on each online CPU.
TIME_ABI_EXHAUSTIVE_COMMAND = "/sbin/nvx-time exhaustive"
TIME_ABI_EXHAUSTIVE_PREFIX = "NVX-TIME-ABI-EXHAUSTIVE: "
TIME_ABI_EXHAUSTIVE_CHECKS = ("X1", "X2", "X3", "X4", "X5", "X6")
TIME_ABI_EXHAUSTIVE_EXIT_PREFIX = "NVX-EXHAUSTIVE-EXIT status="
TIME_ABI_EXHAUSTIVE_COMPLETION_MARKER = b"NVX-EXHAUSTIVE-DONE"
TIME_ABI_EXHAUSTIVE_REPORTED_FAILURES = 8
# One line per test-microvm run that sums up the time ABI evidence in its guest
# logs, which CI uploads only for failed jobs.
TIME_ABI_EVIDENCE_PREFIX = "NVX-TIME-ABI-EVIDENCE: "
RESTORE_DOWNTIME_REPORT_PREFIX = "NVX-RESTORE-DOWNTIME "
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
        choices=(*MICROVM_TEST_SCENARIOS, *MICROVM_EXPLICIT_SCENARIOS),
        help=(
            "scenario to run; repeat to select multiple (default: all except "
            "smp-lapic, managed-exec-config, and sandbox-lifecycle, which run "
            "only when named)"
        ),
    )
    parser.add_argument(
        "--debug-kernel",
        action="store_true",
        help=(
            "boot the CI debug kernel (build/vmlinux-debug), which enables the "
            "soft-lockup and hung-task detectors; selects the same-host restore "
            "scenarios unless --scenario is given"
        ),
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
        default=BuildConstants.BUILD_DIR / "test-results" / "microvm",
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
    contain_process_tree: bool = False,
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
        contain_process_tree=contain_process_tree,
        time_abi_status=True,
    )


def capture_snapshot(
    command: Sequence[str],
    snapshot_path: Path,
    *,
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
    log_filter: str = CAPTURE_LOG_FILTER,
) -> tuple[float, float, float, int]:
    return _capture_snapshot(
        command,
        snapshot_path,
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
        time_abi_status=True,
        log_filter=log_filter,
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
    schema_version = raw["schema_version"]
    if (
        isinstance(schema_version, bool)
        or not isinstance(schema_version, int)
        or schema_version != 1
    ):
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


def _single_framed_marker_value(output: bytes, prefix: bytes, suffix: bytes) -> bytes:
    values: list[bytes] = []
    offset = 0
    while True:
        start = output.find(prefix, offset)
        if start < 0:
            break
        value_start = start + len(prefix)
        value_end = output.find(suffix, value_start)
        if value_end < 0:
            raise RuntimeError(f"malformed {prefix!r} marker")
        values.append(output[value_start:value_end])
        offset = value_end + len(suffix)
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
    # Only failure paths reach here with an open console; the monitor still
    # scans the tail, but nothing is raised over the error being handled.
    if console is not None:
        output = console.finish(check=False)
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
            "set -e\n"
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
            "set -e\n"
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


def _refused_managed_exec(
    session: ControlSession, cwd: str, *, timeout: float
) -> ManagedExecRefused:
    """Runs a workload in an unusable directory; returns the guest's refusal."""
    try:
        result = session.exec(
            ("/bin/touch", MANAGED_REFUSED_CWD_MARKER),
            timeout_ms=5_000,
            response_timeout=timeout,
            cwd=cwd,
        )
    except ManagedExecRefused as refusal:
        return refusal
    raise RuntimeError(
        f"managed working directory {cwd} was not refused: "
        f"returncode={result.returncode} category={result.category} "
        f"stderr={result.stderr!r}"
    )


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
        guest_log_path = output_dir / "managed-lifecycle-guest.log"
        process: subprocess.Popen[bytes] | None = None
        boot_console: TcpConsole | None = None
        with log_path.open("wb") as log:
            try:
                environment = os.environ.copy()
                environment["OPENVMM_LOG"] = "off"
                # OpenVMM reads its capability as soon as it starts.
                capability_input = capability_pipe(capability)
                try:
                    process = subprocess.Popen(
                        command,
                        stdin=capability_input,
                        stdout=log,
                        stderr=subprocess.STDOUT,
                        env=environment,
                    )
                finally:
                    os.close(capability_input)
                record_adversarial_openvmm_pid(process.pid, environment)
                # The guest console has no shell to query: init hands the boot
                # to the managed agent, which serves the control console. The
                # monitor still fails the run on a violation or failed check
                # printed there, and a failed boot check powers the guest off
                # with status 193, which fails the exit check below.
                boot_console = TcpConsole.connect(
                    boot_console_address, timeout, monitor=TimeAbiMonitor(command)
                )
                with ControlSession.connect(
                    Path(endpoint_value), capability, timeout
                ) as session:
                    session.ping(timeout)
                    first = session.exec(
                        (
                            "/bin/sh",
                            "-c",
                            "/bin/touch nvx-managed-state; "
                            'printf \'%s|%s|%s\' "$PWD" "$EMPTY" "$COMPLEX"',
                        ),
                        timeout_ms=5_000,
                        response_timeout=timeout,
                        cwd="/tmp",
                        environment=(
                            "EMPTY=",
                            "COMPLEX=space = \N{SNOWMAN}",
                        ),
                    )
                if (
                    first.returncode != 0
                    or first.category != "exit"
                    or first.stdout != "/tmp||space = \N{SNOWMAN}".encode()
                    or first.stderr
                ):
                    raise RuntimeError(
                        "first managed workload returned an invalid result"
                    )

                with ControlSession.connect(
                    Path(endpoint_value), capability, timeout
                ) as session:
                    empty_environment = session.exec(
                        ("/usr/bin/env",),
                        timeout_ms=5_000,
                        response_timeout=timeout,
                        environment=(),
                    )
                    exact_environment = session.exec(
                        ("/usr/bin/env",),
                        timeout_ms=5_000,
                        response_timeout=timeout,
                        environment=("EMPTY=", "COMPLEX=space = \N{SNOWMAN}"),
                    )
                    if (
                        empty_environment.returncode != 0
                        or empty_environment.stdout
                        or empty_environment.stderr
                        or exact_environment.returncode != 0
                        or exact_environment.stderr
                        or exact_environment.stdout
                        != "EMPTY=\nCOMPLEX=space = \N{SNOWMAN}\n".encode()
                    ):
                        raise RuntimeError(
                            "managed exec did not preserve the exact exec environment"
                        )
                    for workload_timeout in (0, 3_600_001, 86_400_000, 0xFFFFFFFF):
                        boundary = session.exec(
                            ("/bin/true",),
                            timeout_ms=workload_timeout,
                            response_timeout=timeout,
                        )
                        if (
                            boundary.returncode != 0
                            or boundary.category != "exit"
                            or boundary.stdout
                            or boundary.stderr
                        ):
                            raise RuntimeError(
                                "managed exec rejected a valid uint32 timeout"
                            )
                    second = session.exec(
                        (
                            "/bin/sh",
                            "-c",
                            "pwd; test -f /tmp/nvx-managed-state; "
                            'test "${COMPLEX-unset}" = unset; '
                            "test ! -e /run/nvx/workload-machine-id; "
                            "printf second-exec",
                        ),
                        timeout_ms=5_000,
                        response_timeout=timeout,
                    )
                    timed_out = session.exec(
                        ("/bin/sleep", "5"),
                        timeout_ms=100,
                        response_timeout=timeout,
                    )
                    after_timeout = session.exec(
                        ("/bin/sh", "-c", "printf still-usable"),
                        timeout_ms=5_000,
                        response_timeout=timeout,
                    )
                    refused_cwds = [
                        (
                            description,
                            cwd,
                            error,
                            _refused_managed_exec(session, cwd, timeout=timeout),
                        )
                        for description, cwd, error in MANAGED_REFUSED_CWDS
                    ]
                    after_refusals = session.exec(
                        (
                            "/bin/sh",
                            "-c",
                            f"test ! -e {MANAGED_REFUSED_CWD_MARKER} && "
                            "printf still-usable",
                        ),
                        timeout_ms=5_000,
                        response_timeout=timeout,
                    )
                    session.stop(timeout)
                if (
                    second.returncode != 0
                    or second.category != "exit"
                    or second.stdout != b"/\nsecond-exec"
                    or second.stderr
                ):
                    raise RuntimeError(
                        "managed workload state did not survive across exec requests"
                    )
                if timed_out.returncode != 124 or timed_out.category != "timeout":
                    raise RuntimeError("managed workload timeout was not reported")
                if (
                    after_timeout.returncode != 0
                    or after_timeout.category != "exit"
                    or after_timeout.stdout != b"still-usable"
                    or after_timeout.stderr
                ):
                    raise RuntimeError(
                        "managed guest was not usable after a workload timeout"
                    )
                for description, cwd, error, refusal in refused_cwds:
                    if (
                        refusal.category != "cwd-failed"
                        or refusal.status != error
                        or refusal.stdout
                        or f"cannot enter working directory {cwd}: ".encode()
                        not in refusal.stderr
                    ):
                        raise RuntimeError(
                            f"{description} managed working directory was not "
                            f"refused clearly: {refusal} stderr={refusal.stderr!r}"
                        )
                if (
                    after_refusals.returncode != 0
                    or after_refusals.category != "exit"
                    or after_refusals.stdout != b"still-usable"
                    or after_refusals.stderr
                ):
                    raise RuntimeError(
                        "a refused managed workload ran, or the guest was not "
                        "usable after refusals"
                    )
                result = process.wait(timeout=timeout)
                if result != 0:
                    reason = describe_exit_status(result)
                    raise RuntimeError(
                        f"managed OpenVMM process exited with status {result}"
                        + (f": {reason}; see {log_path}" if reason else "")
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
                # Keep the guest log, then fail on anything the monitor found
                # in the console's tail.
                guest_log_path.write_bytes(boot_console.finish(check=False))
                boot_console.finish()
                boot_console = None
            finally:
                if boot_console is not None:
                    guest_log_path.write_bytes(boot_console.finish(check=False))
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


def counting_lapic_script(processors: int) -> str:
    """Return a guest check that every CPU runs the one-shot counting LAPIC.

    The time ABI hides TSC-deadline, so no CPU lists ``tsc_deadline_timer``,
    and each online CPU's clockevent device is ``lapic``, not
    ``lapic-deadline``.
    """
    return (
        "if grep -qw tsc_deadline_timer /proc/cpuinfo; then\n"
        "    echo SMP-LAPIC-FAIL tsc-deadline-exposed\n"
        "    exit 89\n"
        "fi\n"
        "cpu=0\n"
        f'while [ "$cpu" -lt {processors} ]; do\n'
        "    device=$(cat /sys/devices/system/clockevents/clockevent$cpu/"
        "current_device 2>/dev/null)\n"
        '    if [ "$device" != lapic ]; then\n'
        '        echo "SMP-LAPIC-FAIL cpu=$cpu clockevent=${device:-none}"\n'
        "        exit 89\n"
        "    fi\n"
        "    cpu=$((cpu + 1))\n"
        "done\n"
        f"echo {SMP_LAPIC_COUNTING_MARKER.decode()}\n"
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
    counting_lapic: bool = False,
) -> None:
    command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0",
        processors=processors,
    )
    script = (
        smp_probe_script(processors, exit_guest=False)
        + warp_probe_script()
        + "nvx-exit 0\n"
    )
    if counting_lapic:
        script = counting_lapic_script(processors) + script
    result = run_guest_script(
        command,
        script,
        WARP_PROBE_COMPLETION_MARKER,
        timeout=timeout,
        log_path=log_path,
    )
    output = result["text"].encode("utf-8")
    if not contains_output_line(output, SMP_PROBE_COMPLETION_MARKER):
        raise RuntimeError(
            f"{processors}-vCPU guest finished without the SMP probe marker "
            f"{SMP_PROBE_COMPLETION_MARKER.decode()!r}"
        )
    if counting_lapic:
        if not contains_output_line(output, SMP_LAPIC_COUNTING_MARKER):
            raise RuntimeError(
                f"{processors}-vCPU guest finished without the counting-LAPIC "
                f"marker {SMP_LAPIC_COUNTING_MARKER.decode()!r}"
            )
        # The boot line must also report the backend's LAPIC rate for all
        # the CPUs; the status query alone doesn't check the CPU count.
        monitor = TimeAbiMonitor(command)
        monitor.feed(output)
        monitor.finish()
        try:
            monitor.require_boot("the counting-LAPIC check", online_cpus=processors)
        except TimeAbiFailure as error:
            raise RuntimeError(f"{processors}-vCPU smp-lapic: {error}") from error
    check_warp_probe(
        result["text"],
        cpus=processors,
        context=f"{processors}-vCPU boot",
        rounds=warp_rounds(processors),
    )


def _check_exhaustive_report(text: str, *, processors: int) -> None:
    """Check that ``nvx-time exhaustive`` passed every check on every CPU."""
    results: dict[tuple[str, int], dict[str, str]] = {}
    summary: dict[str, str] | None = None
    exit_status: str | None = None
    problems: list[str] = []
    for raw in text.splitlines():
        line = raw.removesuffix("\r")
        if line.startswith(TIME_ABI_EXHAUSTIVE_EXIT_PREFIX):
            exit_status = line.removeprefix(TIME_ABI_EXHAUSTIVE_EXIT_PREFIX)
            continue
        if not line.startswith(TIME_ABI_EXHAUSTIVE_PREFIX):
            continue
        try:
            fields = parse_fields(line.removeprefix(TIME_ABI_EXHAUSTIVE_PREFIX))
            if fields.get("v") != ABI_VERSION:
                raise ValueError(f"version {fields.get('v')!r} is not {ABI_VERSION}")
            if "check" not in fields:
                summary = fields
                continue
            key = (fields["check"], int(fields["cpu"]))
        except (KeyError, ValueError) as error:
            raise RuntimeError(
                f"time ABI exhaustive check: malformed line {line!r}: {error}"
            ) from error
        if key in results:
            problems.append(f"{key[0]} reported cpu={key[1]} twice")
        results[key] = fields
    if exit_status is None:
        raise RuntimeError(
            "time ABI exhaustive check: the guest printed no exit status for "
            f"{TIME_ABI_EXHAUSTIVE_COMMAND}"
        )
    if summary is None and not results:
        raise RuntimeError(
            f"time ABI exhaustive check: {TIME_ABI_EXHAUSTIVE_COMMAND} exited with "
            f"status {exit_status} and reported nothing; the guest image does not "
            "provide the exhaustive check"
        )
    failed = [
        f"{check} cpu={cpu} failed: {fields.get('detail', '')}"
        for (check, cpu), fields in sorted(results.items())
        if fields.get("status") != "pass"
    ]
    if len(failed) > TIME_ABI_EXHAUSTIVE_REPORTED_FAILURES:
        hidden = len(failed) - TIME_ABI_EXHAUSTIVE_REPORTED_FAILURES
        failed = failed[:TIME_ABI_EXHAUSTIVE_REPORTED_FAILURES] + [
            f"{hidden} more failed checks"
        ]
    problems.extend(failed)
    for check in TIME_ABI_EXHAUSTIVE_CHECKS:
        missing = [cpu for cpu in range(processors) if (check, cpu) not in results]
        if missing:
            problems.append(
                f"{check} reported nothing for cpu {', '.join(map(str, missing))}"
            )
    if summary is None:
        problems.append("no summary line")
    else:
        expected = {"status": "ok", "cpus": str(processors), "failures": "0"}
        mismatched = [
            f"{name}={summary.get(name)}"
            for name, value in expected.items()
            if summary.get(name) != value
        ]
        if mismatched:
            problems.append(
                f"summary reports {' '.join(mismatched)} for {processors} vCPUs"
            )
    if exit_status != "0":
        problems.append(f"exit status {exit_status}")
    if problems:
        raise RuntimeError("time ABI exhaustive check: " + "; ".join(problems))


def time_abi_evidence(output_dir: Path, backend: str | None = None) -> dict[str, str]:
    """Sum up the time ABI evidence in the guest logs of one run.

    nvx-time status prints every recorded phase, oldest first, and then its
    runtime line, so a restored guest repeats its source's boot and capture
    lines. Only each query's newest phase line counts: one boot per cold-boot
    query and one restore per post-restore query. Where the guest reports a
    check's CPU time (cpu_us), the evidence also counts the checks over
    ``backend``'s CPU-time budget for their phase; it never gates on either.
    """
    offsets: list[int] = []
    backward: list[int] = []
    elapsed: dict[str, list[int]] = {"boot": [], "capture": [], "restore": []}
    cpu: dict[str, list[int]] = {"boot": [], "capture": [], "restore": []}
    over_budget: dict[str, int] = {"boot": 0, "capture": 0, "restore": 0}
    exhaustive: list[str] = []
    stalls: list[str] = []
    for path in sorted(output_dir.rglob("*.log")):
        newest: dict[str, str] | None = None
        for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
            line = raw.removesuffix("\r")
            try:
                if line.startswith(WARP_SUMMARY_PREFIX):
                    fields = parse_fields(line.removeprefix(WARP_SUMMARY_PREFIX))
                    offsets.append(int(fields["max_abs_offset_ns"]))
                    backward.append(int(fields["max_backward_ns"]))
                elif line.startswith(TIME_ABI_EXHAUSTIVE_PREFIX):
                    fields = parse_fields(line.removeprefix(TIME_ABI_EXHAUSTIVE_PREFIX))
                    if "check" not in fields:
                        exhaustive.append(
                            f"{fields['status']}/{fields['cpus']}/{fields['failures']}"
                        )
                elif line.startswith(RESTORE_DOWNTIME_REPORT_PREFIX):
                    fields = parse_fields(
                        line.removeprefix(RESTORE_DOWNTIME_REPORT_PREFIX)
                    )
                    stalls.append(fields["stalls"])
                else:
                    marker = parse_marker(line)
                    if marker is None:
                        continue
                    if marker["phase"] != "runtime":
                        newest = marker
                        continue
                    last, newest = newest, None
                    if (
                        last is None
                        or last["status"] != "ok"
                        or last["phase"] not in elapsed
                    ):
                        continue
                    phase = last["phase"]
                    elapsed[phase].append(int(last["elapsed_us"]))
                    if "cpu_us" in last:
                        cpu_us = int(last["cpu_us"])
                        cpu[phase].append(cpu_us)
                        budget = (
                            None
                            if backend is None
                            else check_cpu_budget_us(backend, phase, int(last["cpus"]))
                        )
                        if budget is not None and cpu_us > budget:
                            over_budget[phase] += 1
            except (KeyError, ValueError):
                continue
    evidence: dict[str, str] = {}
    if offsets:
        evidence["warp_runs"] = str(len(offsets))
        evidence["warp_max_abs_offset_ns"] = str(max(offsets))
        evidence["warp_max_backward_ns"] = str(max(backward))
    for phase, values in elapsed.items():
        if values:
            evidence[f"{phase}_markers"] = str(len(values))
            evidence[f"{phase}_elapsed_us"] = f"{min(values)}-{max(values)}"
        if cpu[phase]:
            evidence[f"{phase}_cpu_us"] = f"{min(cpu[phase])}-{max(cpu[phase])}"
            if backend is not None and phase in CHECK_CPU_BUDGET_US.get(backend, {}):
                evidence[f"{phase}_cpu_over_budget"] = str(over_budget[phase])
    if exhaustive:
        evidence["exhaustive"] = ",".join(exhaustive)
    if stalls:
        evidence["downtime_stalls"] = ",".join(stalls)
    return evidence


def report_time_abi_evidence(
    output_dir: Path, *, backend: str, guest: str, debug_kernel: bool
) -> str | None:
    """Print the run's evidence line and add it to the GitHub job summary."""
    evidence = time_abi_evidence(output_dir, backend)
    if not evidence:
        return None
    fields = {
        "backend": backend,
        "guest": guest,
        "kernel": "debug" if debug_kernel else "default",
        **evidence,
    }
    line = TIME_ABI_EVIDENCE_PREFIX + " ".join(f"{k}={v}" for k, v in fields.items())
    print(line)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(f"\n`{line}`\n")
    return line


def run_time_abi_conformance(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    processors: int,
    *,
    memory_mib: int,
    timeout: float,
    log_path: Path,
) -> None:
    """Run the guest's exhaustive CI conformance check on every CPU."""
    command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0",
        processors=processors,
    )
    # One input line: the console echoes all of it before the check prints.
    result = run_guest_script(
        command,
        f"{TIME_ABI_EXHAUSTIVE_COMMAND}; "
        f'echo "{TIME_ABI_EXHAUSTIVE_EXIT_PREFIX}$?"; '
        f"echo {TIME_ABI_EXHAUSTIVE_COMPLETION_MARKER.decode()}; nvx-exit 0\n",
        TIME_ABI_EXHAUSTIVE_COMPLETION_MARKER,
        timeout=timeout,
        log_path=log_path,
    )
    _check_exhaustive_report(result["text"], processors=processors)


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
        listener.settimeout(NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS)
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
        server.join(timeout=NETWORK_SERVER_JOIN_TIMEOUT_SECONDS)


def _bind_consecutive_ports(
    socket_type: socket.SocketKind, count: int
) -> list[socket.socket]:
    for base in range(20000, 60000 - count):
        endpoints: list[socket.socket] = []
        try:
            for offset in range(count):
                endpoint = socket.socket(socket.AF_INET, socket_type)
                endpoints.append(endpoint)
                endpoint.bind(("0.0.0.0", base + offset))
                if socket_type == socket.SOCK_STREAM:
                    endpoint.listen(1)
            return endpoints
        except OSError:
            for endpoint in endpoints:
                endpoint.close()
    raise RuntimeError(f"could not reserve {count} consecutive host ports")


def _bind_egress_ports() -> tuple[list[socket.socket], list[socket.socket]]:
    with ExitStack() as cleanup:
        tcp = _bind_consecutive_ports(socket.SOCK_STREAM, 5)
        for endpoint in tcp:
            cleanup.callback(endpoint.close)
        udp = _bind_consecutive_ports(socket.SOCK_DGRAM, 5)
        for endpoint in udp:
            cleanup.callback(endpoint.close)
        cleanup.pop_all()
        return tcp, udp


def _bounded_egress_policy(
    gateway: str,
    tcp_ports: tuple[int, int, int],
    udp_ports: tuple[int, int, int],
    *,
    policy_file: Path | None = None,
) -> CompiledEgressPolicy:
    allow: list[dict[str, object]] = []
    deny: list[dict[str, object]] = []
    for protocol, (start, denied, end) in (
        ("tcp", tcp_ports),
        ("udp", udp_ports),
    ):
        allow.extend(
            (
                {
                    "cidr": "192.0.2.0/24",
                    "except": [f"{gateway}/32"],
                    "protocol": protocol,
                    "port": start,
                    "endPort": end,
                },
                {
                    "cidr": f"{gateway}/32",
                    "protocol": protocol,
                    "port": start,
                    "endPort": end,
                },
            )
        )
        deny.extend(
            (
                {
                    "cidr": "192.0.2.0/24",
                    "except": [f"{gateway}/32"],
                    "protocol": protocol,
                    "port": start,
                    "endPort": end,
                },
                {
                    "cidr": f"{gateway}/32",
                    "protocol": protocol,
                    "port": denied,
                },
            )
        )

    def compile_file(path: Path) -> CompiledEgressPolicy:
        path.write_text(json.dumps({"allow": allow, "deny": deny}), encoding="utf-8")
        return compile_policy_file(path)

    if policy_file is not None:
        return compile_file(policy_file)
    with tempfile.TemporaryDirectory(prefix="nvx-egress-policy-") as temporary:
        return compile_file(Path(temporary) / "policy.json")


def run_l3_l4_egress_policy(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
    guest: str = "alpine",
) -> None:
    tcp, udp = _bind_egress_ports()
    for endpoint in (*tcp, *udp):
        endpoint.settimeout(timeout)
    tcp_ports = tuple(int(endpoint.getsockname()[1]) for endpoint in tcp)
    udp_ports = tuple(int(endpoint.getsockname()[1]) for endpoint in udp)
    server_errors: list[Exception] = []
    allowed_observations: list[str] = []
    blocked_observations: list[str] = []

    def serve_allowed() -> None:
        try:
            for listener, suffix, observation in (
                (tcp[1], b"START", "tcp:start"),
                (tcp[3], b"END", "tcp:end"),
            ):
                connection, _ = listener.accept()
                with connection:
                    connection.settimeout(timeout)
                    request = connection.recv(4096)
                    if not request.startswith(b"GET /allowed HTTP/1."):
                        raise RuntimeError(
                            f"unexpected L3/L4 HTTP request: {request!r}"
                        )
                    body = b"NVX-L3-L4-TCP-ALLOW-" + suffix
                    connection.sendall(
                        b"HTTP/1.1 200 OK\r\nContent-Length: "
                        + str(len(body)).encode("ascii")
                        + b"\r\nConnection: close\r\n\r\n"
                        + body
                    )
                allowed_observations.append(observation)
            for endpoint, suffix, observation in (
                (udp[1], b"START", "udp:start"),
                (udp[3], b"END", "udp:end"),
            ):
                payload, _ = endpoint.recvfrom(128)
                if payload != b"NVX-L3-L4-UDP-ALLOW-" + suffix:
                    raise RuntimeError(f"unexpected allowed UDP payload: {payload!r}")
                allowed_observations.append(observation)
        except Exception as error:
            server_errors.append(error)

    server = threading.Thread(
        target=serve_allowed,
        name="nvx-l3-l4-egress-test",
        daemon=True,
    )
    server.start()
    try:
        policy_path = output_dir / "l3-l4-requested-policy.json"
        _bounded_egress_policy(
            DIRECTIONAL_NETWORK_GATEWAY_IPV4,
            (tcp_ports[1], tcp_ports[2], tcp_ports[3]),
            (udp_ports[1], udp_ports[2], udp_ports[3]),
            policy_file=policy_path,
        )
        command = [
            sys.executable,
            str(Path(__file__).parents[1] / "nvx.py"),
            "run",
            "--guest",
            guest,
            "--hypervisor",
            backend,
            "--memory-mib",
            str(memory_mib),
            "--net",
            DIRECTIONAL_NETWORK_CIDR,
            "--network-profile",
            "portable",
            "--network-egress",
            "deny",
            "--network-ingress",
            "deny",
            "--network-egress-policy-file",
            str(policy_path),
            "--cmdline",
            "quiet loglevel=0",
        ]

        run_guest_script(
            command,
            _render_script(
                "l3-l4-egress-policy.sh.in",
                GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                TCP_ADJACENT_LOW=str(tcp_ports[0]),
                TCP_START=str(tcp_ports[1]),
                TCP_DENIED=str(tcp_ports[2]),
                TCP_END=str(tcp_ports[3]),
                TCP_ADJACENT_HIGH=str(tcp_ports[4]),
                UDP_ADJACENT_LOW=str(udp_ports[0]),
                UDP_START=str(udp_ports[1]),
                UDP_DENIED=str(udp_ports[2]),
                UDP_END=str(udp_ports[3]),
                UDP_ADJACENT_HIGH=str(udp_ports[4]),
            ),
            L3_L4_EGRESS_COMPLETION_MARKER,
            timeout=timeout,
            log_path=output_dir / "l3-l4-egress-policy.log",
            contain_process_tree=True,
        )
        server.join(timeout)
        if server.is_alive():
            raise TimeoutError("L3/L4 allowed endpoints were not reached")
        if server_errors:
            raise RuntimeError(
                "L3/L4 allowed endpoint server failed"
            ) from server_errors[0]
        expected_allowed = ["tcp:start", "tcp:end", "udp:start", "udp:end"]
        if allowed_observations != expected_allowed:
            raise RuntimeError(
                "L3/L4 allowed endpoint observations were incomplete: "
                f"{allowed_observations!r}"
            )

        for endpoint, observation in (
            (tcp[0], "tcp:adjacent-low"),
            (tcp[2], "tcp:interior"),
            (tcp[4], "tcp:adjacent-high"),
        ):
            endpoint.settimeout(NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS)
            try:
                unexpected, _ = endpoint.accept()
            except TimeoutError:
                blocked_observations.append(observation)
            else:
                unexpected.close()
                raise RuntimeError("blocked TCP range port reached the host")
        for endpoint, observation in (
            (udp[0], "udp:adjacent-low"),
            (udp[2], "udp:interior"),
            (udp[4], "udp:adjacent-high"),
        ):
            endpoint.settimeout(NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS)
            try:
                unexpected, _ = endpoint.recvfrom(128)
            except TimeoutError:
                blocked_observations.append(observation)
            else:
                raise RuntimeError(
                    f"blocked UDP range port reached the host: {unexpected!r}"
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
                    "192.0.2.1:sctp:443",
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

        (output_dir / "l3-l4-egress-policy-results.json").write_text(
            json.dumps(
                {
                    "interface": "nvx.py run",
                    "policy_file": policy_path.name,
                    "tcp_ports": {
                        "adjacent_low": tcp_ports[0],
                        "allowed_start": tcp_ports[1],
                        "denied_interior": tcp_ports[2],
                        "allowed_end": tcp_ports[3],
                        "adjacent_high": tcp_ports[4],
                    },
                    "udp_ports": {
                        "adjacent_low": udp_ports[0],
                        "allowed_start": udp_ports[1],
                        "denied_interior": udp_ports[2],
                        "allowed_end": udp_ports[3],
                        "adjacent_high": udp_ports[4],
                    },
                    "observed": {
                        "allowed": allowed_observations,
                        "blocked": blocked_observations,
                    },
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    finally:
        for endpoint in (*tcp, *udp):
            endpoint.close()
        server.join(timeout=NETWORK_SERVER_JOIN_TIMEOUT_SECONDS)


@dataclass(frozen=True)
class _EgressCase:
    name: str
    allow: tuple[dict[str, object], ...]
    deny: tuple[dict[str, object], ...]
    reached: frozenset[str]
    default: str = "deny"


def _egress_protocol_cases(gateway: str, named_port: int) -> tuple[_EgressCase, ...]:
    """Return an allow case and a deny case for each protocol selector."""
    destination = f"{gateway}/32"
    every_probe = frozenset(L3_L4_EGRESS_PROBES)
    protocol_selectors: tuple[tuple[str, dict[str, object], frozenset[str]], ...] = (
        ("tcp", {"protocol": "tcp"}, frozenset(("tcp-named", "tcp-other"))),
        ("udp", {"protocol": "udp"}, frozenset(("udp-named", "udp-other"))),
        ("icmp", {"protocol": "icmp"}, frozenset(("icmp",))),
        (
            "any-port",
            {"protocol": "any", "port": named_port},
            frozenset(("tcp-named", "udp-named")),
        ),
        ("any", {"protocol": "any"}, every_probe),
    )
    cases: list[_EgressCase] = []
    for name, selector, matched in protocol_selectors:
        rule: dict[str, object] = {"cidr": destination, **selector}
        cases.append(_EgressCase(f"allow-{name}", (rule,), (), matched))
        # The address-only allow rule admits every probe, so only deny
        # precedence can block one.
        cases.append(
            _EgressCase(
                f"deny-{name}",
                ({"cidr": destination},),
                (rule,),
                every_probe - matched,
            )
        )
    return tuple(cases)


def _bind_dual_protocol_ports(
    count: int, host: str = "0.0.0.0"
) -> tuple[list[socket.socket], list[socket.socket]]:
    """Reserve consecutive port numbers that are free for both TCP and UDP."""
    for base in range(20000, 60000 - count):
        tcp: list[socket.socket] = []
        udp: list[socket.socket] = []
        try:
            for port in range(base, base + count):
                listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                tcp.append(listener)
                listener.bind((host, port))
                listener.listen(8)
                datagram = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                udp.append(datagram)
                datagram.bind((host, port))
            return tcp, udp
        except OSError:
            for endpoint in (*tcp, *udp):
                endpoint.close()
    raise RuntimeError(f"could not reserve {count} consecutive TCP and UDP host ports")


class _EgressProbeServer:
    """Record which host endpoints the guest's egress probes reach."""

    def __init__(self, endpoints: dict[str, socket.socket], timeout: float) -> None:
        self._endpoints = endpoints
        self._timeout = timeout
        self._stop = threading.Event()
        self._reached: set[str] = set()
        self._errors: list[Exception] = []
        self._thread = threading.Thread(
            target=self._serve,
            name="nvx-l3-l4-egress-probes",
            daemon=True,
        )

    def __enter__(self) -> _EgressProbeServer:
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self._stop.set()
        self._thread.join(NETWORK_SERVER_JOIN_TIMEOUT_SECONDS)

    def finish(self) -> frozenset[str]:
        """Return the reached probes once traffic in flight has had time to land."""
        time.sleep(NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS)
        self._stop.set()
        self._thread.join(self._timeout)
        if self._thread.is_alive():
            raise TimeoutError("L3/L4 egress probe server did not stop")
        if self._errors:
            raise RuntimeError("L3/L4 egress probe server failed") from self._errors[0]
        return frozenset(self._reached)

    def _serve(self) -> None:
        try:
            with selectors.DefaultSelector() as selector:
                for name, endpoint in self._endpoints.items():
                    selector.register(endpoint, selectors.EVENT_READ, name)
                while not self._stop.is_set():
                    for key, _ in selector.select(timeout=0.05):
                        self._receive(
                            cast(str, key.data), cast(socket.socket, key.fileobj)
                        )
        except Exception as error:
            self._errors.append(error)

    def _receive(self, name: str, endpoint: socket.socket) -> None:
        token = f"NVX-EGRESS-PROBE-{name}".encode("ascii")
        if name.startswith("tcp"):
            connection, _ = endpoint.accept()
            with connection:
                connection.settimeout(self._timeout)
                request = connection.recv(4096)
                if not request.startswith(f"GET /{name} HTTP/1.".encode("ascii")):
                    raise RuntimeError(f"unexpected {name} probe request: {request!r}")
                connection.sendall(
                    b"HTTP/1.1 200 OK\r\nContent-Length: "
                    + str(len(token)).encode("ascii")
                    + b"\r\nConnection: close\r\n\r\n"
                    + token
                )
        else:
            payload, _ = endpoint.recvfrom(128)
            if payload != token:
                raise RuntimeError(f"unexpected {name} probe payload: {payload!r}")
        self._reached.add(name)


def _egress_probe_reports(output: str) -> dict[str, str]:
    reports: dict[str, str] = {}
    for line in output.splitlines():
        line = line.strip()
        if line.startswith(L3_L4_EGRESS_PROBE_PREFIX):
            name, _, value = line[len(L3_L4_EGRESS_PROBE_PREFIX) :].partition("=")
            reports[name] = value
    return reports


def _check_egress_case(
    kind: str,
    case: _EgressCase,
    probes: tuple[str, ...],
    guest_reports: dict[str, str],
    host_reached: frozenset[str],
) -> None:
    for probe in probes:
        expected = probe in case.reached
        report = guest_reports.get(probe)
        if probe.startswith("udp"):
            guest_matches = report == "sent"
        else:
            guest_matches = report == ("reached" if expected else "blocked")
        # Consomme answers echo requests to the gateway itself, so only the
        # guest observes ICMP.
        observed = probe in host_reached
        if not guest_matches or (not probe.startswith("icmp") and observed != expected):
            raise RuntimeError(
                f"L3/L4 {kind} case {case.name}: {probe} should be "
                f"{'reached' if expected else 'blocked'}, but the guest reported "
                f"{report!r} and the host "
                f"{'observed' if observed else 'did not observe'} it"
            )


def run_l3_l4_egress_protocols(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
    guest: str = "alpine",
) -> None:
    tcp, udp = _bind_dual_protocol_ports(2)
    try:
        named_port, other_port = (int(endpoint.getsockname()[1]) for endpoint in tcp)
        # Consomme answers ICMP echo itself, so only TCP and UDP have host endpoints.
        endpoints = {
            "tcp-named": tcp[0],
            "tcp-other": tcp[1],
            "udp-named": udp[0],
            "udp-other": udp[1],
        }
        script = _render_script(
            "l3-l4-egress-protocols.sh.in",
            GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
            NAMED_PORT=str(named_port),
            OTHER_PORT=str(other_port),
        )
        results: list[dict[str, object]] = []
        for case in _egress_protocol_cases(
            DIRECTIONAL_NETWORK_GATEWAY_IPV4, named_port
        ):
            policy_path = output_dir / f"l3-l4-egress-{case.name}-policy.json"
            policy_path.write_text(
                json.dumps({"allow": case.allow, "deny": case.deny}, indent=2) + "\n",
                encoding="utf-8",
            )
            command = [
                sys.executable,
                str(Path(__file__).parents[1] / "nvx.py"),
                "run",
                "--guest",
                guest,
                "--hypervisor",
                backend,
                "--memory-mib",
                str(memory_mib),
                "--net",
                DIRECTIONAL_NETWORK_CIDR,
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-ingress",
                "deny",
                "--network-egress-policy-file",
                str(policy_path),
                "--cmdline",
                "quiet loglevel=0",
            ]
            with _EgressProbeServer(endpoints, timeout) as server:
                result = run_guest_script(
                    command,
                    script,
                    L3_L4_EGRESS_PROTOCOLS_COMPLETION_MARKER,
                    timeout=timeout,
                    log_path=output_dir / f"l3-l4-egress-{case.name}.log",
                    contain_process_tree=True,
                )
                host_reached = server.finish()
            guest_reports = _egress_probe_reports(result["text"])
            _check_egress_case(
                "protocol", case, L3_L4_EGRESS_PROBES, guest_reports, host_reached
            )
            results.append(
                {
                    "case": case.name,
                    "policy_file": policy_path.name,
                    "expected_reached": sorted(case.reached),
                    "guest": guest_reports,
                    "host_reached": sorted(host_reached),
                }
            )

        for name, rule, expected in (
            (
                "icmp-port",
                "192.0.2.1:icmp:443",
                b"ICMP egress rules do not take a port",
            ),
            ("any-protocol", "192.0.2.1:any", b"invalid egress transport"),
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
            invalid.extend(("--network-egress", "deny", "--network-egress-allow", rule))
            with OpenvmmProcess(
                invalid,
                output_dir / f"l3-l4-egress-{name}.log",
            ) as process:
                rejected = process.wait(timeout)
            if (
                rejected.returncode == 0
                or expected not in rejected.output
                or BOOT_MARKER in rejected.output
            ):
                raise RuntimeError(
                    f"invalid L3/L4 protocol rule {name} was not rejected before boot"
                )

        (output_dir / "l3-l4-egress-protocols-results.json").write_text(
            json.dumps(
                {
                    "interface": "nvx.py run",
                    "ports": {"named": named_port, "other": other_port},
                    "cases": results,
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    finally:
        for endpoint in (*tcp, *udp):
            endpoint.close()


def _egress_port_range_cases(
    gateway: str, first: int, inside: int, last: int
) -> tuple[_EgressCase, ...]:
    """Return an allow case and a deny case for a TCP, a UDP, and an `any`
    range of ports, and an allowed TCP range around a denied port, as
    MXC-shaped policy rules."""
    destination = [{"cidr": f"{gateway}/32"}]
    every_probe = frozenset(L3_L4_EGRESS_RANGE_PROBES)

    def in_range(*protocols: str) -> frozenset[str]:
        return frozenset(
            f"{protocol}-{position}"
            for protocol in protocols
            for position in ("first", "inside", "last")
        )

    def on_ports(**selector: object) -> dict[str, object]:
        return {"to": destination, "ports": [selector]}

    cases: list[_EgressCase] = []
    for protocol, matched in (
        ("tcp", in_range("tcp")),
        ("udp", in_range("udp")),
        ("any", in_range("tcp", "udp")),
    ):
        rule = on_ports(protocol=protocol, port=first, endPort=last)
        cases.append(_EgressCase(f"allow-{protocol}", (rule,), (), matched))
        # The address-only allow rule admits every probe, so only deny
        # precedence can block one.
        cases.append(
            _EgressCase(
                f"deny-{protocol}",
                ({"to": destination},),
                (rule,),
                every_probe - matched,
            )
        )
    cases.append(
        _EgressCase(
            "allow-tcp-deny-inside",
            (on_ports(protocol="tcp", port=first, endPort=last),),
            (on_ports(protocol="tcp", port=inside),),
            frozenset(("tcp-first", "tcp-last")),
        )
    )
    return tuple(cases)


def run_l3_l4_egress_port_ranges(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
    guest: str = "alpine",
) -> None:
    tcp, udp = _bind_dual_protocol_ports(len(L3_L4_EGRESS_RANGE_POSITIONS))
    try:
        ports = {
            position: int(endpoint.getsockname()[1])
            for position, endpoint in zip(
                L3_L4_EGRESS_RANGE_POSITIONS, tcp, strict=True
            )
        }
        endpoints = {
            f"{protocol}-{position}": endpoint
            for protocol, reserved in (("tcp", tcp), ("udp", udp))
            for position, endpoint in zip(
                L3_L4_EGRESS_RANGE_POSITIONS, reserved, strict=True
            )
        }
        first, last = ports["first"], ports["last"]
        script = _render_script(
            "l3-l4-egress-port-ranges.sh.in",
            GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
            BELOW_PORT=str(ports["below"]),
            FIRST_PORT=str(first),
            INSIDE_PORT=str(ports["inside"]),
            LAST_PORT=str(last),
            ABOVE_PORT=str(ports["above"]),
        )
        public_run = [
            sys.executable,
            str(Path(__file__).parents[1] / "nvx.py"),
            "run",
            "--guest",
            guest,
            "--hypervisor",
            backend,
            "--memory-mib",
            str(memory_mib),
            "--net",
            DIRECTIONAL_NETWORK_CIDR,
            "--network-profile",
            "portable",
            "--network-egress",
            "deny",
            "--network-ingress",
            "deny",
        ]
        results: list[dict[str, object]] = []
        for case in _egress_port_range_cases(
            DIRECTIONAL_NETWORK_GATEWAY_IPV4, first, ports["inside"], last
        ):
            policy_path = output_dir / f"l3-l4-egress-range-{case.name}-policy.json"
            policy_path.write_text(
                json.dumps({"allow": case.allow, "deny": case.deny}, indent=2) + "\n",
                encoding="utf-8",
            )
            # Each range must reach OpenVMM as one native rule, not as a rule
            # for each of its ports.
            native = compile_policy_file(policy_path)
            if not any(
                rule.endswith(f":{first}-{last}")
                for rule in (*native.allow, *native.deny)
            ):
                raise RuntimeError(
                    f"L3/L4 port-range case {case.name} did not lower its range "
                    f"to a native range rule: {native!r}"
                )
            command = [
                *public_run,
                "--network-egress-policy-file",
                str(policy_path),
                "--cmdline",
                "quiet loglevel=0",
            ]
            with _EgressProbeServer(endpoints, timeout) as server:
                result = run_guest_script(
                    command,
                    script,
                    L3_L4_EGRESS_PORT_RANGES_COMPLETION_MARKER,
                    timeout=timeout,
                    log_path=output_dir / f"l3-l4-egress-range-{case.name}.log",
                    contain_process_tree=True,
                )
                host_reached = server.finish()
            guest_reports = _egress_probe_reports(result["text"])
            _check_egress_case(
                "port-range",
                case,
                L3_L4_EGRESS_RANGE_PROBES,
                guest_reports,
                host_reached,
            )
            results.append(
                {
                    "case": case.name,
                    "policy_file": policy_path.name,
                    "native_rules": {
                        "allow": list(native.allow),
                        "deny": list(native.deny),
                    },
                    "expected_reached": sorted(case.reached),
                    "guest": guest_reports,
                    "host_reached": sorted(host_reached),
                }
            )

        # OpenVMM rejects a reversed range and a range without its first port
        # before boot.
        for name, rule, expected in (
            (
                "reversed-range",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:tcp:{last}-{first}",
                b"ends below its first port",
            ),
            (
                "missing-first-port",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}:udp:-{last}",
                b"invalid egress port range",
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
            invalid.extend(("--network-egress", "deny", "--network-egress-allow", rule))
            with OpenvmmProcess(
                invalid,
                output_dir / f"l3-l4-egress-range-{name}.log",
            ) as process:
                rejected = process.wait(timeout)
            if (
                rejected.returncode == 0
                or expected not in rejected.output
                or BOOT_MARKER in rejected.output
            ):
                raise RuntimeError(
                    f"invalid L3/L4 port range {name} was not rejected before boot"
                )

        # The public run rejects the same shapes in a policy file before it
        # starts OpenVMM.
        destination = [{"cidr": f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}/32"}]
        for name, policy_rule, message in (
            (
                "reversed-end-port",
                {
                    "to": destination,
                    "ports": [{"protocol": "tcp", "port": last, "endPort": first}],
                },
                f"allow[0].ports[0].endPort {first} cannot be below port {last}",
            ),
            (
                "end-port-without-port",
                {"to": destination, "ports": [{"protocol": "udp", "endPort": last}]},
                "allow[0].ports[0].endPort requires port",
            ),
        ):
            policy_path = output_dir / f"l3-l4-egress-range-{name}-policy.json"
            policy_path.write_text(
                json.dumps({"allow": [policy_rule]}, indent=2) + "\n",
                encoding="utf-8",
            )
            rejected_run = subprocess.run(
                [
                    *public_run,
                    "--network-egress-policy-file",
                    str(policy_path),
                    "--dry-run",
                ],
                capture_output=True,
                timeout=timeout,
                check=False,
            )
            if rejected_run.returncode == 0 or message.encode() not in (
                rejected_run.stderr
            ):
                raise RuntimeError(
                    f"invalid L3/L4 port range policy {name} was not rejected "
                    "before launch"
                )

        (output_dir / "l3-l4-egress-port-ranges-results.json").write_text(
            json.dumps(
                {"interface": "nvx.py run", "ports": ports, "cases": results},
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    finally:
        for endpoint in (*tcp, *udp):
            endpoint.close()


@dataclass(frozen=True)
class _Ipv6EgressCase(_EgressCase):
    # The gateway address that the guest names as its DNS server, if any.
    dns: str | None = None


def _ipv6_egress_cases(
    gateway_ipv4: str, gateway_ipv6: str, named_port: int, other_port: int
) -> tuple[_Ipv6EgressCase, ...]:
    """Return the IPv6 scenario's MXC-shaped policies, the probes that each
    admits, and the DNS server that each gives the guest: the IPv4 gateway when
    the policy allows DNS to it, and otherwise the IPv6 gateway when it allows
    DNS to that one. The other port follows the named port."""
    gateway = [{"cidr": f"{gateway_ipv6}/128"}]
    every_probe = frozenset(IPV6_EGRESS_PROBES)
    named_tcp: dict[str, object] = {"protocol": "tcp", "port": named_port}
    return (
        # An unfiltered NIC carries every probe, so the IPv6 path itself works.
        _Ipv6EgressCase(
            "allow-all", (), (), every_probe, default="allow", dns=gateway_ipv4
        ),
        # A denied default without rules blocks IPv6 as well as IPv4.
        _Ipv6EgressCase("deny-all", (), (), frozenset()),
        _Ipv6EgressCase(
            "allow-ipv6-tcp-port",
            ({"to": gateway, "ports": [named_tcp]},),
            (),
            frozenset(("tcp6-named",)),
        ),
        # A port range reaches OpenVMM as one IPv6 range rule.
        _Ipv6EgressCase(
            "allow-ipv6-tcp-range",
            (
                {
                    "to": gateway,
                    "ports": [
                        {"protocol": "tcp", "port": named_port, "endPort": other_port}
                    ],
                },
            ),
            (),
            frozenset(("tcp6-named", "tcp6-other")),
        ),
        _Ipv6EgressCase(
            "allow-ipv6-udp-icmp",
            ({"to": gateway, "ports": [{"protocol": "udp"}, {"protocol": "icmp"}]},),
            (),
            frozenset(("udp6-named", "icmp6")),
            dns=gateway_ipv6,
        ),
        # The IPv6 wildcard admits IPv6 alone, and the deny rule takes
        # precedence over it.
        _Ipv6EgressCase(
            "deny-ipv6-tcp-port",
            ({"to": [{"cidr": "::/0"}]},),
            ({"to": gateway, "ports": [named_tcp]},),
            every_probe - {"tcp6-named", "tcp4-named"},
            dns=gateway_ipv6,
        ),
        # A rule without destinations matches both families.
        _Ipv6EgressCase(
            "allow-tcp-port-without-to",
            ({"ports": [named_tcp]},),
            (),
            frozenset(("tcp6-named", "tcp4-named")),
        ),
        # Rules match only their own family, so IPv4 rules leave IPv6 to the
        # denied default, and IPv6 rules leave IPv4 to the allowed one.
        _Ipv6EgressCase(
            "allow-ipv4-only",
            ({"to": [{"cidr": "0.0.0.0/0"}]},),
            (),
            frozenset(("tcp4-named",)),
            dns=gateway_ipv4,
        ),
        _Ipv6EgressCase(
            "deny-ipv6-only",
            (),
            ({"to": [{"cidr": "fd00::/8"}]},),
            frozenset(("tcp4-named",)),
            default="allow",
            dns=gateway_ipv4,
        ),
    )


def _ipv6_egress_dns_report(output: str) -> str | None:
    """Return the DNS server that the guest reported, or ``none``."""
    reports = [
        line.strip()[len(IPV6_EGRESS_DNS_PREFIX) :]
        for line in output.splitlines()
        if line.strip().startswith(IPV6_EGRESS_DNS_PREFIX)
    ]
    return reports[-1] if reports else None


def _bind_ipv6_egress_ports() -> dict[str, socket.socket]:
    """Reserve the IPv6 scenario's host endpoints on two consecutive ports.

    The gateway maps guest flows onto host loopback, so the IPv6 probes reach
    ``::1`` and the IPv4 probe reaches ``127.0.0.1``.
    """
    try:
        with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as loopback:
            loopback.bind(("::1", 0))
    except OSError as error:
        raise RuntimeError("the host has no IPv6 loopback address") from error
    layout = (
        ("tcp6-named", socket.AF_INET6, socket.SOCK_STREAM, "::1", 0),
        ("tcp6-other", socket.AF_INET6, socket.SOCK_STREAM, "::1", 1),
        ("udp6-named", socket.AF_INET6, socket.SOCK_DGRAM, "::1", 0),
        ("tcp4-named", socket.AF_INET, socket.SOCK_STREAM, "127.0.0.1", 0),
    )
    for base in range(20000, 59999):
        endpoints: dict[str, socket.socket] = {}
        try:
            for name, family, kind, host, offset in layout:
                endpoint = socket.socket(family, kind)
                endpoints[name] = endpoint
                endpoint.bind((host, base + offset))
                if kind == socket.SOCK_STREAM:
                    endpoint.listen(8)
            return endpoints
        except OSError:
            for endpoint in endpoints.values():
                endpoint.close()
    raise RuntimeError("could not reserve consecutive IPv6 and IPv4 loopback ports")


def _ipv6_identity_values() -> dict[str, str]:
    """Return the guest scripts' IPv6 identity, including the kernel's spelling
    of addresses in ``/proc/net/if_inet6`` and ``/proc/net/ipv6_route``."""

    def kernel(address: str) -> str:
        return ipaddress.IPv6Address(address).exploded.replace(":", "")

    return {
        "GUEST_IPV6": DIRECTIONAL_NETWORK_GUEST_IPV6,
        "GATEWAY_IPV6": DIRECTIONAL_NETWORK_GATEWAY_IPV6,
        "PREFIX_LENGTH": str(DIRECTIONAL_NETWORK_IPV6_PREFIX_LENGTH),
        "GUEST_IPV6_HEX": kernel(DIRECTIONAL_NETWORK_GUEST_IPV6),
        "GATEWAY_IPV6_HEX": kernel(DIRECTIONAL_NETWORK_GATEWAY_IPV6),
        "PREFIX_LENGTH_HEX": f"{DIRECTIONAL_NETWORK_IPV6_PREFIX_LENGTH:02x}",
        "ANY_IPV6_HEX": kernel("::"),
    }


def _ipv6_egress_rule_arguments(*rules: str) -> list[str]:
    arguments = ["--network-egress", "deny"]
    for rule in rules:
        arguments.extend(("--network-egress-allow", rule))
    return arguments


def _run_ipv6_egress_snapshot(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
    endpoint: socket.socket,
    named_port: int,
) -> None:
    """Check that a restored NIC keeps its IPv6 identity and policy."""
    ipv6_rule = f"{DIRECTIONAL_NETWORK_GATEWAY_IPV6}/128:tcp:{named_port}"
    with (
        tempfile.TemporaryDirectory(prefix="nvx-ipv6-egress-") as temporary,
        _EgressProbeServer({"tcp6-named": endpoint}, timeout) as server,
    ):
        snapshot_path = Path(temporary) / "snapshot"
        capture_command = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                network=DIRECTIONAL_NETWORK_CIDR,
            ),
            "--snapshot-destination",
            str(snapshot_path),
            *_ipv6_egress_rule_arguments(ipv6_rule),
        ]
        with OpenvmmProcess(
            capture_command,
            output_dir / "ipv6-egress-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-ipv6-egress-snapshot",
                "NVX_IPV6_EGRESS_SNAPSHOT",
                _render_script(
                    "ipv6-egress-snapshot.sh.in",
                    GATEWAY_IPV6=DIRECTIONAL_NETWORK_GATEWAY_IPV6,
                    NAMED_PORT=str(named_port),
                ),
            )
            source = process.wait(timeout)
        if source.returncode != 0:
            raise RuntimeError(
                f"IPv6 egress capture source exited with {source.returncode}"
            )
        source_lines = _output_lines(source.output)
        if source_lines.count(IPV6_EGRESS_BEFORE_MARKER) != 1:
            raise RuntimeError("IPv6 egress source did not reach capture exactly once")
        if IPV6_EGRESS_AFTER_MARKER in source_lines:
            raise RuntimeError("IPv6 egress source crossed the capture boundary")

        # The policy digest binds the IPv6 rule, so an IPv4 rule for the same
        # port does not satisfy it.
        changed_command = [
            *snapshot_restore_command(
                executable, backend, snapshot_path, network_profile="portable"
            ),
            *_ipv6_egress_rule_arguments(
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}/32:tcp:{named_port}"
            ),
        ]
        with OpenvmmProcess(
            changed_command,
            output_dir / "ipv6-egress-restore-changed.log",
        ) as process:
            changed = process.wait(timeout)
        if changed.returncode == 0 or (
            b"restore-time egress policy does not match" not in changed.output
        ):
            raise RuntimeError(
                "IPv6 egress restore with a changed rule was not rejected"
            )

        restore_command = [
            *snapshot_restore_command(
                executable, backend, snapshot_path, network_profile="portable"
            ),
            *_ipv6_egress_rule_arguments(ipv6_rule),
        ]
        with OpenvmmProcess(
            restore_command,
            output_dir / "ipv6-egress-restore.log",
        ) as process:
            process.wait_for(IPV6_EGRESS_AFTER_MARKER, timeout)
            restored = process.wait(timeout)
        if restored.returncode != 0:
            raise RuntimeError(f"IPv6 egress restore exited with {restored.returncode}")
        if _output_lines(restored.output).count(IPV6_EGRESS_AFTER_MARKER) != 1:
            raise RuntimeError("IPv6 egress restore did not reach the host once")
        if server.finish() != frozenset(("tcp6-named",)):
            raise RuntimeError("IPv6 egress snapshot probes did not reach the host")


def run_ipv6_egress_policy(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
    guest: str = "alpine",
) -> None:
    endpoints = _bind_ipv6_egress_ports()
    try:
        named_port = int(endpoints["tcp6-named"].getsockname()[1])
        other_port = int(endpoints["tcp6-other"].getsockname()[1])
        script = _render_script(
            "ipv6-egress-policy.sh.in",
            **_ipv6_identity_values(),
            GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
            NAMED_PORT=str(named_port),
            OTHER_PORT=str(other_port),
        )
        results: list[dict[str, object]] = []
        for case in _ipv6_egress_cases(
            DIRECTIONAL_NETWORK_GATEWAY_IPV4,
            DIRECTIONAL_NETWORK_GATEWAY_IPV6,
            named_port,
            other_port,
        ):
            policy_path = output_dir / f"ipv6-egress-{case.name}-policy.json"
            policy_path.write_text(
                json.dumps({"allow": case.allow, "deny": case.deny}, indent=2) + "\n",
                encoding="utf-8",
            )
            native = compile_policy_file(policy_path)
            command = [
                sys.executable,
                str(Path(__file__).parents[1] / "nvx.py"),
                "run",
                "--guest",
                guest,
                "--hypervisor",
                backend,
                "--memory-mib",
                str(memory_mib),
                "--net",
                DIRECTIONAL_NETWORK_CIDR,
                "--network-profile",
                "portable",
                "--network-egress",
                case.default,
                "--network-ingress",
                "deny",
                "--network-egress-policy-file",
                str(policy_path),
                "--cmdline",
                "quiet loglevel=0",
            ]
            with _EgressProbeServer(endpoints, timeout) as server:
                result = run_guest_script(
                    command,
                    script,
                    IPV6_EGRESS_COMPLETION_MARKER,
                    timeout=timeout,
                    log_path=output_dir / f"ipv6-egress-{case.name}.log",
                    contain_process_tree=True,
                )
                host_reached = server.finish()
            guest_reports = _egress_probe_reports(result["text"])
            _check_egress_case(
                "IPv6 egress", case, IPV6_EGRESS_PROBES, guest_reports, host_reached
            )
            dns = _ipv6_egress_dns_report(result["text"])
            if dns != (case.dns or "none"):
                raise RuntimeError(
                    f"IPv6 egress case {case.name}: the guest named {dns!r} as its "
                    f"DNS server instead of {case.dns or 'none'!r}"
                )
            results.append(
                {
                    "case": case.name,
                    "default": case.default,
                    "policy_file": policy_path.name,
                    "native_rules": {
                        "allow": list(native.allow),
                        "deny": list(native.deny),
                    },
                    "expected_reached": sorted(case.reached),
                    "guest": guest_reports,
                    "host_reached": sorted(host_reached),
                    "dns": dns,
                }
            )

        _run_ipv6_egress_snapshot(
            executable,
            kernel,
            initrd,
            backend,
            memory_mib=memory_mib,
            timeout=timeout,
            output_dir=output_dir,
            endpoint=endpoints["tcp6-named"],
            named_port=named_port,
        )

        invalid = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            network=DIRECTIONAL_NETWORK_CIDR,
        )
        invalid.extend(
            _ipv6_egress_rule_arguments(f"{DIRECTIONAL_NETWORK_GATEWAY_IPV6}/129")
        )
        with OpenvmmProcess(
            invalid,
            output_dir / "ipv6-egress-invalid-prefix.log",
        ) as process:
            rejected = process.wait(timeout)
        if (
            rejected.returncode == 0
            or b"IPv6 prefix /129 is outside the supported range" not in rejected.output
            or BOOT_MARKER in rejected.output
        ):
            raise RuntimeError(
                "an invalid IPv6 egress rule was not rejected before boot"
            )

        (output_dir / "ipv6-egress-policy-results.json").write_text(
            json.dumps(
                {
                    "interface": "nvx.py run",
                    "guest_ipv6": (
                        f"{DIRECTIONAL_NETWORK_GUEST_IPV6}/"
                        f"{DIRECTIONAL_NETWORK_IPV6_PREFIX_LENGTH}"
                    ),
                    "gateway_ipv6": DIRECTIONAL_NETWORK_GATEWAY_IPV6,
                    "ports": {"named": named_port, "other": other_port},
                    "cases": results,
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    finally:
        for endpoint in endpoints.values():
            endpoint.close()


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


def _host_loopback_port_candidates() -> list[int]:
    """Draw distinct ports uniformly from ``HOST_LOOPBACK_PORT_RANGE``.

    Windows excludes ports per protocol, usually in blocks of 100, and gives
    back-to-back ephemeral binds consecutive ports. Ports that UDP allocates
    can therefore walk through a block that excludes only TCP until every
    attempt fails (#473). Independent draws fail only in proportion to the
    share of the range that is excluded or busy.
    """
    first, last = HOST_LOOPBACK_PORT_RANGE
    return random.SystemRandom().sample(
        range(first, last + 1), HOST_LOOPBACK_PORT_BIND_ATTEMPTS
    )


def _bind_tcp_udp_listener_pair(
    tcp_timeout: float, udp_timeout: float
) -> tuple[socket.socket, socket.socket]:
    refused: list[str] = []
    last_error: OSError | None = None
    for port in _host_loopback_port_candidates():
        with ExitStack() as sockets:
            udp_listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sockets.callback(udp_listener.close)
            tcp_listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sockets.callback(tcp_listener.close)
            protocol = "udp"
            try:
                udp_listener.bind(("127.0.0.1", port))
                protocol = "tcp"
                tcp_listener.bind(("0.0.0.0", port))
            except OSError as error:
                refused.append(f"{port}/{protocol}")
                last_error = error
                continue

            tcp_listener.listen(1)
            tcp_listener.settimeout(tcp_timeout)
            udp_listener.settimeout(udp_timeout)
            sockets.pop_all()
            return tcp_listener, udp_listener

    raise RuntimeError(
        "failed to allocate a port available to both TCP and UDP "
        f"after {HOST_LOOPBACK_PORT_BIND_ATTEMPTS} attempts; "
        f"refused {', '.join(refused)}; last error: {last_error}"
    ) from last_error


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
    with ExitStack() as listeners:
        denied_general, denied_general_udp = _bind_tcp_udp_listener_pair(
            timeout, NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS
        )
        listeners.callback(denied_general.close)
        listeners.callback(denied_general_udp.close)
        proxy, proxy_udp = _bind_tcp_udp_listener_pair(
            timeout, NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS
        )
        listeners.callback(proxy.close)
        listeners.callback(proxy_udp.close)
        listeners.pop_all()
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
    denied_udp = [proxy_udp, denied_general_udp]
    try:
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
        denied_general.settimeout(NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS)
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
            proxy_server.join(timeout=NETWORK_SERVER_JOIN_TIMEOUT_SECONDS)

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
        allow_server.join(timeout=NETWORK_SERVER_JOIN_TIMEOUT_SECONDS)

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


IO_REPARSE_TAG_LX_SYMLINK = 0xA000001D
LX_SYMLINK_VERSION = 2


def read_wsl_symlink(path: Path) -> bytes:
    """Return the target stored in a WSL-style symbolic link on Windows."""
    if sys.platform != "win32":
        raise RuntimeError("WSL-style symbolic links exist only on Windows")
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateFileW.restype = wintypes.HANDLE
    kernel32.CreateFileW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.HANDLE,
    ]
    kernel32.DeviceIoControl.restype = wintypes.BOOL
    kernel32.DeviceIoControl.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
        wintypes.LPVOID,
    ]
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    file_read_attributes = 0x80
    share_all = 0x7
    open_existing = 3
    open_reparse_point = 0x00200000
    backup_semantics = 0x02000000
    fsctl_get_reparse_point = 0x000900A8
    handle = kernel32.CreateFileW(
        str(path),
        file_read_attributes,
        share_all,
        None,
        open_existing,
        open_reparse_point | backup_semantics,
        None,
    )
    if handle in (None, wintypes.HANDLE(-1).value):
        raise RuntimeError(f"cannot open guest symbolic link: {path}")
    try:
        buffer = ctypes.create_string_buffer(16 * 1024)
        returned = wintypes.DWORD()
        if not kernel32.DeviceIoControl(
            handle,
            fsctl_get_reparse_point,
            None,
            0,
            buffer,
            len(buffer),
            ctypes.byref(returned),
            None,
        ):
            raise RuntimeError(f"guest symbolic link is not a WSL-style link: {path}")
    finally:
        kernel32.CloseHandle(handle)
    # REPARSE_DATA_BUFFER header: tag, data length, and reserved; the LX
    # payload is a 32-bit version followed by the UTF-8 target.
    data = buffer.raw[: returned.value]
    tag, length = struct.unpack_from("<IH", data)
    if tag != IO_REPARSE_TAG_LX_SYMLINK or len(data) != 8 + length or length < 4:
        raise RuntimeError(f"guest symbolic link is not a WSL-style link: {path}")
    if struct.unpack_from("<I", data, 8)[0] != LX_SYMLINK_VERSION:
        raise RuntimeError(f"guest symbolic link has an unknown layout: {path}")
    return data[12:]


def assert_guest_symlink(path: Path, target: str) -> None:
    """Check a symbolic link that the guest created on a virtio-fs share.

    Linux hosts store the exact target. Windows hosts store a WSL-style link
    with the exact target, which Windows neither reports as a symbolic link
    nor follows.
    """
    if sys.platform == "win32":
        if read_wsl_symlink(path) != target.encode():
            raise RuntimeError(
                f"guest symbolic link {path} does not point to {target!r}"
            )
        try:
            path.read_bytes()
        except OSError:
            return
        raise RuntimeError(f"the host followed a guest symbolic link: {path}")
    if not path.is_symlink() or os.readlink(path) != target:
        raise RuntimeError(f"guest symbolic link {path} does not point to {target!r}")


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
        assert_guest_symlink(allowed / "token-link", "../secrets/token")
        assert_guest_symlink(root / "guest-alias", "secrets")
        if os.path.lexists(secrets / "guest-link"):
            raise RuntimeError("guest created a symbolic link in a denied path")

        outside = Path(temporary) / "outside"
        outside.mkdir()
        for name, denied_paths, expected in (
            ("outside", (outside,), b"outside the filesystem export root"),
            ("duplicate", (secrets, secrets), b"unique and non-overlapping"),
            (
                "root",
                (root,),
                b"can hide its root only to expose allowed paths inside it",
            ),
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


def identity_capabilities_inherited(status: str, *, root: bool) -> tuple[bool, bool]:
    """Return the CAP_SETUID and CAP_SETGID a child process inherits.

    `status` is the content of `/proc/self/status`. A root process passes the
    capabilities on through its bounding set, and an unprivileged one only
    through its ambient set.
    """
    fields: dict[str, str] = {}
    for line in status.splitlines():
        name, separator, value = line.partition(":")
        if separator:
            fields[name] = value.strip()
    capabilities = int(fields["CapBnd" if root else "CapAmb"], 16)
    return (
        capabilities & (1 << CAP_SETUID) != 0,
        capabilities & (1 << CAP_SETGID) != 0,
    )


def openvmm_inherits_identity_capabilities() -> tuple[bool, bool]:
    """Return the CAP_SETUID and CAP_SETGID a child OpenVMM inherits."""
    if sys.platform != "linux":
        return (False, False)
    return identity_capabilities_inherited(
        Path("/proc/self/status").read_text(encoding="ascii"),
        root=os.geteuid() == 0,
    )


def filesystem_owner_expectations(
    *,
    export_uid: int,
    export_gid: int,
    openvmm_uid: int,
    openvmm_gid: int,
    cap_setuid: bool,
    cap_setgid: bool,
    supplementary_groups: Sequence[int],
) -> tuple[bool, bool]:
    """Derive the filesystem-owner expectations for guest callers.

    Returns (root_allowed, foreign_allowed): whether squashed guest root and
    foreign guest callers, respectively, must own the files they create rather
    than fail with EPERM.

    Per `lxutil::with_fs_identity`, a request that runs as squashed guest root
    runs as the export owner: it needs CAP_SETUID only when the export root's
    UID differs from OpenVMM's filesystem UID, and CAP_SETGID only when the
    export root's GID differs from OpenVMM's filesystem GID or OpenVMM has
    supplementary groups to drop. Foreign callers always differ from OpenVMM
    in both UID and GID, so they need both capabilities.
    """
    needs_setuid = export_uid != openvmm_uid
    needs_setgid = export_gid != openvmm_gid or bool(supplementary_groups)
    root_allowed = (not needs_setuid or cap_setuid) and (not needs_setgid or cap_setgid)
    foreign_allowed = cap_setuid and cap_setgid
    return root_allowed, foreign_allowed


def openvmm_other_groups() -> list[int]:
    """Return a child OpenVMM's supplementary groups other than its GID.

    Caller ownership performs every request without them, which only
    `CAP_SETGID` allows when there are any.
    """
    if sys.platform != "linux":
        return []
    gid = os.getegid()
    return sorted({group for group in os.getgroups() if group != gid})


def run_filesystem_owner(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    def boot_command(mount: str) -> list[str]:
        command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            mount=mount,
        )
        command.extend(("--mount-owner", "caller"))
        return command

    def assert_rejected(name: str, mount: str, expected: bytes) -> None:
        with OpenvmmProcess(
            boot_command(mount), output_dir / f"filesystem-owner-{name}.log"
        ) as process:
            result = process.wait(timeout)
        if (
            result.returncode == 0
            or expected not in result.output
            or BOOT_MARKER in result.output
        ):
            raise RuntimeError(
                f"caller ownership of a {name} export was not rejected before boot"
            )

    with tempfile.TemporaryDirectory(prefix="nvx-filesystem-owner-") as temporary:
        root = Path(temporary) / "share"
        root.mkdir()
        if sys.platform != "linux":
            assert_rejected(
                "Windows",
                f"/mnt/share,{root},rw",
                b"--mount-owner caller requires a Linux host",
            )
            return

        root.chmod(0o777)
        if os.geteuid() == 0:
            # Caller ownership squashes guest root to the share owner, which
            # therefore must not be root.
            os.chown(
                root, FILESYSTEM_OWNER_ROOT_RUN_OWNER, FILESYSTEM_OWNER_ROOT_RUN_OWNER
            )
        status = root.stat()
        owner = (status.st_uid, status.st_gid)
        taken = (*owner, os.geteuid(), os.getegid())
        foreign_uid, foreign_gid = FILESYSTEM_OWNER_FOREIGN_IDENTITY
        while foreign_uid in taken or foreign_gid in taken:
            foreign_uid, foreign_gid = foreign_uid + 2, foreign_gid + 2
        foreign = (foreign_uid, foreign_gid)
        cap_setuid, cap_setgid = openvmm_inherits_identity_capabilities()
        other_groups = openvmm_other_groups()
        # Guest root keeps its privileges only when OpenVMM can assume the
        # export owner identity: CAP_SETUID when the export UID differs,
        # CAP_SETGID when the export GID differs or supplementary groups
        # must be dropped.
        root_allowed, foreign_allowed = filesystem_owner_expectations(
            export_uid=owner[0],
            export_gid=owner[1],
            openvmm_uid=os.geteuid(),
            openvmm_gid=os.getegid(),
            cap_setuid=cap_setuid,
            cap_setgid=cap_setgid,
            supplementary_groups=other_groups,
        )
        # Group dropping is only the blocker when there are supplementary
        # groups to drop and CAP_SETGID is absent; otherwise the blocker is
        # a mismatched export UID/GID.
        groups_blocked = bool(other_groups) and not cap_setgid
        identity_cause = (
            "drop its supplementary groups"
            if groups_blocked
            else "assume the export's identity"
        )
        print(
            "OpenVMM "
            + ("can" if (cap_setuid and cap_setgid) else "cannot")
            + " assume other host identities"
            + ("" if root_allowed else f" or {identity_cause}")
            + "; guest root must "
            + ("own its files" if root_allowed else "fail with EPERM")
            + ", and foreign guest callers must "
            + ("own their files" if foreign_allowed else "fail with EPERM")
        )
        # Squashed guest root must not give a file one of OpenVMM's groups.
        chgrp_targets = [group for group in other_groups if group != owner[1]]
        run_guest_script(
            boot_command(f"/mnt/share,{root},rw"),
            _render_script(
                "filesystem-owner.sh.in",
                OWNER=f"{owner[0]}:{owner[1]}",
                FOREIGN=f"{foreign[0]}:{foreign[1]}",
                OTHER_GROUP=str(chgrp_targets[0]) if chgrp_targets else "none",
                ROOT_RESULT="owned" if root_allowed else "denied",
                FOREIGN_RESULT="owned" if foreign_allowed else "denied",
            ),
            FILESYSTEM_OWNER_MARKER,
            timeout=timeout,
            log_path=output_dir / "filesystem-owner.log",
        )

        for path in (root, *root.rglob("*")):
            status = path.lstat()
            if status.st_uid == 0 or status.st_gid == 0:
                raise RuntimeError(f"the guest left a root-owned host file: {path}")
        if not root_allowed:
            if any(root.iterdir()):
                raise RuntimeError(
                    "a guest caller wrote to the share although OpenVMM cannot "
                    + identity_cause
                )
        else:
            status = (root / "root-file").lstat()
            if (status.st_uid, status.st_gid) != owner or not status.st_mode & S_ISUID:
                raise RuntimeError(
                    "guest root did not leave a setuid file owned by the share owner"
                )
            if os.path.lexists(root / "device"):
                raise RuntimeError("squashed guest root created a device node")
            if foreign_allowed:
                status = (root / "foreign" / "nested" / "file").lstat()
                if (status.st_uid, status.st_gid) != foreign:
                    raise RuntimeError("a foreign guest caller does not own its file")
            elif os.path.lexists(root / "foreign"):
                raise RuntimeError("a foreign guest caller wrote as OpenVMM")

        filesystem_root = Path("/")
        status = filesystem_root.stat()
        if status.st_uid == 0 and status.st_gid == 0:
            assert_rejected(
                "root-owned",
                f"/mnt/share,{filesystem_root},ro",
                b"must not be owned by UID 0 or GID 0",
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


def _measure_restore(
    command: Sequence[str],
    *,
    context: str,
    environment: dict[str, str],
    timeout: float,
    log_path: Path,
    failure_marker: bytes,
) -> bytes:
    """Restore a snapshot whose post-restore script ends with the restore
    marker and a queued guest exit, and return the complete output."""
    try:
        measure_once(
            command,
            environment=environment,
            timeout=timeout,
            marker=RESTORE_MARKER,
            marker_must_be_line=True,
            guest_exit_prequeued=True,
            log_path=log_path,
            failure_marker=failure_marker,
        )
    except GuestFailureReported as error:
        raise RuntimeError(
            f"{context}: guest reported {error.line}\n"
            f"--- OpenVMM output ---\n{error.output_tail}"
        ) from error
    return log_path.read_bytes()


def _check_restore_warp(output: bytes, *, processors: int, context: str) -> None:
    """Validate the warp probe that a post-restore script ran."""
    if not contains_output_line(output, WARP_PROBE_COMPLETION_MARKER):
        raise RuntimeError(f"{context}: the guest did not finish its warp probe")
    check_warp_probe(
        output.decode("utf-8", "replace"),
        cpus=processors,
        context=context,
        rounds=warp_rounds(processors),
    )


def post_restore_checks() -> str:
    """Return the guest checks that run after every restore of the restore
    scenarios: the warp probe, then the time ABI status, which waits for the
    restore's deferred checks."""
    return warp_probe_script() + status_script()


def _check_restore_status(
    output: bytes,
    command: Sequence[str],
    *,
    processors: int,
    context: str,
) -> dict[str, str]:
    """Validate the restore line that the post-restore status query printed."""
    monitor = TimeAbiMonitor(command)
    try:
        monitor.feed(output)
        monitor.finish()
        monitor.require_status("the restore checks finished")
        return monitor.require_restore(
            "the restore checks finished",
            online_cpus=processors,
            generation=RESTORED_GENERATION,
        )
    except TimeAbiFailure as error:
        raise RuntimeError(f"{context}: {error}") from error


def run_smp_snapshot(
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
    counts = list(dict.fromkeys(processor_counts))
    for processors in counts:
        # The first snapshot is restored twice to prove that a restore leaves
        # it reusable; the warp probe runs after every restore.
        restores = 2 if processors == counts[0] else 1
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
                timeout=timeout,
                processors=processors,
                post_restore_script=smp_probe_script(processors, exit_guest=False)
                + post_restore_checks(),
                log_path=output_dir / f"smp-snapshot-{processors}-capture.log",
            )
            fingerprint = _snapshot_fingerprint(snapshot_path)
            for restore_index in range(restores):
                context = f"{processors}-vCPU SMP restore {restore_index}"
                restore_command = snapshot_restore_command(
                    executable,
                    backend,
                    snapshot_path,
                    processors=processors,
                )
                output = _measure_restore(
                    restore_command,
                    context=context,
                    environment=_restore_environment(),
                    timeout=timeout,
                    log_path=output_dir
                    / f"smp-snapshot-{processors}-restore-{restore_index}.log",
                    failure_marker=WARP_PROBE_FAILURE_MARKER,
                )
                _check_restore_warp(output, processors=processors, context=context)
                _check_restore_status(
                    output, restore_command, processors=processors, context=context
                )
                if _snapshot_fingerprint(snapshot_path) != fingerprint:
                    raise RuntimeError(f"{context} modified snapshot artifacts")


def _declared_cpu_profiles(output: bytes) -> list[str]:
    """Return the CPU profile of each time ABI rates declaration in OpenVMM's
    log, in order: one per partition."""
    return [match.group(1).decode() for match in DECLARED_CPU_PROFILE.finditer(output)]


def run_cpu_profile_fallback(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Exercise ``--cpu-profile auto``'s fallback to a host profile on a host
    that a built-in profile serves, through the ``host-cpu-unknown`` test hook.

    The cold boot falls back to the host's host profile, warns once with
    OpenVMM's marker and a link to the CPU profile request form, and boots a
    guest that passes its time ABI checks on that profile. The guest's snapshot
    records the profile, which a restore with ``auto`` takes from the snapshot
    without falling back again, and the restored guest passes the warp probe
    and its restore checks."""
    cold_boot = "CPU profile fallback cold boot"
    context = "CPU profile fallback restore"
    with tempfile.TemporaryDirectory(prefix="nvx-cpu-profile-fallback-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        boot_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            processors=1,
        )
        capture_log = output_dir / "cpu-profile-fallback-capture.log"
        capture_snapshot(
            [
                *boot_command,
                *CPU_PROFILE_FALLBACK_HOOK,
                "--snapshot-destination",
                str(snapshot_path),
            ],
            snapshot_path,
            timeout=timeout,
            processors=1,
            post_restore_script=post_restore_checks(),
            log_path=capture_log,
            log_filter=CPU_PROFILE_FALLBACK_LOG_FILTER,
        )
        capture = capture_log.read_bytes()
        try:
            profile = parse_cpu_profile_fallback(capture.decode("utf-8", "replace"))
        except ValueError as error:
            raise RuntimeError(f"{cold_boot}: {error}") from error
        if profile is None:
            raise RuntimeError(
                f"{cold_boot}: OpenVMM logged no {CPU_PROFILE_FALLBACK_MARKER} warning"
            )
        if CPU_PROFILE_REQUEST_URL.encode() not in capture:
            raise RuntimeError(
                f"{cold_boot}: the warning does not link to {CPU_PROFILE_REQUEST_URL}"
            )
        declared = _declared_cpu_profiles(capture)
        if declared != [profile]:
            raise RuntimeError(
                f"{cold_boot} declared CPU profiles {declared}, not {profile}"
            )

        restore_command = snapshot_restore_command(
            executable,
            backend,
            snapshot_path,
            processors=1,
        )
        environment = _restore_environment()
        environment["OPENVMM_LOG"] = TIME_ABI_RESTORE_LOG_FILTER
        output = _measure_restore(
            restore_command,
            context=context,
            environment=environment,
            timeout=timeout,
            log_path=output_dir / "cpu-profile-fallback-restore.log",
            failure_marker=WARP_PROBE_FAILURE_MARKER,
        )
        _check_restore_warp(output, processors=1, context=context)
        _check_restore_status(output, restore_command, processors=1, context=context)
        if CPU_PROFILE_FALLBACK_MARKER.encode() in output:
            raise RuntimeError(f"{context} fell back to a host profile again")
        declared = _declared_cpu_profiles(output)
        if declared != [profile]:
            raise RuntimeError(
                f"{context} declared CPU profiles {declared}, not the snapshot's "
                f"{profile}"
            )


def _restore_vp_bindings(output: bytes) -> list[int]:
    """Return the VP indices bound by one profiled OpenVMM process."""
    bound: list[int] = []
    thread_bind_records = 0
    for line in _output_lines(output):
        record = parse_snapshot_profile_line(line)
        if record is None or record["operation"] != "startup":
            continue
        phase = cast(str, record["phase"])
        if phase == "vp_thread_bind":
            thread_bind_records += 1
        elif phase == "vp_bind_bsp":
            bound.append(0)
        elif phase.startswith("vp_bind_ap_"):
            index = phase.removeprefix("vp_bind_ap_")
            if not index.isdecimal() or int(index) == 0:
                raise RuntimeError(f"malformed VP binding profile phase {phase!r}")
            bound.append(int(index))
    if thread_bind_records != 1:
        raise RuntimeError(
            "expected exactly one startup.vp_thread_bind profile record, "
            f"found {thread_bind_records}"
        )
    return sorted(bound)


def _restore_label(target: int | None) -> str:
    return "untargeted restore" if target is None else f"restore target {target}"


def _check_restore_vp_bindings(
    output: bytes,
    backend: str,
    *,
    target: int | None,
    capacity: int,
) -> None:
    # Only MSHV instantiates the VP prefix of an explicit restore target.
    # Untargeted MSHV restores and every KVM or WHP restore bind the capacity.
    expected = target if backend == "mshv" and target is not None else capacity
    bound = _restore_vp_bindings(output)
    if bound != list(range(expected)):
        raise RuntimeError(
            f"{_restore_label(target)} bound VPs {bound} on {backend}; "
            f"expected exactly VPs 0..{expected - 1} of capacity {capacity}"
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
    boot_online = 1
    cmdline = f"quiet loglevel=0 maxcpus={boot_online}"
    # The warp probe replaces the guest TSC warp guard: it measures the skew
    # between every pair of CPUs, including the ones the restore activated.
    script = _read_script("restore-processors.sh") + post_restore_checks()
    # The VP-binding lifecycle records identify the VPs that each restore
    # instantiates without changing restore behavior.
    environment = _restore_environment()
    environment["OPENVMM_LOG"] = TIME_ABI_RESTORE_LOG_FILTER
    environment[SNAPSHOT_PROFILE_ENV] = "1"
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
            timeout=timeout,
            processors=boot_online,
            post_restore_script=script,
            log_path=output_dir / "restore-processors-capture.log",
        )
        fingerprint = _snapshot_fingerprint(snapshot_path)
        # An untargeted restore keeps the captured boot-online prefix.
        targets: list[int | None] = [*dict.fromkeys(processor_counts), None]
        for target in targets:
            name = "untargeted" if target is None else str(target)
            online = boot_online if target is None else target
            restore_command = snapshot_restore_command(
                executable,
                backend,
                snapshot_path,
                processors=capacity,
                restore_processors=target,
            )
            output = _measure_restore(
                restore_command,
                context=_restore_label(target),
                environment=environment,
                timeout=timeout,
                log_path=output_dir / f"restore-processors-{name}.log",
                failure_marker=RESTORE_PROCESSORS_FAILURE_MARKER,
            )
            marker = f"NVX-RESTORE-PROCESSORS-OK count={online}".encode()
            if not contains_output_line(output, marker):
                raise RuntimeError(
                    f"{_restore_label(target)} did not report {marker.decode()!r}"
                )
            _check_restore_warp(
                output, processors=online, context=_restore_label(target)
            )
            _check_restore_status(
                output,
                restore_command,
                processors=online,
                context=_restore_label(target),
            )
            _check_restore_vp_bindings(
                output,
                backend,
                target=target,
                capacity=capacity,
            )
            if _snapshot_fingerprint(snapshot_path) != fingerprint:
                raise RuntimeError(
                    f"{_restore_label(target)} modified snapshot artifacts"
                )


def run_restore_downtime(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Restore snapshots after a downtime longer than the RCU stall timeout.

    Every snapshot is captured first, so one shared downtime window covers
    them all. The restored guest must finish its time ABI restore, report no
    RCU stall, and show that monotonic time advanced by the downtime.
    """
    cases = [
        (processors, expedited)
        for processors in RESTORE_DOWNTIME_PROCESSORS
        for expedited in (False, True)
    ]
    check = _render_script(
        "restore-downtime.sh.in",
        SETTLE_SECONDS=str(RESTORE_DOWNTIME_SETTLE_SECONDS),
        MIN_UPTIME_SECONDS=str(int(RESTORE_DOWNTIME_SECONDS)),
    )
    environment = {"OPENVMM_LOG": TIME_ABI_RESTORE_LOG_FILTER}
    with tempfile.TemporaryDirectory(prefix="nvx-restore-downtime-") as temporary:
        snapshots: list[tuple[str, int, Path]] = []
        for processors, expedited in cases:
            name = f"{processors}-vcpu" + ("-expedited" if expedited else "")
            cmdline = "quiet loglevel=0" + (
                " rcupdate.rcu_expedited=1" if expedited else ""
            )
            snapshot_path = Path(temporary) / name
            boot_command = workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                cmdline,
                processors=processors,
            )
            # The restored guest keeps running after the restore marker so the
            # harness can stage its downtime check; the post-restore status
            # query waits for the restore to finish first.
            capture_snapshot(
                [*boot_command, "--snapshot-destination", str(snapshot_path)],
                snapshot_path,
                timeout=timeout,
                processors=processors,
                teardown_mode="host-terminate",
                post_restore_script=post_restore_checks(),
                log_path=output_dir / f"restore-downtime-{name}-capture.log",
            )
            snapshots.append((name, processors, snapshot_path))
        restore_after = time.monotonic() + RESTORE_DOWNTIME_SECONDS
        for name, processors, snapshot_path in snapshots:
            time.sleep(max(0.0, restore_after - time.monotonic()))
            context = f"{name} restore after a {RESTORE_DOWNTIME_SECONDS:g} s downtime"
            with OpenvmmProcess(
                snapshot_restore_command(
                    executable,
                    backend,
                    snapshot_path,
                    processors=processors,
                ),
                output_dir / f"restore-downtime-{name}.log",
                environment=environment,
            ) as process:
                process.wait_for_line(RESTORE_MARKER, timeout)
                process.wait_for_time_abi("restore", TIME_ABI_RESTORE_FINISH_SECONDS)
                _stage_script(
                    process, RESTORE_DOWNTIME_PATH, "NVX_RESTORE_DOWNTIME", check
                )
                process.wait_for_line(RESTORE_DOWNTIME_COMPLETION_MARKER, timeout)
                result = process.wait(timeout)
            if result.returncode != 0:
                error = process.time_abi.exit_error(result.returncode)
                raise RuntimeError(f"{context}: {error}")
            _check_restore_warp(result.output, processors=processors, context=context)
            try:
                process.time_abi.require_status(context)
                process.time_abi.require_restore(
                    context, online_cpus=processors, generation=RESTORED_GENERATION
                )
            except TimeAbiFailure as error:
                raise RuntimeError(f"{context}: {error}") from error


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


CANCELED_CAPTURE_RCU_PREFIX = b"NVX-CANCELED-CAPTURE-RCU "
CANCELED_CAPTURE_DONE_MARKER = b"NVX-CANCELED-CAPTURE-DONE"
RCU_STALL_SUPPRESS_PATH = "/sys/module/rcupdate/parameters/rcu_cpu_stall_suppress"


def canceled_capture_checks() -> str:
    """Return the guest checks after a snapshot request that OpenVMM released:
    the time ABI status, then whether RCU stall detection is still suppressed."""
    rcu = CANCELED_CAPTURE_RCU_PREFIX.decode()
    done = CANCELED_CAPTURE_DONE_MARKER.decode()
    # A quote pair splits each marker, so the console's echo of the command,
    # which the tty may wrap, never contains it.
    return (
        status_script()
        + f'echo "{rcu[:4]}""{rcu[4:]}$(cat {RCU_STALL_SUPPRESS_PATH})"; '
        + f'echo "{done[:4]}""{done[4:]}"\n'
    )


def check_canceled_capture(output: bytes, command: Sequence[str]) -> None:
    """Check that a released snapshot request left the source as it was.

    The snapshot agent saves and overrides the stall detectors' settings
    before the request; when the request returns in the source, it removes its
    barriers and restores the saved values (nvx-time cancel-capture). The
    source then reports a passing status at generation 0 with no restore, and
    RCU stall detection is no longer suppressed.
    """
    context = "after the released snapshot request"
    monitor = TimeAbiMonitor(command)
    try:
        monitor.feed(output)
        monitor.finish()
        monitor.require_status("its checks finished")
        monitor.require_boot("its checks finished")
    except TimeAbiFailure as error:
        raise RuntimeError(f"{context}: {error}") from error
    if monitor.restores:
        raise RuntimeError(
            f"{context}: nvx-time status reported a restore at "
            f"generation={monitor.restores[-1].get('generation')}, but the "
            "request returned in the source"
        )
    try:
        value = _single_marker_value(output, CANCELED_CAPTURE_RCU_PREFIX)
    except RuntimeError as error:
        raise RuntimeError(f"{context}: {error}") from error
    if value != b"0":
        raise RuntimeError(
            f"{context}: rcu_cpu_stall_suppress is {value.decode(errors='replace')!r}, "
            "not 0: the snapshot agent did not restore the value it saved"
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
        process.wait_for_line(no_destination_marker, timeout)
        checks_start = len(process.output)
        process.send_bytes(canceled_capture_checks().encode())
        process.wait_for_line(
            CANCELED_CAPTURE_DONE_MARKER, max(timeout, STATUS_TIMEOUT_SECONDS)
        )
        check_canceled_capture(process.output[checks_start:], no_destination_command)
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
                _read_script("snapshot-core.sh"),
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
                # The guest's shell is on this virtio console, so it answers
                # the cold boot's status query there.
                console = TcpConsole.connect(
                    address,
                    timeout,
                    monitor=TimeAbiMonitor(capture_command),
                    time_abi_status=True,
                )
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
            restore_command = snapshot_restore_command(
                executable, backend, snapshot_path
            )
            with OpenvmmProcess(
                restore_command,
                output_dir / f"console-snapshot-restore-{restore_index}-process.log",
            ) as process:
                try:
                    console = TcpConsole.connect(
                        address, timeout, monitor=TimeAbiMonitor(restore_command)
                    )
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
        server.join(timeout=NETWORK_SERVER_JOIN_TIMEOUT_SECONDS)
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
        if os.path.lexists(read_only_root / "link"):
            raise RuntimeError("read-only virtio-fs mount accepted a symbolic link")

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
        assert_guest_symlink(live_root / "handle-link", "open-handle")
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

        if sys.platform == "linux":
            # The snapshot contract binds the ownership mode of the share.
            owner_command = snapshot_restore_command(executable, backend, live_snapshot)
            owner_command.extend(
                ("--mount", f"/mnt/share,{live_root},rw", "--mount-owner", "caller")
            )
            _expect_process_failure(
                owner_command,
                output_dir / "filesystem-live-owner-mismatch.log",
                timeout,
                b"does not match the snapshot contract",
            )

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


def _tree_contents(root: Path) -> dict[str, bytes | None]:
    """Return the regular-file contents and directory names below `root`."""
    contents: dict[str, bytes | None] = {}
    for directory, names, files in os.walk(root):
        for name in names:
            contents[(Path(directory) / name).relative_to(root).as_posix()] = None
        for name in files:
            path = Path(directory) / name
            contents[path.relative_to(root).as_posix()] = path.read_bytes()
    return contents


# The guest binds each child of the aggregate at its target with its mode, and
# each file child at a file.
_SHARE_TOKENS = (
    "nvx_share=0,/workspace,rw nvx_share=1,/opt/hostedtoolcache,ro "
    "nvx_share=2,/srv/data,rw nvx_share=3,/config/settings.json,ro,file "
    "nvx_share=4,/results/output.txt,rw,file"
)


def _shared_files(files: Path) -> tuple[Path, Path]:
    """Return the read-only settings file and the read-write output file that
    the filesystem-shares scenario shares from `files`, each beside a file
    that it does not share."""
    return files / "config" / "settings.json", files / "results" / "output.txt"


def _aggregate_arguments(
    workspace: Path,
    toolcache: Path,
    data: Path,
    files: Path,
    names: tuple[str, str, str, str, str] = ("0", "1", "2", "3", "4"),
) -> tuple[str, ...]:
    """Return the OpenVMM options that share the read-write workspace, the
    read-only tool cache, the data directory, and the settings and output
    files below `files` as the children `names` of one aggregate. The data
    directory's root hides everything but `public` and `notes one.txt`, and
    only the notes are writable. The settings file is read-only and the output
    file read-write, and neither shares its directory."""
    workspace_name, toolcache_name, data_name, settings_name, output_name = names
    settings, output = _shared_files(files)
    return (
        "--mount-aggregate",
        "/run/nvx/shares",
        "--mount-child",
        f"{workspace_name},{workspace},rw",
        "--mount-child",
        f"{toolcache_name},{toolcache},ro",
        "--mount-child",
        f"{data_name},{data},rw",
        "--mount-child",
        f"{settings_name},{settings},ro,file",
        "--mount-child",
        f"{output_name},{output},rw,file",
        "--mount-deny",
        str(data),
        "--mount-allow",
        str(data / "public"),
        "--mount-allow",
        str(data / "notes one.txt"),
        "--mount-write",
        str(data / "notes one.txt"),
    )


def _shares_command(
    executable: Path,
    backend: str,
    kernel: Path,
    initrd: Path,
    memory_mib: int,
    workspace: Path,
    toolcache: Path,
    data: Path,
    files: Path,
) -> list[str]:
    command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        f"quiet loglevel=0 {_SHARE_TOKENS}",
    )
    command.extend(_aggregate_arguments(workspace, toolcache, data, files))
    return command


def run_filesystem_shares(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Attach a read-write workspace, a read-only tool cache, a data directory
    with a hidden root, and a read-only and a read-write file, each without
    its directory, together, as the children of one aggregate."""
    with tempfile.TemporaryDirectory(prefix="nvx-filesystem-shares-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        toolcache = root / "toolcache"
        data = root / "data"
        files = root / "files"
        settings, output = _shared_files(files)
        (workspace / "secrets").mkdir(parents=True)
        (toolcache / "tools").mkdir(parents=True)
        (toolcache / "credentials").mkdir()
        (data / "public").mkdir(parents=True)
        settings.parent.mkdir(parents=True)
        output.parent.mkdir(parents=True)
        (workspace / "seed").write_bytes(b"NVX-WORKSPACE")
        (workspace / "secrets" / "token").write_bytes(b"NVX-SECRET")
        (toolcache / "seed").write_bytes(b"NVX-TOOLCACHE")
        (toolcache / "tools" / "node").write_bytes(b"NVX-TOOL")
        (toolcache / "credentials" / "token").write_bytes(b"NVX-CREDENTIAL")
        (data / "public" / "readme").write_bytes(b"NVX-PUBLIC")
        (data / "notes one.txt").write_bytes(b"NVX-NOTES\n")
        (data / "private.txt").write_bytes(b"NVX-PRIVATE")
        settings.write_bytes(b"NVX-SETTINGS")
        (settings.parent / "secret.json").write_bytes(b"NVX-CONFIG-SECRET")
        output.write_bytes(b"NVX-OUTPUT\n")
        (output.parent / "other.txt").write_bytes(b"NVX-OTHER")
        toolcache_contents = _tree_contents(toolcache)
        files_contents = _tree_contents(files)

        command = _shares_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            workspace,
            toolcache,
            data,
            files,
        )
        command.extend(
            (
                "--mount-deny",
                str(workspace / "secrets"),
                "--mount-deny",
                str(toolcache / "credentials"),
            )
        )
        data_contents = _tree_contents(data)
        run_guest_script(
            command,
            _read_script("filesystem-shares.sh"),
            FILESYSTEM_SHARES_MARKER,
            timeout=timeout,
            log_path=output_dir / "filesystem-shares.log",
        )
        if (workspace / "from-guest").read_bytes() != b"NVX-GUEST-WRITE\n":
            raise RuntimeError("read-write share did not accept a guest write")
        if not (workspace / "guest-directory").is_dir():
            raise RuntimeError("read-write share did not accept a guest directory")
        assert_guest_symlink(workspace / "toolcache-seed", "/opt/hostedtoolcache/seed")
        if (workspace / "linked").exists():
            raise RuntimeError("a hard link joined two children of the aggregate")
        if _tree_contents(toolcache) != toolcache_contents:
            raise RuntimeError("guest modified the read-only share")
        if (workspace / "secrets" / "token").read_bytes() != b"NVX-SECRET":
            raise RuntimeError("guest modified a denied path")
        data_contents["notes one.txt"] = b"NVX-NOTES\nNVX-GUEST-NOTE\n"
        if _tree_contents(data) != data_contents:
            raise RuntimeError(
                "guest changed the data share beyond its one writable path"
            )
        files_contents["results/output.txt"] = b"NVX-GUEST-OUTPUT\n"
        if _tree_contents(files) != files_contents:
            raise RuntimeError(
                "guest changed the shared files' directories, the read-only "
                "file, or the read-write file other than as it wrote"
            )

        nested = workspace / "nested"
        nested.mkdir()
        third = root / "third"
        third.mkdir()
        for name, arguments, expected in (
            (
                "repeated-mount",
                (
                    "--mount",
                    f"/workspace,{workspace},rw",
                    "--mount",
                    f"/opt/hostedtoolcache,{toolcache},ro",
                ),
                b"cannot be used multiple times",
            ),
            (
                "overlapping-children",
                (
                    "--mount-aggregate",
                    "/run/nvx/shares",
                    "--mount-child",
                    f"0,{workspace},rw",
                    "--mount-child",
                    f"1,{nested},ro",
                ),
                b"must not overlap",
            ),
            # A file overlaps a directory child that contains it.
            (
                "file-inside-child",
                (
                    "--mount-aggregate",
                    "/run/nvx/shares",
                    "--mount-child",
                    f"0,{settings.parent},rw",
                    "--mount-child",
                    f"1,{settings},ro,file",
                ),
                b"must not overlap",
            ),
            # The `file` flag, not the host object, decides a child's kind, so
            # a directory never stands in for a file, nor a file for a
            # directory.
            (
                "directory-as-file",
                (
                    "--mount-aggregate",
                    "/run/nvx/shares",
                    "--mount-child",
                    f"0,{settings.parent},ro,file",
                ),
                b"not a regular file",
            ),
            (
                "file-as-directory",
                (
                    "--mount-aggregate",
                    "/run/nvx/shares",
                    "--mount-child",
                    f"0,{settings},ro",
                ),
                b"not a plain directory",
            ),
            # The guest mounts the root of a lone share, which is a directory.
            (
                "file-mount",
                ("--mount", f"/config/settings.json,{settings},ro"),
                b"not a plain directory",
            ),
        ):
            invalid = workload_boot_command(
                executable, backend, kernel, initrd, memory_mib, "quiet loglevel=0"
            )
            invalid.extend(arguments)
            _expect_boot_failure(
                invalid, output_dir / f"filesystem-shares-{name}.log", timeout, expected
            )
        for name, arguments, expected in (
            (
                "mount-and-aggregate",
                ("--mount", f"/third,{third},ro"),
                b"cannot be used with",
            ),
            ("relative-deny", ("--mount-deny", "secrets"), b"absolute host path"),
            # A file child's mode applies to the whole file.
            ("file-policy", ("--mount-deny", str(settings)), b"exposes the file"),
        ):
            invalid = _shares_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                workspace,
                toolcache,
                data,
                files,
            )
            invalid.extend(arguments)
            _expect_boot_failure(
                invalid, output_dir / f"filesystem-shares-{name}.log", timeout, expected
            )

        # Every child belongs to the snapshot contract, in order.
        snapshot = root / "snapshot"
        with OpenvmmProcess(
            [
                *_shares_command(
                    executable,
                    backend,
                    kernel,
                    initrd,
                    memory_mib,
                    workspace,
                    toolcache,
                    data,
                    files,
                ),
                "--snapshot-destination",
                str(snapshot),
            ],
            output_dir / "filesystem-shares-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-filesystem-shares",
                "NVX_FILESYSTEM_SHARES",
                _read_script("filesystem-shares-snapshot.sh"),
            )
            source = process.wait(timeout)
        source_lines = _output_lines(source.output)
        if source.returncode != 0 or (
            source_lines.count(FILESYSTEM_SHARES_BEFORE_MARKER) != 1
        ):
            raise RuntimeError("aggregate snapshot capture failed")
        if FILESYSTEM_SHARES_AFTER_MARKER in source_lines:
            raise RuntimeError("aggregate snapshot source crossed the capture boundary")
        fingerprint = _snapshot_fingerprint(snapshot)

        aggregate = _aggregate_arguments(workspace, toolcache, data, files)
        for name, arguments, expected in (
            (
                "missing-share",
                (
                    "--mount-aggregate",
                    "/run/nvx/shares",
                    "--mount-child",
                    f"0,{workspace},rw",
                    "--mount-child",
                    f"1,{toolcache},ro",
                ),
                b"do not match the snapshot contract",
            ),
            (
                "swapped-shares",
                _aggregate_arguments(toolcache, workspace, data, files),
                b"do not match the snapshot contract",
            ),
            # The children's names pin the targets where the guest bound them.
            (
                "renamed-share",
                _aggregate_arguments(
                    workspace, toolcache, data, files, ("0", "1", "data", "3", "4")
                ),
                b"do not match the snapshot contract",
            ),
            (
                "single-mount",
                ("--mount", f"/workspace,{workspace},rw"),
                b"--mount-aggregate and --mount-child options",
            ),
        ):
            invalid = snapshot_restore_command(executable, backend, snapshot)
            invalid.extend(arguments)
            _expect_process_failure(
                invalid,
                output_dir / f"filesystem-shares-{name}.log",
                timeout,
                expected,
                (FILESYSTEM_SHARES_AFTER_MARKER,),
            )
        restore = snapshot_restore_command(executable, backend, snapshot)
        restore.extend(aggregate)
        with OpenvmmProcess(
            restore, output_dir / "filesystem-shares-restore.log"
        ) as process:
            process.wait_for(FILESYSTEM_SHARES_AFTER_MARKER, timeout)
            restored = process.wait(timeout)
        if restored.returncode != 0:
            raise RuntimeError(
                f"aggregate snapshot restore exited with {restored.returncode}"
            )
        if _snapshot_fingerprint(snapshot) != fingerprint:
            raise RuntimeError("aggregate snapshot restore modified snapshot artifacts")
        if (workspace / "journal").read_bytes() != b"NVX-BEFORENVX-AFTER":
            raise RuntimeError("read-write share did not resume after restore")
        if output.read_bytes() != b"NVX-FILE-BEFORENVX-FILE-AFTER":
            raise RuntimeError("read-write file did not resume after restore")
        if _tree_contents(toolcache) != toolcache_contents:
            raise RuntimeError("restored guest modified the read-only share")
        if settings.read_bytes() != b"NVX-SETTINGS":
            raise RuntimeError("restored guest modified the read-only file")

        # A snapshot pins the object that a file child names, so a file that
        # the host puts in its place, such as by saving over it, is another.
        replacement = settings.with_name("settings.json.new")
        replacement.write_bytes(b"NVX-SETTINGS")
        replacement.replace(settings)
        replaced = snapshot_restore_command(executable, backend, snapshot)
        replaced.extend(aggregate)
        _expect_process_failure(
            replaced,
            output_dir / "filesystem-shares-replaced-file.log",
            timeout,
            b"root identity does not match",
            (FILESYSTEM_SHARES_AFTER_MARKER,),
        )


def _policy_arguments(share: Path) -> tuple[str, ...]:
    """Return the access policy of the filesystem-policy scenario's share."""
    return (
        "--mount-deny",
        str(share / "logs"),
        "--mount-allow",
        str(share / "logs" / "payloads"),
        "--mount-deny",
        str(share / "logs" / "payloads" / "private"),
        "--mount-write",
        str(share / "out"),
        "--mount-write",
        str(share / "build.log"),
    )


def _policy_protected_contents(share: Path) -> dict[str, bytes | None]:
    """Return what the policy must keep unchanged: every entry of the share
    other than the writable `out` and `build.log`, and everything below the
    denied `logs`."""
    contents: dict[str, bytes | None] = {
        name: (share / name).read_bytes() if (share / name).is_file() else None
        for name in os.listdir(share)
        if name not in ("out", "build.log")
    }
    contents.update(
        {
            f"logs/{name}": value
            for name, value in _tree_contents(share / "logs").items()
        }
    )
    return contents


def run_filesystem_policy(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Narrow a read-write share's writes and expose a path in a hidden subtree.

    The share's `logs` directory is denied except for `logs/payloads`, inside
    which `logs/payloads/private` is denied again, and only `out` and
    `build.log` are writable.
    """
    with tempfile.TemporaryDirectory(prefix="nvx-filesystem-policy-") as temporary:
        root = Path(temporary)
        share = root / "share"
        for directory in ("out", "logs/gateway", "logs/payloads/private"):
            (share / directory).mkdir(parents=True)
        for name, contents in (
            ("seed", b"NVX-SEED"),
            ("build.log", b"NVX-LOG"),
            ("logs/secret", b"NVX-SECRET"),
            ("logs/gateway/log", b"NVX-GATEWAY"),
            ("logs/payloads/payload", b"NVX-PAYLOAD"),
            ("logs/payloads/private/key", b"NVX-KEY"),
        ):
            (share / name).write_bytes(contents)
        protected = _policy_protected_contents(share)
        mount = f"/workspace,{share},rw"

        def boot_command(mode: str = "rw") -> list[str]:
            return workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                mount=f"/workspace,{share},{mode}",
            )

        run_guest_script(
            [*boot_command(), *_policy_arguments(share)],
            _read_script("filesystem-policy.sh"),
            FILESYSTEM_POLICY_MARKER,
            timeout=timeout,
            log_path=output_dir / "filesystem-policy.log",
        )
        if (share / "out" / "from-guest").read_bytes() != b"NVX-GUEST-WRITE\n":
            raise RuntimeError("writable path did not accept a guest write")
        if (share / "out" / "renamed").read_bytes() != b"nested\n":
            raise RuntimeError("writable path did not accept a guest rename")
        if (share / "build.log").read_bytes() != b"NVX-LOGAPPEND":
            raise RuntimeError("writable file did not accept a guest append")
        assert_guest_symlink(share / "out" / "seed-link", "../seed")
        if _policy_protected_contents(share) != protected:
            raise RuntimeError("guest modified the share outside its writable paths")

        for name, arguments, expected in (
            (
                "read-only-share",
                (
                    *boot_command("ro"),
                    "--mount-write",
                    str(share / "out"),
                ),
                b"read-only microVM filesystem cannot have writable paths",
            ),
            (
                "allow-outside-deny",
                (*boot_command(), "--mount-allow", str(share / "out")),
                b"must be inside a denied path",
            ),
            (
                "hidden-write",
                (
                    *boot_command(),
                    "--mount-deny",
                    str(share / "logs"),
                    "--mount-write",
                    str(share / "logs" / "gateway"),
                ),
                b"hidden by a denied path",
            ),
            (
                "overlapping-write",
                (
                    *boot_command(),
                    "--mount-write",
                    str(share / "logs"),
                    "--mount-write",
                    str(share / "logs" / "gateway"),
                ),
                b"writable paths overlap",
            ),
            (
                "root-write",
                (*boot_command(), "--mount-write", str(share)),
                b"cannot be the complete filesystem export",
            ),
        ):
            _expect_boot_failure(
                list(arguments),
                output_dir / f"filesystem-policy-{name}.log",
                timeout,
                expected,
            )

        # The policy belongs to the snapshot contract, and the restored guest
        # keeps both its writable handle and the policy.
        snapshot = root / "snapshot"
        with OpenvmmProcess(
            [
                *boot_command(),
                *_policy_arguments(share),
                "--snapshot-destination",
                str(snapshot),
            ],
            output_dir / "filesystem-policy-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-filesystem-policy",
                "NVX_FILESYSTEM_POLICY",
                _read_script("filesystem-policy-snapshot.sh"),
            )
            source = process.wait(timeout)
        source_lines = _output_lines(source.output)
        if source.returncode != 0 or (
            source_lines.count(FILESYSTEM_POLICY_BEFORE_MARKER) != 1
        ):
            raise RuntimeError("share access-policy snapshot capture failed")
        if FILESYSTEM_POLICY_AFTER_MARKER in source_lines:
            raise RuntimeError(
                "share access-policy snapshot source crossed the capture boundary"
            )
        fingerprint = _snapshot_fingerprint(snapshot)

        for name, arguments in (
            ("missing-policy", ()),
            (
                "changed-policy",
                (
                    "--mount-deny",
                    str(share / "logs"),
                    "--mount-write",
                    str(share / "out"),
                ),
            ),
        ):
            invalid = snapshot_restore_command(executable, backend, snapshot)
            invalid.extend(("--mount", mount, *arguments))
            _expect_process_failure(
                invalid,
                output_dir / f"filesystem-policy-{name}.log",
                timeout,
                b"does not match the snapshot contract",
                (FILESYSTEM_POLICY_AFTER_MARKER,),
            )
        restore = snapshot_restore_command(executable, backend, snapshot)
        restore.extend(("--mount", mount, *_policy_arguments(share)))
        with OpenvmmProcess(
            restore, output_dir / "filesystem-policy-restore.log"
        ) as process:
            process.wait_for(FILESYSTEM_POLICY_AFTER_MARKER, timeout)
            restored = process.wait(timeout)
        if restored.returncode != 0:
            raise RuntimeError(
                f"share access-policy snapshot restore exited with {restored.returncode}"
            )
        if _snapshot_fingerprint(snapshot) != fingerprint:
            raise RuntimeError(
                "share access-policy snapshot restore modified snapshot artifacts"
            )
        if (share / "out" / "journal").read_bytes() != b"NVX-BEFORENVX-AFTER":
            raise RuntimeError("writable path did not resume after restore")
        if _policy_protected_contents(share) != protected:
            raise RuntimeError(
                "restored guest modified the share outside its writable paths"
            )


def _expect_boot_failure(
    command: list[str], log_path: Path, timeout: float, expected: bytes
) -> None:
    with OpenvmmProcess(command, log_path) as process:
        result = process.wait(timeout)
    if (
        result.returncode == 0
        or expected not in result.output
        or BOOT_MARKER in result.output
    ):
        raise RuntimeError(
            f"OpenVMM did not reject the invalid configuration before boot: {command}"
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
            expected_value = str(value).encode()
            marker = (
                SCRATCH_FRESH_VALUE_PREFIX + expected_value + SCRATCH_FRESH_VALUE_SUFFIX
            )
            with OpenvmmProcess(
                fresh_restore_command(scratch),
                output_dir / f"scratch-fresh-restore-{restore_index}.log",
            ) as process:
                process.wait_for(marker, timeout)
                restored = process.wait(timeout)
            restored_lines = _output_lines(restored.output)
            restored_value = _single_framed_marker_value(
                restored.output,
                SCRATCH_FRESH_VALUE_PREFIX,
                SCRATCH_FRESH_VALUE_SUFFIX,
            )
            if restored.returncode != 0 or (
                restored_lines.count(SCRATCH_FRESH_POST_MARKER) != 1
                or restored_value != expected_value
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


def _snapshot_tier_kinds(tier: str) -> tuple[bool, bool, bool]:
    if tier not in ("platform", "workload-start", "instance-checkpoint"):
        raise ValueError(f"unsupported snapshot tier {tier!r}")
    return tier == "platform", tier == "workload-start", tier == "instance-checkpoint"


def _snapshot_tier_script(tier: str) -> str:
    platform, workload_start, instance_checkpoint = _snapshot_tier_kinds(tier)
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
printf 'restored-direct-claimed\\n' >/run/nvx/scratch/direct-claimed
sync
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
        # After the tier's assertions, the restored guest asks nvx-time status,
        # which waits for the restore's deferred checks, the debug kernel's
        # watchdogs among them, and prints the restore line; the script turns
        # off set -e first, so a failing status still reports its exit status.
        # It then waits for one byte from the host before nvx-exit, so the
        # query's lines reach the virtio console before the VM stops.
        STATUS_QUERY=status_script().rstrip("\n"),
    )


def _snapshot_storage_policy(tier: str) -> tuple[str, ...]:
    if tier == "platform":
        return ()
    if tier == "workload-start":
        generation = "00112233445566778899aabbccddee01"
        restore_mode = "private-copy"
    elif tier == "instance-checkpoint":
        generation = "00112233445566778899aabbccddee02"
        restore_mode = "direct-claimed"
    else:
        raise ValueError(f"unsupported snapshot tier {tier!r}")
    return (
        "--snapshot-block-identity",
        "generation",
        "--snapshot-generation-id",
        generation,
        "--snapshot-scratch-restore-mode",
        restore_mode,
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
    platform, workload_start, instance_checkpoint = _snapshot_tier_kinds(tier)

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
                *_snapshot_storage_policy(tier),
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
                # The guest's shell is on this virtio console, so it answers
                # the cold boot's status query there.
                console = TcpConsole.connect(
                    address,
                    timeout,
                    monitor=TimeAbiMonitor(capture_command),
                    time_abi_status=True,
                )
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
        fingerprint = _scratch_snapshot_fingerprint(snapshot)

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
                console = TcpConsole.connect(
                    address, timeout, monitor=TimeAbiMonitor(restore_command)
                )
                console.send_bytes(b"Z")
                console.wait_for(repair_marker, timeout)
                if workload_start:
                    console.wait_for(workload_marker, timeout)
                console.wait_for(released_marker, timeout)
                console.wait_for(expected_layer, timeout)
                # The guest's status query waits for the restore's deferred
                # checks. It then waits for one byte from here before nvx-exit,
                # so the query's lines reach the console before the VM stops.
                console.wait_for_time_abi_status(STATUS_TIMEOUT_SECONDS)
                console.send_bytes(b"Z")
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
        # The restored guest's status query, after the tier's assertions, must
        # report a passing restore line, so the deferred restore checks ran.
        _check_restore_status(
            restore_console,
            restore_command,
            processors=int(restore_command[restore_command.index("--processors") + 1]),
            context=f"{tier} restore",
        )
        restored_fingerprint = _scratch_snapshot_fingerprint(snapshot)
        if restored_fingerprint[:3] != fingerprint[:3]:
            raise RuntimeError(f"{tier} restore modified snapshot payloads")
        if instance_checkpoint:
            if restored_fingerprint[3] == fingerprint[3]:
                raise RuntimeError(
                    "instance-checkpoint restore did not modify direct-claimed scratch"
                )
        elif restored_fingerprint != fingerprint:
            raise RuntimeError(f"{tier} restore modified reusable snapshot scratch")

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
                    timeout_console = TcpConsole.connect(
                        address, timeout, monitor=TimeAbiMonitor(timeout_command)
                    )
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


def require_debug_kernel(config: Path) -> None:
    """Fail unless a kernel config enables the debug kernel's detectors.

    The guest's C11 check passes vacuously on a kernel without them, so a
    production kernel would otherwise pass the debug-kernel job.
    """
    lines = set(
        require_file(config, "microVM debug kernel config")
        .read_text(encoding="utf-8")
        .splitlines()
    )
    missing = [
        option
        for option in KernelBuildConstants.DEBUG_WATCHDOG_CONFIG
        if option not in lines
    ]
    if missing:
        raise ScriptError(
            f"{config} is not the CI debug kernel config; it lacks "
            + ", ".join(missing)
        )


def run(args: argparse.Namespace) -> int:
    validate_openvmm_test_backend(args.backend)
    descriptor = guest_descriptor(args.guest)
    if args.memory_mib is None:
        args.memory_mib = descriptor.default_memory_mib
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    debug_kernel = getattr(args, "debug_kernel", False)
    if debug_kernel:
        require_debug_kernel(artifact_path(KernelBuildConstants.DEBUG_CONFIG_NAME))
        kernel = require_file(
            artifact_path(KernelBuildConstants.DEBUG_BINARY_NAME),
            "microVM Linux debug kernel",
        )
    else:
        kernel = require_file(
            artifact_path(KernelBuildConstants.BINARY_NAME),
            "microVM Linux direct kernel",
        )
    initrd = require_file(
        artifact_path(descriptor.initramfs_name),
        f"microVM {descriptor.distribution} initramfs",
    )
    output_dir: Path = args.output_dir
    output_dir.mkdir(parents=True, exist_ok=True)
    unsupported_scenarios: set[str] = set()
    if descriptor.name == "ubuntu":
        unsupported_scenarios.update(UBUNTU_UNSUPPORTED_SCENARIOS)
    if not descriptor.sandbox_control:
        unsupported_scenarios.update(SANDBOX_CONTROL_SCENARIOS)
    if args.scenario is None:
        defaults = DEBUG_KERNEL_SCENARIOS if debug_kernel else MICROVM_TEST_SCENARIOS
        scenarios = tuple(
            scenario
            for scenario in defaults
            if scenario not in unsupported_scenarios
            and scenario not in PUBLIC_SANDBOX_SCENARIOS
        )
    else:
        scenarios = tuple(dict.fromkeys(args.scenario))
        unsupported = set(scenarios) & unsupported_scenarios
        if unsupported:
            guest_label = (
                "Ubuntu" if descriptor.name == "ubuntu" else descriptor.distribution
            )
            raise ScriptError(
                f"{guest_label} guest does not support correctness scenario(s): "
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
    if "cpu-profile-fallback" in scenarios:
        print(
            "Running microVM CPU profile fallback correctness on "
            f"OpenVMM/{args.backend}"
        )
        run_cpu_profile_fallback(
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
    if "filesystem-shares" in scenarios:
        print(
            "Running read-write, read-only, and hidden-root microVM shares "
            f"through one aggregate on OpenVMM/{args.backend}"
        )
        run_filesystem_shares(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "filesystem-policy" in scenarios:
        print(
            "Running microVM share write narrowing and read masking "
            f"on OpenVMM/{args.backend}"
        )
        run_filesystem_policy(
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
    if "managed-exec-config" in scenarios:
        print(
            f"Running public managed execution configuration on OpenVMM/{args.backend}"
        )
        run_managed_exec_configuration(
            args.backend, timeout=args.timeout, output_dir=output_dir
        )
    if "sandbox-lifecycle" in scenarios:
        print(f"Running public managed sandbox lifecycle on OpenVMM/{args.backend}")
        run_sandbox_lifecycle(args.backend, timeout=args.timeout, output_dir=output_dir)
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
        print(
            "Running public nvx.py L3/L4 egress policy acceptance "
            f"on OpenVMM/{args.backend}"
        )
        run_l3_l4_egress_policy(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
            guest=descriptor.name,
        )
    if "l3-l4-egress-protocols" in scenarios:
        print(
            "Running public nvx.py L3/L4 egress protocol acceptance "
            f"on OpenVMM/{args.backend}"
        )
        run_l3_l4_egress_protocols(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
            guest=descriptor.name,
        )
    if "l3-l4-egress-port-ranges" in scenarios:
        print(
            "Running public nvx.py L3/L4 egress port-range acceptance "
            f"on OpenVMM/{args.backend}"
        )
        run_l3_l4_egress_port_ranges(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
            guest=descriptor.name,
        )
    if "ipv6-egress-policy" in scenarios:
        print(
            "Running public nvx.py IPv6 egress policy acceptance "
            f"on OpenVMM/{args.backend}"
        )
        run_ipv6_egress_policy(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
            guest=descriptor.name,
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
    if "filesystem-owner" in scenarios:
        print(f"Running microVM filesystem caller ownership on OpenVMM/{args.backend}")
        run_filesystem_owner(
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
    for scenario, counting_lapic in (("smp", False), ("smp-lapic", True)):
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
                counting_lapic=counting_lapic,
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
            args.processors,
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
    if "restore-downtime" in scenarios:
        print(
            "Running microVM long-downtime restore correctness on "
            f"OpenVMM/{args.backend}"
        )
        run_restore_downtime(
            executable,
            kernel,
            initrd,
            args.backend,
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
    if "time-abi-conformance" in scenarios:
        processors = max(args.processors)
        print(
            f"Running microVM time ABI conformance ({processors} vCPU) "
            f"on OpenVMM/{args.backend}"
        )
        run_time_abi_conformance(
            executable,
            kernel,
            initrd,
            args.backend,
            processors,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            log_path=output_dir / "time-abi-conformance.log",
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

    report_time_abi_evidence(
        output_dir,
        backend=args.backend,
        guest=descriptor.name,
        debug_kernel=debug_kernel,
    )
    print(f"Wrote microVM correctness logs to {output_dir}")
    return 0
