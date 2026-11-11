"""Correctness tests for NVX Linux guests running on OpenVMM microVMs."""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import math
import os
import queue
import random
import re
import selectors
import shlex
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from collections.abc import Callable, Collection, Sequence
from contextlib import ExitStack, suppress
from dataclasses import dataclass
from pathlib import Path
from stat import S_ISUID
from typing import Any, NamedTuple, cast

from . import nvx_rs
from .adversarial_oracles import terminate_process_tree
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
from .guests import GUEST_NAMES, GuestDescriptor, guest_descriptor
from .managed_exec_tests import run_managed_exec_configuration, workload_artifact_paths
from .microvm_durations import REPORT_NAME, DurationReport
from .microvm_shards import configure_parser as configure_shards_parser
from .microvm_shards import load_partition, shard_scenarios
from .nvx_rs import EgressRules, compile_egress_policy
from .openvmm_process import BootConsole, OpenvmmProcess
from .sandbox_lifecycle_tests import run_sandbox_lifecycle
from .sandbox_proxy_tests import run_sandbox_proxy_only
from .sandbox_ram_upper_tests import run_sandbox_ram_upper
from .shard_plans import parse_shard
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
    parse_cpu_profile_fallback,
    parse_fields,
    parse_marker,
    status_script,
    warp_probe_script,
    warp_rounds,
)

MICROVM_TEST_SCENARIOS = (
    "command-line-limit",
    "console-exit",
    "console-snapshot",
    "cpu-profile-fallback",
    "directional-network-policy",
    "denied-filesystem-paths",
    "endpoint-policy-snapshot",
    "filesystem-comma-paths",
    "filesystem-owner",
    "filesystem-policy",
    "filesystem-shares",
    "filesystem-snapshot",
    "guest-boot",
    "guest-identity",
    "host-loopback-policy",
    "interrupt-map",
    "ipv6-egress-policy",
    "lifecycle",
    "l3-l4-egress-policy",
    "l3-l4-egress-port-ranges",
    "l3-l4-egress-protocols",
    "managed-lifecycle",
    "managed-exec-config",
    "network-snapshot",
    "platform-aggregate-export",
    "platform-filesystem",
    "platform-network-policy",
    "platform-restore-processors",
    "platform-zero-fill",
    "private-network-ingress",
    "restore-downtime",
    "restore-memory",
    "restore-processors",
    "run-console-restore",
    "sandbox-blocks",
    "sandbox-lifecycle",
    "sandbox-proxy-only",
    "sandbox-ram-upper",
    "scratch-snapshot",
    "smp",
    "smp-snapshot",
    "snapshot-core",
    "snapshot-rollback",
    "snapshot-tiers",
    "structured-outcome",
    "time-abi-conformance",
    "time-host-correction",
    "untiered-network-policy",
    "virtio-features",
    "virtio-net",
    "workload-identity",
)
UBUNTU_UNSUPPORTED_SCENARIOS = frozenset(("console-snapshot",))
SANDBOX_CONTROL_SCENARIOS = frozenset(
    (
        "interrupt-map",
        "managed-exec-config",
        "platform-aggregate-export",
        "platform-filesystem",
        "platform-network-policy",
        "platform-restore-processors",
        "platform-zero-fill",
        "sandbox-blocks",
        "sandbox-lifecycle",
        "sandbox-proxy-only",
        "sandbox-ram-upper",
        "scratch-snapshot",
        "snapshot-tiers",
        "virtio-features",
    )
)
# The public `nvx-rs sandbox` scenarios, which also read the Ubuntu distro
# layer and the sandbox scratch template that `nvx-rs download` installs. A
# default run runs them, but CI runs them in its guest and sandbox jobs, which
# take those Ubuntu artifacts, rather than in the shards of the default suite.
PUBLIC_SANDBOX_SCENARIOS = frozenset(
    (
        "managed-exec-config",
        "sandbox-lifecycle",
        "sandbox-proxy-only",
        "sandbox-ram-upper",
    )
)
# The same-host restore cases also run on the CI debug kernel (doc/ci.md),
# whose soft-lockup and hung-task detectors the guest's time ABI watcher
# reports.
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
# run_guest_script stores each script at GUEST_SCRIPT_PATH with a
# here-document that GUEST_SCRIPT_DELIMITER ends, and the guest prints the
# start line just before the script's first command.
GUEST_SCRIPT_PATH = "/tmp/nvx-guest-script"
GUEST_SCRIPT_DELIMITER = "NVX_GUEST_SCRIPT"
GUEST_SCRIPT_START_MARKER = b"NVX-GUEST-SCRIPT-START"
LIFECYCLE_COMPLETION_MARKER = b"NVX-LIFECYCLE-OK"
# What lifecycle.sh writes to the byte channel, one byte at a time, and
# OpenVMM relays to its stderr beside the boot console.
LIFECYCLE_BYTE_CHANNEL_OUTPUT = b"\x00\r\n\x7f\xff\n"
# A line of the kernel log as a console prints it, after printk's timestamp.
KERNEL_LOG_LINE = re.compile(r"\[ *\d+\.\d{6}\] ")
SANDBOX_BLOCKS_COMPLETION_MARKER = b"NVX-SANDBOX-BLOCKS-OK"
VIRTIO_NET_COMPLETION_MARKER = b"NVX-VIRTIO-NET-OK"
VIRTIO_FEATURES_COMPLETION_MARKER = b"NVX-VIRTIO-FEATURES-OK"
# The prefix of the line on which the virtio-features scenario's guest reports
# a virtio device: the base of its MMIO window, its virtio device ID, and the
# features that its driver negotiated, one character per bit from bit 0.
VIRTIO_FEATURES_REPORT_PREFIX = "NVX-VIRTIO-FEATURES "
VIRTIO_FEATURES_NETWORK = "10.0.0.2/24"
INTERRUPT_MAP_COMPLETION_MARKER = b"NVX-INTERRUPT-MAP-OK"
INTERRUPT_MAP_CAPTURE_MARKER = b"NVX-INTERRUPT-MAP-CAPTURE"
# The prefix of the lines on which the interrupt-map scenario's guest reports,
# after a step, the window of a virtio device and the interrupt that its
# driver requested, and of the line of /proc/interrupts' counts of APIC errors
# and spurious PIC interrupts (ERR), of level-triggered interrupts that the
# IOAPIC misrouted (MIS), and of spurious APIC interrupts (SPU).
INTERRUPT_MAP_REPORT_PREFIX = "NVX-INTERRUPT-MAP "
INTERRUPT_MAP_ERRORS_PREFIX = "NVX-INTERRUPT-MAP-ERRORS "
INTERRUPT_MAP_ERROR_COUNTS = ("err", "mis", "spu")
INTERRUPT_MAP_NETWORK = "10.0.0.2/24"
INTERRUPT_MAP_SHARE_SEED = "NVX-INTERRUPT-MAP-SHARE"
# The record with which the interrupt-map scenario's guest attaches to the
# control console's session broker, so that the broker answers with a Reset
# record (4.4.3): the bootstrap GuestAttach record, whose 44-byte header holds
# the magic NVXS, version 1, type 1, and zero flags, instance, epoch,
# sequence, and payload length, as the GUEST_ATTACH vector of OpenVMM's
# control-session protocol gives it.
INTERRUPT_MAP_GUEST_ATTACH = b"NVXS" + bytes((1, 0, 1, 0)) + bytes(36)
# The devices of the interrupt-map scenario's snapshot, by the bases of their
# windows: the NIC, the host filesystem with a share, and the boot console.
INTERRUPT_MAP_SNAPSHOT_DEVICES = (0xD000_0000, 0xD000_1000, 0xD000_2000)


class MicrovmVirtioDevice(NamedTuple):
    """A fixed virtio device of the microVM, as Table 2.8 of the architecture
    guide (doc/design/2.5-device-topology-transport-and-discovery.md) lists
    it: its name, virtio device ID, and the feature bits that it offers the
    guest, every one of which Linux negotiates, and as Table 2.7 lists its
    interrupt, the IOAPIC input of its window."""

    name: str
    device_id: int
    features: frozenset[int]
    irq: int


# Each fixed virtio device of the microVM, by the base of its MMIO window.
MICROVM_VIRTIO_DEVICES = {
    0xD000_0000: MicrovmVirtioDevice("Network", 1, frozenset({5, 32}), 16),
    0xD000_1000: MicrovmVirtioDevice(
        "Host filesystem", 26, frozenset({28, 29, 32}), 17
    ),
    0xD000_2000: MicrovmVirtioDevice("Boot console", 3, frozenset({28, 29, 32}), 18),
    0xD000_3000: MicrovmVirtioDevice(
        "`distro` block", 2, frozenset({2, 5, 6, 9, 10, 28, 29, 32}), 19
    ),
    0xD000_6000: MicrovmVirtioDevice(
        "`scratch` block", 2, frozenset({2, 6, 9, 10, 28, 29, 32}), 22
    ),
    0xD000_7000: MicrovmVirtioDevice("Control console", 3, frozenset({28, 29, 32}), 23),
}
# The x86 guest kernel's COMMAND_LINE_SIZE. OpenVMM requires the effective
# kernel command line, with the tokens that it appends after the caller's, to
# fit in it with its terminating NUL, and refuses a longer one before boot.
KERNEL_COMMAND_LINE_SIZE = 2048
COMMAND_LINE_LIMIT_NETWORK = "10.0.0.2/24"
# The kernel takes a parameter whose name has a dot for a module's and passes
# it to neither init nor its environment, so this token only takes up room.
COMMAND_LINE_PADDING = "cmdline_limit.padding="
COMMAND_LINE_PREFIX = b"NVX-COMMAND-LINE: "
COMMAND_LINE_LIMIT_COMPLETION_MARKER = b"NVX-COMMAND-LINE-LIMIT-OK"
COMMAND_LINE_LIMIT_ERROR = (
    b"microVM kernel command line is 2048 bytes; "
    b"the limit is 2047 bytes plus the terminating NUL"
)
DIRECTIONAL_NETWORK_ALLOW_MARKER = b"NVX-DIRECTIONAL-NETWORK-ALLOW-OK"
DIRECTIONAL_NETWORK_DENY_MARKER = b"NVX-DIRECTIONAL-NETWORK-DENY-OK"
DIRECTIONAL_NETWORK_INGRESS_READY_MARKER = b"NVX-DIRECTIONAL-INGRESS-READY"
DIRECTIONAL_NETWORK_GUEST_IPV4 = "192.0.2.2"
DIRECTIONAL_NETWORK_GATEWAY_IPV4 = "192.0.2.1"
DIRECTIONAL_NETWORK_CIDR = f"{DIRECTIONAL_NETWORK_GUEST_IPV4}/24"
# A /30 holds only the gateway and the guest, so it leaves no address at which
# OpenVMM could present host peers, which explicit host-loopback access needs.
HOST_LOOPBACK_NARROW_NETWORK_CIDR = f"{DIRECTIONAL_NETWORK_GUEST_IPV4}/30"
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
HOST_LOOPBACK_CHECKED = b"NVX-HOST-LOOPBACK-CHECKED"
HOST_LOOPBACK_TCP_ACK = b"NVX-HOST-LOOPBACK-TCP-ACK\n"
HOST_LOOPBACK_TCP_REQUEST = b"NVX-HOST-LOOPBACK-INBOUND\n"
HOST_LOOPBACK_UDP_REQUEST = b"NVX-HOST-LOOPBACK-UDP\n"
HOST_LOOPBACK_UDP_REPLY = b"NVX-HOST-LOOPBACK-UDP-REPLY\n"
HOST_LOOPBACK_UDP_ECHO_REQUEST = b"NVX-HOST-LOOPBACK-UDP-ECHO\n"
HOST_LOOPBACK_UDP_ECHO_REPLY = b"NVX-HOST-LOOPBACK-UDP-ECHO-REPLY\n"
HOST_LOOPBACK_PORT_BIND_ATTEMPTS = 16
# IANA's dynamic port range, which is also Windows' default. The host-loopback
# scenario draws the port of each TCP and UDP listener pair from it.
HOST_LOOPBACK_PORT_RANGE = (49152, 65535)
PRIVATE_INGRESS_LISTENING_MARKER = b"NVX-PRIVATE-INGRESS-LISTENING"
PRIVATE_INGRESS_CHECKED = b"NVX-PRIVATE-INGRESS-CHECKED"
PRIVATE_INGRESS_REACHED_MARKER = b"NVX-PRIVATE-INGRESS-REACHED-OK"
PRIVATE_INGRESS_REFUSED_MARKER = b"NVX-PRIVATE-INGRESS-REFUSED-OK"
PRIVATE_INGRESS_UNPUBLISHED_MARKER = b"NVX-PRIVATE-INGRESS-UNPUBLISHED-OK"
PRIVATE_INGRESS_TCP_REQUEST = b"NVX-PRIVATE-INGRESS-TCP-REQUEST\n"
PRIVATE_INGRESS_TCP_REPLY = b"NVX-PRIVATE-INGRESS-TCP-REPLY\n"
PRIVATE_INGRESS_UDP_REQUEST = b"NVX-PRIVATE-INGRESS-UDP-REQUEST\n"
PRIVATE_INGRESS_UDP_REPLY = b"NVX-PRIVATE-INGRESS-UDP-REPLY\n"
SANDBOX_BLOCK_SIZE = 8 * 1024 * 1024
SNAPSHOT_CORE_CONTINUED_MARKER = b"NVX-SNAPSHOT-CORE-CONTINUED"
SNAPSHOT_CORE_COMPLETION_MARKER = b"NVX-SNAPSHOT-CORE-OK"
CONSOLE_BINARY_MARKER = b"\0\r\n\x7f\xffNVX-CONSOLE-BINARY"
CONSOLE_RX_READY_MARKER = b"NVX-CONSOLE-RX-READY"
CONSOLE_RX_QUEUED_MARKER = b"NVX-CONSOLE-RX-QUEUED"
CONSOLE_SNAPSHOT_TRIGGER = b"\x01"
CONSOLE_RX_RESTORED_MARKER = b"NVX-CONSOLE-RX-RESTORED"
CONSOLE_TX_DONE_MARKER = b"NVX-CONSOLE-TX-DONE"
CONSOLE_EXIT_COMPLETION_MARKER = b"NVX-CONSOLE-EXIT-OK"
CONSOLE_EXIT_PAYLOAD_BYTES = 64 * 1024
CONSOLE_EXIT_READ_DELAY_SECONDS = 2.0
# Each console-exit run's guest exit status and the console that carries its
# payload: the boot console, on OpenVMM's standard output, or the byte channel,
# on its standard error.
CONSOLE_EXIT_RUNS = ((0, "hvc1"), (37, "hvc0"))
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
FILESYSTEM_COMMA_PATHS_MARKER = b"NVX-FILESYSTEM-COMMA-PATHS-OK"
NETWORK_BEFORE_MARKER = b"NVX-NETWORK-BEFORE"
NETWORK_INVALIDATED_MARKER = b"NVX-NETWORK-OLD-FLOW-INVALIDATED"
NETWORK_AFTER_MARKER = b"NVX-NETWORK-AFTER"
SCRATCH_PAIRED_POST_MARKER = b"NVX-SCRATCH-PAIRED-POST-OUT"
SCRATCH_PAIRED_RESTORED_MARKER = b"NVX-SCRATCH-PAIRED-RESTORED"
SCRATCH_FRESH_POST_MARKER = b"NVX-SCRATCH-FRESH-POST-OUT"
SCRATCH_FRESH_VALUE_PREFIX = b"NVX-SCRATCH-FRESH-VALUE-"
SCRATCH_FRESH_VALUE_SUFFIX = b"-END"
WORKLOAD_IDENTITY_MARKER = b"NVX-WORKLOAD-IDENTITY-OK uid=65534 gid=65534"
BOOT_MARKER = b"NVX-GUEST-BOOT-OK:"
GUEST_BOOT_COMPLETION_MARKER = b"NVX-GUEST-BOOT-CHECK-OK"
GUEST_IDENTITY_COMPLETION_MARKER = b"NVX-GUEST-IDENTITY-OK"
RUN_CONSOLE_RESTORE_MARKER = b"NVX-RUN-CONSOLE-RESTORE-OK"
RESTORE_PROCESSORS_FAILURE_MARKER = b"NVX-RESTORE-PROCESSORS-FAIL"
# A platform snapshot opts into restore-time vCPU activation as an untiered one
# does, by capturing its capacity booted with maxcpus=. A capacity of 4 leaves
# a supported count, 8, above it.
PLATFORM_RESTORE_PROCESSORS_CAPACITY = 4
PLATFORM_RESTORE_PROCESSORS_BOOT_ONLINE = 1
# The restore-online counts that OpenVMM refuses before the guest runs, and
# why: counts that a microVM does not support, and a supported count above the
# capacity. The snapshot's restore preparation checks them before the
# command-line validation, which would refuse the unsupported counts too.
PLATFORM_RESTORE_PROCESSORS_REJECTED = (
    (0, b"restore-online VP count 0 is not supported"),
    (3, b"restore-online VP count 3 is not supported"),
    (8, b"restore-online VP count 8 exceeds VP capacity 4"),
)
PLATFORM_POST_RESTORE_MARKER = b"NVX-POST-RESTORE-OK: tier=platform"
# A platform restore presents the captured capacity of its distro block over a
# shorter image. The image ends inside a 4-KiB page, so the device reads that
# page with one request across the image's end, and the restore declares the
# fresh scratch absent.
PLATFORM_ZERO_FILL_CAPACITY = 1024 * 1024
PLATFORM_ZERO_FILL_IMAGE_SECTORS = 601
PLATFORM_ZERO_FILL_SCRATCH = 1024 * 1024
PLATFORM_ZERO_FILL_FAILURE_MARKER = b"NVX-ZERO-FILL-FAIL"
PLATFORM_ZERO_FILL_IMAGE_MARKER = b"NVX-ZERO-FILL-IMAGE-OK"
PLATFORM_ZERO_FILL_SCRATCH_MARKER = b"NVX-ZERO-FILL-SCRATCH-OK"
# The restores that OpenVMM refuses before the guest runs, and why.
PLATFORM_ZERO_FILL_REJECTED = (
    ("longer-image", b"longer than the platform snapshot's"),
    ("absent-and-file", b"--restore-absent-scratch attaches the scratch block"),
    ("no-scratch", b"requires a scratch block or --restore-absent-scratch"),
)
PAIRED_ABSENT_SCRATCH_REJECTION = (
    b"--restore-absent-scratch requires the fresh scratch of a platform snapshot"
)
# The storage generation to which the paired scratch snapshot binds its
# blocks, which each of its restores states again, and another one. OpenVMM
# reads no block to verify its content, which the generation alone vouches
# for, so it refuses a restore that states another generation or none, and
# one of a layer or a scratch.img of another length.
SCRATCH_PAIRED_GENERATION = "00112233445566778899aabbccddee10"
SCRATCH_PAIRED_OTHER_GENERATION = "00112233445566778899aabbccddee11"
SCRATCH_PAIRED_GENERATION_REJECTIONS = (
    (
        "wrong",
        SCRATCH_PAIRED_OTHER_GENERATION,
        b"the snapshot binds its sandbox blocks to storage generation "
        + SCRATCH_PAIRED_GENERATION.encode()
        + b", not "
        + SCRATCH_PAIRED_OTHER_GENERATION.encode(),
    ),
    ("missing", None, b"pass it with --restore-generation-id"),
)
SCRATCH_PAIRED_WRONG_LAYER_REJECTION = b"doesn't match the requested machine"
# A NIC template is a platform snapshot whose NIC was captured with a complete
# deny-all policy. Each clone restores it with a policy of its own, and must
# keep the template's NIC identity, which the portable profile derives from the
# network.
PLATFORM_NETWORK_CIDR = "10.0.0.2/24"
PLATFORM_NETWORK_GATEWAY_IPV4 = "10.0.0.1"
PLATFORM_NETWORK_IDENTITY = (
    b"NVX-PLATFORM-NETWORK-IDENTITY mac=52:54:00:00:00:02 ipv4=10.0.0.2/24 "
    b"ipv6=fd00::a00:2/120"
)
PLATFORM_NETWORK_TEMPLATE_POLICY = (
    "--network-egress",
    "deny",
    "--host-loopback",
    "deny",
)
PLATFORM_NETWORK_FAILURE_MARKER = b"NVX-PLATFORM-NETWORK-FAIL"
PLATFORM_NETWORK_LISTENING_MARKER = b"NVX-PLATFORM-NETWORK-LISTENING"
PLATFORM_NETWORK_CHECKED = b"NVX-PLATFORM-NETWORK-CHECKED"
PLATFORM_NETWORK_PROBE_PREFIX = b"NVX-PLATFORM-NETWORK-PROBE "
PLATFORM_NETWORK_GUEST_PORT = 18080
# How long a clone's probe that its policy denies waits for its connection.
PLATFORM_NETWORK_PROBE_TIMEOUT_SECONDS = 2
# The restores of the NIC template that OpenVMM refuses before the guest runs,
# and why, besides the busy forward port's and the plain deny-all snapshot's.
PLATFORM_NETWORK_REJECTED = (
    ("kept-policy", b"restore-time egress policy does not match the snapshot"),
    ("kept-host-loopback", b"microVM snapshots do not support --host-loopback allow"),
    ("readdressed", b"network addressing from saved state"),
    ("foreign-proxy", b"must match the guest gateway"),
)
# OpenVMM leads its fatal error with the stable code of a forward whose host
# port it cannot bind.
PLATFORM_NETWORK_BUSY_FORWARD_REJECTION = b"fatal error: [E_PORT_FORWARD_BIND] "
PLATFORM_NETWORK_NOT_TEMPLATE_REJECTION = b"is not a complete deny-all"
# Lines that only a restored clone's guest prints, from its platform repair's
# report to the restore's end, so that a refused restore prints none of them.
PLATFORM_NETWORK_GUEST_MARKERS = (
    PLATFORM_POST_RESTORE_MARKER,
    PLATFORM_NETWORK_LISTENING_MARKER,
    RESTORE_MARKER,
)
# A NIC template without a tier is a snapshot without sandbox blocks whose NIC
# was captured with the complete deny-all policy. Each restore installs a
# policy of its own, and its guest's repair applies the DNS server and the
# listener announcement of that policy, which the restore packet carries.
UNTIERED_POST_RESTORE_MARKER = b"NVX-POST-RESTORE-OK: tier=legacy"
UNTIERED_NETWORK_FAILURE_MARKER = b"NVX-UNTIERED-NETWORK-FAIL"
UNTIERED_NETWORK_LISTENING_MARKER = b"NVX-UNTIERED-NETWORK-LISTENING"
UNTIERED_NETWORK_CHECKED = b"NVX-UNTIERED-NETWORK-CHECKED"
UNTIERED_NETWORK_REPORT_PREFIX = b"NVX-UNTIERED-NETWORK-REPORT "
UNTIERED_NETWORK_INBOUND_PREFIX = b"NVX-UNTIERED-NETWORK-INBOUND "
# The template's NIC is on the directional network, whose documentation range
# no host address shares: an ingress listener refuses peers inside the guest's
# own subnets, so a host address there would not reach the port that
# private-network ingress publishes.
UNTIERED_NETWORK_CIDR = DIRECTIONAL_NETWORK_CIDR
UNTIERED_NETWORK_GATEWAY_IPV4 = DIRECTIONAL_NETWORK_GATEWAY_IPV4
# The gateway's IPv6 address, which a policy that admits DNS over IPv6 alone
# names as the guest's DNS server.
UNTIERED_NETWORK_GATEWAY_IPV6 = DIRECTIONAL_NETWORK_GATEWAY_IPV6
# The most clients that the guest's server answers, as many as a restore's
# host reaches it through: its forward and its published port.
UNTIERED_NETWORK_MAX_CLIENTS = 2
# The restores of the NIC template that OpenVMM refuses before the guest runs,
# and why.
UNTIERED_NETWORK_REJECTED = (
    ("kept-policy", b"restore-time egress policy does not match the snapshot"),
    ("ingress", b"microVM snapshots do not support --network-ingress allow"),
)
# Lines that only a restored guest prints, so that a refused restore prints
# none of them.
UNTIERED_NETWORK_GUEST_MARKERS = (
    UNTIERED_POST_RESTORE_MARKER,
    UNTIERED_NETWORK_LISTENING_MARKER,
    RESTORE_MARKER,
)
# A platform restore attaches a host directory to the filesystem slot that the
# capture left dormant. The clone mounts it after the restore's input gate
# releases, reads the host's seed from it, and writes its result to it.
PLATFORM_FILESYSTEM_FAILURE_MARKER = b"NVX-PLATFORM-FILESYSTEM-FAIL"
PLATFORM_FILESYSTEM_MARKER = b"NVX-PLATFORM-FILESYSTEM-OK"
PLATFORM_FILESYSTEM_HOST_SEED = b"NVX-HOST-TO-GUEST"
PLATFORM_FILESYSTEM_GUEST_RESULT = b"NVX-GUEST-TO-HOST"
# A platform restore attaches the aggregate of the filesystem-shares scenario
# to the filesystem slot that the capture left dormant. The clone keeps its
# captured command line, which names no filesystem, so it mounts the aggregate
# itself and checks each child's kind, mode, and policy through its root.
PLATFORM_AGGREGATE_FAILURE_MARKER = b"NVX-PLATFORM-AGGREGATE-FAIL"
PLATFORM_AGGREGATE_MARKER = b"NVX-PLATFORM-AGGREGATE-OK"
# The restores of the platform snapshot that OpenVMM refuses before the guest
# runs, because their aggregates are ones that a cold boot refuses, and why.
PLATFORM_AGGREGATE_REJECTED = (
    ("overlapping-children", b"must not overlap"),
    ("file-in-directory", b"must not overlap"),
    ("directory-as-file", b"not a regular file"),
    ("file-as-directory", b"not a plain directory"),
    ("repeated-name", b"not unique"),
    ("file-policy", b"exposes the file"),
    ("outside-policy", b"outside the filesystem export roots"),
    ("relative-policy", b"absolute host path"),
)
# A Windows host refuses caller ownership at a restore as at a cold boot.
PLATFORM_AGGREGATE_OWNER_REJECTION = b"--mount-owner caller requires a Linux host"
# Lines that only a restored clone's guest prints, from its platform repair's
# report to the restore's end, so that a refused restore prints none of them.
PLATFORM_AGGREGATE_GUEST_MARKERS = (
    PLATFORM_POST_RESTORE_MARKER,
    PLATFORM_AGGREGATE_MARKER,
    RESTORE_MARKER,
)
# The dormant slot of a snapshot of any other tier, or without a tier, binds
# only a --mount at a restore.
DORMANT_AGGREGATE_REJECTION = (
    b"--mount-aggregate on a dormant filesystem slot requires a platform snapshot"
)
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
# The time-host-correction scenario's host clock corrections, in ppm, that
# OpenVMM's host-rate-offset-ppm hook makes it publish, against the sign of the
# host's own correction, which an unhooked cold boot's time sample gives: a
# cold boot's and a restore's within the guest's 500 ppm frequency limit, and
# a restore beyond that limit. The hook itself is bounded by OpenVMM's parser.
TIME_HOST_CORRECTION_BOOT_PPM = 200
TIME_HOST_CORRECTION_RESTORE_PPM = 150
TIME_HOST_CORRECTION_LIMIT_PPM = 900
OPENVMM_HOST_RATE_OFFSET_LIMIT_PPM = 10_000
# Seconds between the guest's two time samples of each phase.
TIME_HOST_CORRECTION_SPACING_SECONDS = 5
# Seconds after clearing the PLL phase offset: longer than one kernel second even at Linux's
# slowest allowed correction, so the tick length no longer carries the cached phase correction.
TIME_HOST_CORRECTION_PHASE_SETTLE_SECONDS = 2
# The most measurement windows that the guest's report takes, retrying one that
# the discipline, host UTC, or an unpaired or uncertain sample left unusable.
TIME_HOST_CORRECTION_ATTEMPTS = 3
# The largest uncertainty, in ns, of a time sample that a measurement window
# pairs: the bound of the guest's discipline (SAMPLE_BOUND_NS in
# nvx-rs/src/guest/time/system.rs). nvx-time sample also reports a sample
# beyond it, whose realtime bracket can widen the rate check's allowance past
# the hooks' corrections: two samples 5 s apart, each uncertain by 1 ms, would
# allow 400 ppm. Such a window is retried instead.
TIME_HOST_CORRECTION_SAMPLE_BOUND_NS = 100_000
# How far, in ppm, the guest's clock may run from the host's correction that it
# holds, against the guest TSC counted at the declared rate, beyond the
# uncertainty of the samples' pairing of the guest's realtime clock with that
# TSC: the kernel runs a frequency correction to a fraction of a ppm, and a
# guest that didn't apply it would run 150 ppm or more off it.
TIME_HOST_CORRECTION_GUEST_PPM = 1
# How far, in ppm, host UTC may run from its published correction against the
# same TSC, beyond half the correction's change between the samples and their
# pairing bounds: half the 150 ppm of the smallest hook that two samples
# bracket, which a hook that skewed host UTC or its correction alone would
# leave between them. The host's time service slews host UTC from an update:
# a slew under way at the first sample shows in its correction, which half the
# change covers when it ends before the second, and a window between whose
# samples an update began one that ran host UTC off its correction is retried,
# as Linux's maximum error shows the update. The guest's discipline absorbs
# such slews at its next poll.
TIME_HOST_CORRECTION_HOST_PPM = 75
TIME_HOST_CORRECTION_STATE_PREFIX = "NVX-TIME-HOST-CORRECTION-STATE: "
TIME_HOST_CORRECTION_SAMPLE_PREFIX = "NVX-TIME-HOST-CORRECTION-SAMPLE: "
# The time-state fields that must stay fixed over one measurement window.
TIME_HOST_CORRECTION_DISCIPLINE_FIELDS = (
    "frequency_ppb",
    "frequency_limited",
    "host_correction_ppb",
    "discontinuities",
    "last_step_realtime_ns",
    "samples",
)
TIME_HOST_CORRECTION_COMPLETION_MARKER = b"NVX-TIME-HOST-CORRECTION-OK"
# Linux's limit of the frequency correction, in ppb.
LINUX_FREQUENCY_LIMIT_PPB = 500_000
# How much, in us, Linux grows the maximum error of host UTC each second while
# a time service synchronizes it: MAXFREQ, 500 ppm. The service sets the
# maximum error at each update of host UTC.
LINUX_MAX_ERROR_GROWTH_US = 500
# The flag of a time sample that a test hook altered.
TIME_SAMPLE_TEST_HOOKS = 0x08
# The guest finishes a restore after the RCU grace-period release, which gives
# up after rcu_cpu_stall_timeout (21 s), and the deferred C7 check.
TIME_ABI_RESTORE_FINISH_SECONDS = 30.0
# Every scenario captures a cold-booted guest (generation 0), and OpenVMM
# cannot capture a restored one, so every restore carries generation 1.
RESTORED_GENERATION = 1
# The time ABI's long-downtime case (doc/ci.md): longer than the 21 s RCU
# stall timeout, at 1 and 8 vCPUs, with and without expedited grace periods.
RESTORE_DOWNTIME_SECONDS = 30.0
RESTORE_DOWNTIME_PROCESSORS = (1, 8)
RESTORE_DOWNTIME_COMPLETION_MARKER = b"NVX-RESTORE-DOWNTIME-OK"
RESTORE_DOWNTIME_PATH = "/tmp/nvx-restore-downtime"
# Gives the RCU stall detector time to report a stall that the release of
# the stall suppression exposed.
RESTORE_DOWNTIME_SETTLE_SECONDS = 2
# The time ABI's longest downtime (doc/design/3.2-clock-and-time.md): 365
# days, the fixed duration of 31,536,000 s.
MAX_DOWNTIME_SECONDS = 365 * 24 * 60 * 60
# The restore-downtime scenario's simulated one-year restores (doc/ci.md):
# OpenVMM's downtime-add-s hook adds what brings the real downtime, which the
# harness bounds by the time since it started the capture, to this much below
# the bound, which covers the restore's own time to its anchor. Their sources
# alternate between the same boot's monotonic clock and, through the
# boot-id-mismatch hook, which simulates a reboot or another host, UTC.
RESTORE_YEAR_MARGIN_SECONDS = 60
RESTORE_YEAR_PATH = "/tmp/nvx-restore-year"
RESTORE_YEAR_COMPLETION_MARKER = b"NVX-RESTORE-YEAR-OK"
RESTORE_YEAR_REPORT_PREFIX = "NVX-RESTORE-YEAR "
# How far, in seconds, the guest's raw monotonic clock may advance from the
# real downtime plus the simulated one, against its realtime clock, at the
# declared TSC rate as the kernel counts it, to the kHz, beyond the ppb by
# which the kernel's clocksource multiplier, rounded to the nearest for its
# 600 s range, counts cycles off that rate at any plausible rate up to 10 GHz;
# how far, in ppm of the simulated downtime, its frequency-corrected
# monotonic and boot-time clocks may advance beyond that rate times the
# frequency correction that it held before the capture, by the phase slew
# that its discipline ran then; and how far its realtime clock may stand
# from host UTC once the report arrives.
RESTORE_YEAR_CLOCK_TOLERANCE_SECONDS = 1
RESTORE_YEAR_MULTIPLIER_PPB = 500
RESTORE_YEAR_SLEW_PPM = 5
RESTORE_YEAR_REALTIME_TOLERANCE_SECONDS = 10
# The timer that the simulated downtime expires: one hour, armed before the
# capture, which must fire within this many seconds of the restore's clock
# step, before or after it: the kernel expires it at its first tick after the
# restore, which can come seconds before the guest's repair steps realtime.
RESTORE_YEAR_TIMER_SECONDS = 3600
RESTORE_YEAR_TIMER_DELAY_SECONDS = 10
# How long, in seconds, the capture of each restore-downtime snapshot gives the
# year's pre-capture preparation beyond the scenario's timeout: a second of its
# workload, up to 5 s for the freeze, and the wait for the time daemon's next
# discipline poll, which comes 16 s after the last one at first and 64 s after
# it once the daemon has settled, and which the preparation gives up on after
# 700 sleeps of 0.1 s.
RESTORE_YEAR_PREPARATION_SECONDS = 90
# How much longer, in seconds, a simulated one-year restore may take to print
# the restore marker than the restore of the same snapshot after the real
# downtime: Linux accumulates the year's TSC advance at its first tick, 128
# ticks per step, with interrupts off.
RESTORE_YEAR_READY_ALLOWANCE_SECONDS = 10
# The most CPU time, in clock ticks (USER_HZ, 100 Hz) and in microseconds,
# that the frozen workload may gain while it stays frozen across the
# restore, and the least and most that it must gain in the second that it
# runs afterwards.
RESTORE_YEAR_FROZEN_TICKS = 1
RESTORE_YEAR_FROZEN_USAGE_USEC = 10_000
RESTORE_YEAR_THAWED_MIN_USAGE_USEC = 100_000
RESTORE_YEAR_THAWED_MAX_USAGE_USEC = 3_000_000
RESTORE_YEAR_THAWED_MIN_TICKS = 10
RESTORE_YEAR_THAWED_MAX_TICKS = 300
# The most CPU time, in clock ticks, that the console shell's reaped
# children, among them the capture's requester, which ran at the capture,
# may have used in all: ten minutes, where the 8-vCPU warp probes use about
# half a minute, and a requester charged the downtime would have used a year.
RESTORE_YEAR_REAPED_MAX_TICKS = 60_000
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


def default_suite() -> tuple[str, ...]:
    """Return the default suite of the Alpine guest on the default kernel,
    which CI runs in the shards of microvm_shards.json: the scenarios of a
    default run but the public sandbox scenarios, which CI runs in its guest
    and sandbox jobs."""
    return tuple(
        scenario
        for scenario in MICROVM_TEST_SCENARIOS
        if scenario not in PUBLIC_SANDBOX_SCENARIOS
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
            "scenario to run; repeat to select multiple (default: all but "
            "smp-lapic, which runs only when named)"
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
        "--shard",
        type=parse_shard,
        metavar="INDEX/COUNT",
        help=(
            "run shard INDEX of the COUNT shards of the default Alpine suite on "
            "the default kernel, without the public sandbox scenarios, as "
            "scripts/nvx_tools/microvm_shards.json splits it for the backend, "
            "such as 1/2; COUNT must be its number of shards"
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
        help=(
            "directory for complete per-scenario OpenVMM logs and the scenario "
            "duration report"
        ),
    )
    parser.set_defaults(handler=run)


def configure_shard_parser(parser: argparse.ArgumentParser) -> None:
    """Configure `plan-microvm-shards` for the default suite."""
    configure_shards_parser(
        parser, suite=default_suite, processors=MICROVM_PROCESSOR_COUNTS
    )


class GuestScriptResult(GuestCommandResult):
    """A guest script's run. ``text`` is the whole console transcript, and
    ``output`` what the console printed after the script's start line, which
    is the script's own output."""

    output: str


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
    boot_marker: bytes = BOOT_MARKER,
    on_stderr: Callable[[bytes], None] | None = None,
) -> GuestScriptResult:
    """Run ``script`` in the shell of the guest that ``command`` boots, and
    require it to print the line ``completion_marker``.

    The guest's terminal echoes console input that arrives while a command
    runs into whatever that command prints, so a script that the console typed
    line by line could find the echo of its later lines inside the output of
    its earlier ones (#785). The console therefore stages the whole script at
    GUEST_SCRIPT_PATH first. Its last line prints GUEST_SCRIPT_START_MARKER
    and sources the script, which runs in the shell as typed lines did, but
    only once the guest has consumed all the input. The console output after
    that line, ``output``, is thus the script's own, and ``completion_marker``
    must be one of its lines. The console types the script once the guest
    prints ``boot_marker``: its boot marker, or for a restore, a marker that
    the restored guest prints. With ``on_stderr``, OpenVMM's stderr reaches it
    rather than the console, so that ``text`` and ``output`` hold only what
    the console printed, as benchmark.run_guest_script describes.
    """
    result = _run_guest_script(
        command,
        _staged_script(
            GUEST_SCRIPT_PATH,
            GUEST_SCRIPT_DELIMITER,
            script,
            # The quotes keep the start line out of this line's echo.
            f'echo NVX-GUEST-SCRIPT-"START"; . {GUEST_SCRIPT_PATH}',
        ),
        completion_marker,
        timeout=timeout,
        windows_cpus=windows_cpus,
        teardown_mode=teardown_mode,
        log_path=log_path,
        boot_marker=boot_marker,
        contain_process_tree=contain_process_tree,
        time_abi_status=True,
        on_stderr=on_stderr,
    )
    output = _guest_script_output(result["text"])
    if not contains_output_line(output.encode("utf-8"), completion_marker):
        raise RuntimeError(
            f"guest script did not print {completion_marker.decode()!r} after "
            f"its start line\n--- OpenVMM output ---\n{result['text'][-4096:]}"
        )
    return GuestScriptResult(
        text=result["text"],
        wall_ms=result["wall_ms"],
        peak_rss_bytes=result["peak_rss_bytes"],
        output=output,
    )


def _guest_script_output(text: str) -> str:
    """Return what the console printed after a staged script's start line."""
    start = GUEST_SCRIPT_START_MARKER.decode()
    lines = text.split("\n")
    for index, line in enumerate(lines):
        if line.removesuffix("\r") == start:
            return "\n".join(lines[index + 1 :])
    raise RuntimeError(
        f"guest never printed the start line {start!r} of its script"
        f"\n--- OpenVMM output ---\n{text[-4096:]}"
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
    pre_capture_script: str | None = None,
    pre_capture_seconds: float = 0.0,
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
        pre_capture_script=pre_capture_script,
        pre_capture_seconds=pre_capture_seconds,
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


def _staged_script(path: str, delimiter: str, script: str, run: str) -> str:
    """Return console input that stores ``script`` at ``path`` and then runs
    the command line ``run``.

    The shell reads a here-document whole before it runs the command that
    reads it, so none of the script runs before the guest has consumed all of
    it, and ``run`` follows it as the input's last line.
    """
    if not script.endswith("\n"):
        raise ValueError("a staged guest script must end with a newline")
    if delimiter in script.split("\n"):
        raise ValueError(f"a staged guest script cannot hold the line {delimiter!r}")
    return f"cat >{path} <<'{delimiter}'\n{script}{delimiter}\n{run}\n"


def _stage_script(
    process: OpenvmmProcess,
    path: str,
    delimiter: str,
    script: str,
) -> None:
    process.send_bytes(_staged_script(path, delimiter, script, f"sh {path}").encode())


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


def _console_endpoint(directory: Path) -> str:
    """Return a fresh endpoint at which a boot console listens: a Unix socket
    in ``directory``, which must be beside a snapshot that records it, or a
    named pipe on Windows.

    A boot console never listens on TCP, which guests could reach through the
    host-loopback mapping of their gateways (microsoft/nvx#551).
    """
    if os.name == "nt":
        return f"//./pipe/openvmm-microvm-{uuid.uuid4().hex}"
    return str(directory / "console.sock")


def _persist_console_log(
    console: BootConsole | None,
    output: bytes,
    log_path: Path,
) -> bytes:
    # Only failure paths reach here with an open console; the monitor still
    # scans the tail, but nothing is raised over the error being handled.
    if console is not None:
        output = console.finish(check=False)
    log_path.write_bytes(output)
    return output


def _queue_console_rx_and_request_snapshot(
    console: BootConsole,
    data: bytes,
    timeout: float,
) -> None:
    """Send ``data`` to the guest's console, and once the guest has queued
    it, request the snapshot with one more byte, which the guest counts
    without reading."""
    console.send_bytes(data)
    console.wait_for_line(CONSOLE_RX_QUEUED_MARKER, timeout)
    console.send_bytes(CONSOLE_SNAPSHOT_TRIGGER)


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


def check_quiet_console(text: str) -> None:
    """Require that the console transcript ``text`` of a boot with
    ``quiet loglevel=0`` holds no line of the kernel log. The early console,
    ``earlycon=xe9``, registers before the kernel parses the options that
    follow it, so it must apply their log level first: otherwise it prints
    the kernel's first messages, at one VM exit per byte."""
    line = KERNEL_LOG_LINE.search(text)
    if line is not None:
        printed = text[line.start() :].partition("\n")[0].rstrip("\r")
        raise RuntimeError(
            f"a boot with quiet loglevel=0 printed the kernel log line {printed!r}"
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
    """Run lifecycle.sh in a quiet boot with the boot console.

    The script writes LIFECYCLE_BYTE_CHANNEL_OUTPUT to the byte channel just
    before it prints its marker on the boot console. OpenVMM relays the one
    to its stderr and the other to its stdout, with no order between them,
    so the scenario reads stderr apart from the console, which alone carries
    the markers (#1290), and requires the bytes there, in order. Both streams
    must be quiet, since the early console prints on the byte channel.
    """
    command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0",
    )
    stderr = bytearray()
    result = run_guest_script(
        command,
        _read_script("lifecycle.sh"),
        LIFECYCLE_COMPLETION_MARKER,
        timeout=timeout,
        log_path=log_path,
        on_stderr=stderr.extend,
    )
    check_quiet_console(stderr.decode("utf-8", "replace"))
    check_quiet_console(result["text"])
    if LIFECYCLE_BYTE_CHANNEL_OUTPUT not in stderr:
        raise RuntimeError(
            "OpenVMM's stderr lacks the byte channel's bytes "
            f"{LIFECYCLE_BYTE_CHANNEL_OUTPUT.hex(' ')}, in order; it ends with "
            f"{bytes(stderr[-256:])!r}"
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


# The ignored nvx-rs test that drives the managed lifecycle of a guest whose
# init hands the boot to the managed agent: nvx-rs/tests/managed_lifecycle.rs.
MANAGED_LIFECYCLE_TEST = "managed_lifecycle"


def managed_lifecycle_test_command(cargo: str) -> list[str]:
    """Return the Cargo command that runs MANAGED_LIFECYCLE_TEST."""
    return [
        cargo,
        "test",
        "--manifest-path",
        os.fspath(nvx_rs.DIRECTORY / "Cargo.toml"),
        "--locked",
        "--test",
        MANAGED_LIFECYCLE_TEST,
        "--",
        "--ignored",
        "--nocapture",
    ]


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
    """Run the managed lifecycle of a guest whose init hands the boot to the
    managed agent.

    The nvx-rs test MANAGED_LIFECYCLE_TEST starts OpenVMM with the command
    built here and drives the guest's control session with nvx-rs, so that no
    Python client of the control protocol remains. This checks what the test
    leaves: OpenVMM's outcome report and the guest console.
    """
    cargo = shutil.which("cargo")
    if cargo is None:
        raise ScriptError(
            "the managed-lifecycle scenario runs the nvx-rs test "
            f"{MANAGED_LIFECYCLE_TEST} with Cargo, which is not on PATH; "
            "install Rust (https://rustup.rs)"
        )
    # Cargo runs the test in the nvx-rs directory, so every path is absolute.
    output_dir = output_dir.resolve()
    with tempfile.TemporaryDirectory(prefix="nvx-managed-lifecycle-") as temporary:
        root = Path(temporary).resolve()
        endpoint_value = (
            str(root / "control.sock"),
            f"//./pipe/openvmm-microvm-{uuid.uuid4().hex}",
        )[os.name == "nt"]
        report_path = root / "managed-outcome.json"
        # The test copies the guest console, which listens beside the control
        # endpoint, to this log.
        console_endpoint = _console_endpoint(root)
        guest_log_path = output_dir / "managed-lifecycle-guest.log"
        guest_log_path.unlink(missing_ok=True)
        command = workload_boot_command(
            executable.resolve(),
            backend,
            kernel.resolve(),
            initrd.resolve(),
            memory_mib,
            "quiet loglevel=0",
            boot_console=False,
        )
        command.extend(
            (
                "--microvm-workload-identity",
                "65534:65534",
                "--microvm-lifecycle",
                "managed",
                "--virtio-console",
                f"listen={console_endpoint}",
                "--microvm-control-console",
                f"listen={endpoint_value}",
                "--microvm-control-auth-stdin",
                "--microvm-report",
                str(report_path),
            )
        )
        environment = os.environ.copy()
        # The test records OpenVMM's process ID for an adversarial campaign
        # as the microVM scenarios do, but from the nvx-rs directory, so a
        # relative journal resolves here.
        journal = environment.get("NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL")
        if journal:
            environment["NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL"] = os.path.abspath(
                journal
            )
        environment.update(
            {
                "OPENVMM_LOG": "off",
                "NVX_MANAGED_LIFECYCLE_COMMAND": json.dumps(command),
                "NVX_MANAGED_LIFECYCLE_ENDPOINT": endpoint_value,
                "NVX_MANAGED_LIFECYCLE_CONSOLE": console_endpoint,
                "NVX_MANAGED_LIFECYCLE_TIMEOUT": str(timeout),
                "NVX_MANAGED_LIFECYCLE_OUTPUT_DIR": os.fspath(output_dir),
            }
        )
        test_command = managed_lifecycle_test_command(cargo)
        print(f">> {shlex.join(test_command)}", flush=True)
        process = subprocess.Popen(
            test_command,
            cwd=BuildConstants.REPO_ROOT,
            env=environment,
        )
        try:
            returncode = process.wait()
        except BaseException:
            # A signal that reached nvx.py alone, such as a SIGINT, leaves
            # Cargo's descendants running: the test and the OpenVMM that it
            # started, which use this temporary directory. End and reap them
            # all before it goes, as nvx.py ended the OpenVMM that it started.
            terminate_process_tree(process, process_group=False)
            raise
        # The guest console has no shell to query: init hands the boot to the
        # managed agent, which serves the control console. The monitor still
        # fails the run on a violation or failed check printed there, before
        # the test's own failure, which it may explain. A failed boot check
        # powers the guest off with status 193, which fails the test.
        try:
            guest_output = guest_log_path.read_bytes()
        except FileNotFoundError:
            guest_output = b""
        monitor = TimeAbiMonitor(command)
        monitor.feed(guest_output)
        monitor.finish()
        if returncode != 0:
            raise RuntimeError(
                f"the managed lifecycle test {MANAGED_LIFECYCLE_TEST} failed with "
                f"exit status {returncode}; see its output above and "
                f"{output_dir / 'managed-lifecycle.log'}"
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
    """Check that a guest-requested exit keeps the guest's last output and its
    exit status.

    Each run restores a guest that writes a payload and a marker to one of its
    consoles and then exits, while the host delays its reads, so that OpenVMM's
    relay of that console lags behind the guest. The exit must write the whole
    output of the boot console and of the byte channel alike."""
    expected = (
        b"x" * CONSOLE_EXIT_PAYLOAD_BYTES
        + b"\n"
        + CONSOLE_EXIT_COMPLETION_MARKER
        + b"\n"
    )
    with tempfile.TemporaryDirectory(prefix="nvx-console-exit-") as temporary:
        for exit_code, console in CONSOLE_EXIT_RUNS:
            name = f"console-exit-{processors}vcpu-{console}-status-{exit_code}"
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
                    CONSOLE=f"/dev/{console}",
                    PAYLOAD_BYTES=str(CONSOLE_EXIT_PAYLOAD_BYTES),
                    COMPLETION_MARKER=CONSOLE_EXIT_COMPLETION_MARKER.decode(),
                    EXIT_CODE=str(exit_code),
                ),
                log_path=output_dir / f"{name}-capture.log",
            )
            restore_command = snapshot_restore_command(
                executable, backend, snapshot_path, processors=processors
            )
            with OpenvmmProcess(
                restore_command,
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
    output = result["output"].encode("utf-8")
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
        # the CPUs; the status query alone doesn't check the CPU count. The
        # query ran before the script, so its lines are in the transcript.
        monitor = TimeAbiMonitor(command)
        monitor.feed(result["text"].encode("utf-8"))
        monitor.finish()
        try:
            monitor.require_boot("the counting-LAPIC check", online_cpus=processors)
        except TimeAbiFailure as error:
            raise RuntimeError(f"{processors}-vCPU smp-lapic: {error}") from error
    check_warp_probe(
        result["output"],
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
    result = run_guest_script(
        command,
        f"{TIME_ABI_EXHAUSTIVE_COMMAND}; "
        f'echo "{TIME_ABI_EXHAUSTIVE_EXIT_PREFIX}$?"; '
        f"echo {TIME_ABI_EXHAUSTIVE_COMPLETION_MARKER.decode()}; nvx-exit 0\n",
        TIME_ABI_EXHAUSTIVE_COMPLETION_MARKER,
        timeout=timeout,
        log_path=log_path,
    )
    _check_exhaustive_report(result["output"], processors=processors)


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


def command_line_limit_command(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    memory_mib: int,
    padding: int,
) -> list[str]:
    """Return the command line limit scenario's OpenVMM command, whose caller
    tokens include a padding token with a value of ``padding`` bytes."""
    if padding < 1:
        raise ValueError("the command-line padding must hold at least one byte")
    return workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        f"quiet loglevel=0 {COMMAND_LINE_PADDING}{'x' * padding}",
        network=COMMAND_LINE_LIMIT_NETWORK,
    )


def run_command_line_limit(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Boot a kernel command line that fills the guest kernel's buffer.

    OpenVMM appends its own tokens after the caller's, ending with the
    network bootstrap, so they are the first that the guest kernel would cut
    from a command line that does not fit. A first boot with a one-byte
    padding token learns the effective command line from the guest. A second
    boot pads the caller's tokens until the effective command line holds
    KERNEL_COMMAND_LINE_SIZE - 1 bytes; the guest's /proc/cmdline must equal
    the first one with the longer padding, and the guest must act on the last
    token. OpenVMM must refuse a command line one byte longer before boot.
    """

    def boot(padding: int, log_name: str) -> str:
        result = run_guest_script(
            command_line_limit_command(
                executable, kernel, initrd, backend, memory_mib, padding
            ),
            _read_script("command-line-limit.sh"),
            COMMAND_LINE_LIMIT_COMPLETION_MARKER,
            timeout=timeout,
            log_path=output_dir / log_name,
        )
        return _single_marker_value(
            result["output"].encode("utf-8"), COMMAND_LINE_PREFIX
        ).decode("utf-8")

    padding_token = f"{COMMAND_LINE_PADDING}x"
    shortest = boot(1, "command-line-limit-shortest.log")
    tokens = shortest.split(" ")
    if tokens.count(padding_token) != 1:
        raise RuntimeError(
            f"the guest's kernel command line lacks the padding token: {shortest!r}"
        )
    padding = KERNEL_COMMAND_LINE_SIZE - len(shortest.encode("utf-8"))
    if padding < 1:
        raise RuntimeError(
            f"the shortest kernel command line already holds {len(shortest)} bytes"
        )
    expected = " ".join(
        f"{COMMAND_LINE_PADDING}{'x' * padding}" if token == padding_token else token
        for token in tokens
    )
    longest = boot(padding, "command-line-limit.log")
    if longest != expected or len(longest.encode("utf-8")) != (
        KERNEL_COMMAND_LINE_SIZE - 1
    ):
        raise RuntimeError(
            f"the guest received a {len(longest)}-byte kernel command line ending "
            f"{longest[-80:]!r}, not the expected {len(expected)}-byte one ending "
            f"{expected[-80:]!r}"
        )

    with OpenvmmProcess(
        command_line_limit_command(
            executable, kernel, initrd, backend, memory_mib, padding + 1
        ),
        output_dir / "command-line-limit-over.log",
    ) as process:
        result = process.wait(timeout)
    if (
        result.returncode == 0
        or BOOT_MARKER in result.output
        or COMMAND_LINE_LIMIT_ERROR not in result.output
    ):
        raise RuntimeError(
            "OpenVMM did not refuse a kernel command line one byte over the guest "
            "kernel's limit before boot"
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
) -> EgressRules:
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

    def compile_file(path: Path) -> EgressRules:
        path.write_text(json.dumps({"allow": allow, "deny": deny}), encoding="utf-8")
        return compile_egress_policy(path)

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
            *nvx_rs.command_line(("run",)),
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
                    "interface": "nvx-rs run",
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
                *nvx_rs.command_line(("run",)),
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
            guest_reports = _egress_probe_reports(result["output"])
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
                    "interface": "nvx-rs run",
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
            *nvx_rs.command_line(("run",)),
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
            native = compile_egress_policy(policy_path)
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
            guest_reports = _egress_probe_reports(result["output"])
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
        # starts OpenVMM, with the code of a refused request, naming the field.
        destination = [{"cidr": f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}/32"}]
        for name, policy_rule, field in (
            (
                "reversed-end-port",
                {
                    "to": destination,
                    "ports": [{"protocol": "tcp", "port": last, "endPort": first}],
                },
                "allow[0].ports[0].endPort",
            ),
            (
                "end-port-without-port",
                {"to": destination, "ports": [{"protocol": "udp", "endPort": last}]},
                "allow[0].ports[0].endPort",
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
            reported = nvx_rs.reported_error(rejected_run.stderr)
            if (
                rejected_run.returncode == 0
                or reported is None
                or reported.code != "unsupported"
                or field not in reported.message
            ):
                raise RuntimeError(
                    f"invalid L3/L4 port range policy {name} was not rejected "
                    "before launch"
                )

        (output_dir / "l3-l4-egress-port-ranges-results.json").write_text(
            json.dumps(
                {"interface": "nvx-rs run", "ports": ports, "cases": results},
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
            native = compile_egress_policy(policy_path)
            command = [
                *nvx_rs.command_line(("run",)),
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
            guest_reports = _egress_probe_reports(result["output"])
            _check_egress_case(
                "IPv6 egress", case, IPV6_EGRESS_PROBES, guest_reports, host_reached
            )
            dns = _ipv6_egress_dns_report(result["output"])
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
                    "interface": "nvx-rs run",
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


HOST_LOOPBACK_HTTP_REQUEST_LIMIT = 8192


def _read_http_request(connection: socket.socket, timeout: float) -> bytes:
    """Read a request up to its blank line, however TCP splits it.

    A request can arrive in several reads, so one ``recv`` is not enough. The
    read stops at the header terminator, at the peer's close, or when the
    request is too large or the deadline passes.
    """
    deadline = time.monotonic() + timeout
    request = bytearray()
    while b"\r\n\r\n" not in request:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("host-loopback HTTP request did not complete in time")
        connection.settimeout(remaining)
        chunk = connection.recv(4096)
        if not chunk:
            break
        request.extend(chunk)
        if len(request) > HOST_LOOPBACK_HTTP_REQUEST_LIMIT:
            raise RuntimeError("host-loopback HTTP request exceeds its size limit")
    return bytes(request)


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
            request = _read_http_request(connection, timeout)
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


def _udp_echo_server(
    server: socket.socket, stop: threading.Event, errors: list[Exception]
) -> None:
    """Answer each echo request that ``server`` receives until ``stop`` is set."""
    try:
        server.settimeout(0.05)
        while not stop.is_set():
            try:
                request, peer = server.recvfrom(4096)
            except (TimeoutError, ConnectionResetError):
                # Windows reports a datagram that a closed port refused as a
                # reset on the next receive.
                continue
            if request != HOST_LOOPBACK_UDP_ECHO_REQUEST:
                raise RuntimeError(
                    f"unexpected host-loopback UDP echo request: {request!r}"
                )
            server.sendto(HOST_LOOPBACK_UDP_ECHO_REPLY, peer)
    except Exception as error:
        errors.append(error)


def _bind_host_loopback_udp_echo(
    family: int, address: str, published: Sequence[int]
) -> socket.socket:
    """Bind a UDP socket to an ephemeral port of ``address`` outside ``published``.

    The probes that drew the guest ports that OpenVMM publishes released them,
    so an ephemeral bind can take one, keep OpenVMM from publishing it, and
    receive the host's datagrams for the guest. Each rejected socket holds its
    port until the search ends, so no later attempt draws that port again.
    """
    attempts: list[socket.socket] = []
    try:
        for _ in range(HOST_LOOPBACK_PORT_BIND_ATTEMPTS):
            udp_echo = socket.socket(family, socket.SOCK_DGRAM)
            attempts.append(udp_echo)
            if family == socket.AF_INET6:
                udp_echo.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            udp_echo.bind((address, 0))
            if int(udp_echo.getsockname()[1]) not in published:
                return attempts.pop()
    finally:
        for rejected in attempts:
            rejected.close()
    raise RuntimeError(
        f"failed to bind a UDP port on {address} outside guest ports "
        f"{sorted(published)} after {HOST_LOOPBACK_PORT_BIND_ATTEMPTS} attempts"
    )


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
                GATEWAY_IPV6=DIRECTIONAL_NETWORK_GATEWAY_IPV6,
                GENERAL_PORT=str(denied_general_port),
                PROXY_PORT=str(proxy_port),
                GUEST_PORT="0",
                PRIVATE_GUEST_PORT="0",
                UDP_IPV6_GUEST_PORT="0",
                UDP_ECHO_PORT="0",
                UDP_ECHO_IPV6_PORT="0",
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
                GATEWAY_IPV6=DIRECTIONAL_NETWORK_GATEWAY_IPV6,
                GENERAL_PORT=str(denied_general_port),
                PROXY_PORT=str(proxy_port),
                GUEST_PORT="0",
                PRIVATE_GUEST_PORT="0",
                UDP_IPV6_GUEST_PORT="0",
                UDP_ECHO_PORT="0",
                UDP_ECHO_IPV6_PORT="0",
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
    guest_port = _host_loopback_publication_port()
    while True:
        # Busybox's UDP server answers only its first peer, so the guest
        # answers UDP over IPv6 loopback on a port of its own.
        udp_ipv6_guest_port = _host_loopback_publication_port()
        _, host_forward_port = _available_tcp_address()
        _, private_guest_port = _available_tcp_address()
        ports = {guest_port, udp_ipv6_guest_port, host_forward_port, private_guest_port}
        if len(ports) == 4:
            break
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
    udp_echoes: list[socket.socket] = []
    udp_echo_servers: list[threading.Thread] = []
    udp_echo_errors: list[Exception] = []
    udp_echo_stop = threading.Event()
    try:
        # Host UDP services on IPv4 and IPv6 loopback, which the guest reaches
        # at its gateway. The guest's UDP servers are dual-stack, so OpenVMM
        # publishes both of their ports on both loopback addresses, and
        # neither service may hold either port.
        for family, address in (
            (socket.AF_INET, "127.0.0.1"),
            (socket.AF_INET6, "::1"),
        ):
            udp_echo = _bind_host_loopback_udp_echo(
                family, address, (guest_port, udp_ipv6_guest_port)
            )
            udp_echoes.append(udp_echo)
            udp_echo_server = threading.Thread(
                target=_udp_echo_server,
                args=(udp_echo, udp_echo_stop, udp_echo_errors),
                name="nvx-host-loopback-udp-echo",
                daemon=True,
            )
            udp_echo_server.start()
            udp_echo_servers.append(udp_echo_server)
        udp_echo_port, udp_echo_ipv6_port = (
            int(udp_echo.getsockname()[1]) for udp_echo in udp_echoes
        )
        allow_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            network=DIRECTIONAL_NETWORK_CIDR,
        )
        # Egress admits only the guest's own connection to the general host
        # service and its datagrams to the UDP services, so the guest's
        # replies to the host pass because host loopback admits them, not
        # because egress does.
        allow_command.extend(
            (
                "--host-loopback",
                "allow",
                "--network-ingress",
                "deny",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}/32:tcp:{allowed_general_port}",
                "--network-egress-allow",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV4}/32:udp:{udp_echo_port}",
                "--network-egress-allow",
                f"{DIRECTIONAL_NETWORK_GATEWAY_IPV6}/128:udp:{udp_echo_ipv6_port}",
                "--host-loopback-forward",
                f"tcp:{host_forward_port}:{guest_port}",
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
                    GATEWAY_IPV6=DIRECTIONAL_NETWORK_GATEWAY_IPV6,
                    GENERAL_PORT=str(allowed_general_port),
                    PROXY_PORT="0",
                    GUEST_PORT=str(guest_port),
                    PRIVATE_GUEST_PORT=str(private_guest_port),
                    UDP_IPV6_GUEST_PORT=str(udp_ipv6_guest_port),
                    UDP_ECHO_PORT=str(udp_echo_port),
                    UDP_ECHO_IPV6_PORT=str(udp_echo_ipv6_port),
                ),
            )
            process.wait_for_line(HOST_LOOPBACK_INGRESS_READY_MARKER, timeout)
            _verify_host_loopback_publication(
                guest_port,
                udp_ipv6_guest_port,
                host_forward_port,
                private_guest_port,
                timeout,
            )
            process.send_line(HOST_LOOPBACK_CHECKED.decode())
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
        if udp_echo_errors:
            raise RuntimeError(
                "host-loopback UDP echo service failed"
            ) from udp_echo_errors[0]
    finally:
        allowed_general.close()
        allow_server.join(timeout=NETWORK_SERVER_JOIN_TIMEOUT_SECONDS)
        udp_echo_stop.set()
        for udp_echo_server in udp_echo_servers:
            udp_echo_server.join(timeout=NETWORK_SERVER_JOIN_TIMEOUT_SECONDS)
        for udp_echo in udp_echoes:
            udp_echo.close()

    run_host_loopback_rejections(
        executable,
        kernel,
        initrd,
        backend,
        memory_mib=memory_mib,
        timeout=timeout,
        output_dir=output_dir,
    )


def _host_loopback_publication_port() -> int:
    """Draw a port that OpenVMM can publish on host loopback for TCP and UDP.

    The port is free on every host address too, which Windows checks before
    it binds a loopback address.
    """
    last_error: OSError | None = None
    for port in _host_loopback_port_candidates():
        with ExitStack() as sockets:
            try:
                for family, address in (
                    (socket.AF_INET, "0.0.0.0"),
                    (socket.AF_INET6, "::"),
                ):
                    for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
                        probe = socket.socket(family, kind)
                        sockets.callback(probe.close)
                        if family == socket.AF_INET6:
                            probe.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
                        probe.bind((address, port))
            except OSError as error:
                last_error = error
                continue
        return port
    raise RuntimeError(
        "failed to find a port free for TCP and UDP on every host address"
    ) from last_error


def _holds_loopback(family: int, kind: socket.SocketKind, port: int) -> bool:
    """Return whether a host socket holds ``port`` on the loopback address of
    ``family``, as OpenVMM's listener for a guest port published there does."""
    probe = socket.socket(family, kind)
    try:
        if family == socket.AF_INET6:
            probe.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        probe.bind(("::1" if family == socket.AF_INET6 else "127.0.0.1", port))
    except OSError:
        return True
    finally:
        probe.close()
    return False


def _host_loopback_tcp_exchange(address: str, port: int, timeout: float) -> None:
    """Exchange a line with the guest's TCP server through ``address:port``.

    The guest serves one client at a time, and may withdraw the published port
    between clients, so a connection that ends before the server's line is
    retried.
    """
    family = socket.AF_INET6 if ":" in address else socket.AF_INET
    deadline = time.monotonic() + timeout
    while True:
        reply = b""
        try:
            with socket.socket(family, socket.SOCK_STREAM) as client:
                client.settimeout(min(timeout, 5))
                client.connect((address, port))
                while not reply.endswith(b"\n"):
                    chunk = client.recv(4096)
                    if not chunk:
                        break
                    reply += chunk
                if reply == HOST_LOOPBACK_TCP_ACK:
                    client.sendall(HOST_LOOPBACK_TCP_REQUEST)
                    client.shutdown(socket.SHUT_WR)
                    return
        except OSError as error:
            if time.monotonic() >= deadline:
                raise TimeoutError(
                    f"guest TCP server did not answer through {address}:{port}"
                ) from error
        if reply:
            raise RuntimeError(f"unexpected guest TCP reply: {reply!r}")
        if time.monotonic() >= deadline:
            raise TimeoutError(
                f"guest TCP server did not answer through {address}:{port}"
            )
        time.sleep(0.1)


def _host_loopback_udp_exchange(address: str, port: int, timeout: float) -> None:
    """Exchange a datagram with the guest's UDP server through ``address:port``.

    The request is repeated until the guest answers it, from the port that it
    reached, since the guest may still be resolving the address at which it
    sees the host when the first request arrives.
    """
    family = socket.AF_INET6 if ":" in address else socket.AF_INET
    with socket.socket(family, socket.SOCK_DGRAM) as client:
        client.bind((address, 0))
        client.settimeout(0.5)
        deadline = time.monotonic() + timeout
        while True:
            client.sendto(HOST_LOOPBACK_UDP_REQUEST, (address, port))
            try:
                reply, source = client.recvfrom(4096)
                break
            except (TimeoutError, ConnectionResetError) as error:
                if time.monotonic() >= deadline:
                    raise TimeoutError(
                        f"guest UDP server did not answer through {address}:{port}"
                    ) from error
    if reply != HOST_LOOPBACK_UDP_REPLY or tuple(source[:2]) != (address, port):
        raise RuntimeError(f"unexpected guest UDP reply {reply!r} from {source}")


def _verify_host_loopback_publication(
    guest_port: int,
    udp_ipv6_guest_port: int,
    host_forward_port: int,
    private_guest_port: int,
    timeout: float,
) -> None:
    """Verify from the host what explicit host-loopback access publishes."""
    _wait_until(
        lambda: (
            _holds_loopback(socket.AF_INET, socket.SOCK_STREAM, guest_port)
            and _holds_loopback(socket.AF_INET6, socket.SOCK_STREAM, guest_port)
            and _holds_loopback(socket.AF_INET, socket.SOCK_DGRAM, guest_port)
            and _holds_loopback(socket.AF_INET6, socket.SOCK_DGRAM, udp_ipv6_guest_port)
        ),
        timeout,
        f"guest ports {guest_port} and {udp_ipv6_guest_port} were not published "
        "on host loopback",
    )
    # The UDP listener was announced after every other one had started, so a
    # guest-loopback listener, had it been announced, would be published now.
    for family in (socket.AF_INET, socket.AF_INET6):
        if _holds_loopback(family, socket.SOCK_STREAM, private_guest_port):
            raise RuntimeError("a listener on guest loopback was published")
    for address in _host_ipv4_addresses():
        try:
            unexpected = socket.create_connection((address, guest_port), timeout=1)
        except OSError:
            continue
        unexpected.close()
        raise RuntimeError(
            f"host-loopback access published guest port {guest_port} on {address}"
        )
    for address, port in (
        ("127.0.0.1", guest_port),
        ("::1", guest_port),
        ("127.0.0.1", host_forward_port),
    ):
        _host_loopback_tcp_exchange(address, port, timeout)
    for address, port in (
        ("127.0.0.1", guest_port),
        ("::1", udp_ipv6_guest_port),
    ):
        _host_loopback_udp_exchange(address, port, timeout)


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
    for name, network, extra, expected in (
        (
            "allow-snapshot",
            DIRECTIONAL_NETWORK_CIDR,
            ("--host-loopback", "allow", "--snapshot-destination"),
            b"microVM snapshots do not support --host-loopback allow",
        ),
        (
            "allow-narrow-network",
            HOST_LOOPBACK_NARROW_NETWORK_CIDR,
            ("--host-loopback", "allow"),
            b"--host-loopback allow requires an IPv4 network of /29 or larger",
        ),
        (
            "deny-forward",
            DIRECTIONAL_NETWORK_CIDR,
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
            DIRECTIONAL_NETWORK_CIDR,
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
            network=network,
        )
        invalid.extend(extra)
        with tempfile.TemporaryDirectory(prefix="nvx-loopback-rejection-") as temporary:
            pidfile = Path(temporary) / "absent" / "openvmm.pid"
            if name == "allow-snapshot":
                invalid.extend(
                    (
                        str(Path(temporary) / "absent" / "snapshot"),
                        "--pidfile",
                        str(pidfile),
                    )
                )
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


def _private_ingress_port() -> int:
    """Draw a port that the host can publish for both TCP and UDP."""
    last_error: OSError | None = None
    for port in _host_loopback_port_candidates():
        with ExitStack() as sockets:
            try:
                for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
                    probe = socket.socket(socket.AF_INET, kind)
                    sockets.callback(probe.close)
                    probe.bind(("0.0.0.0", port))
            except OSError as error:
                last_error = error
                continue
        return port
    raise RuntimeError(
        "failed to find a port free for TCP and UDP on every host address"
    ) from last_error


def _publishes(port: int, kind: socket.SocketKind) -> bool:
    """Return whether a host socket holds ``port`` on every IPv4 address.

    During the scenario, that socket is OpenVMM's listener for the guest port.
    """
    probe = socket.socket(socket.AF_INET, kind)
    try:
        probe.bind(("0.0.0.0", port))
    except OSError:
        return True
    finally:
        probe.close()
    return False


def _wait_until(condition: Callable[[], bool], timeout: float, failure: str) -> None:
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() >= deadline:
            raise TimeoutError(failure)
        time.sleep(0.05)


def _host_ipv4_addresses() -> list[str]:
    """Return the host's own non-loopback IPv4 addresses that are known."""
    addresses: set[str] = set()
    with suppress(OSError):
        for info in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET):
            addresses.add(str(info[4][0]))
    with suppress(OSError), socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
        # Connecting a UDP socket sends nothing; it selects the source address
        # of the default route.
        route.connect(("192.0.2.1", 9))
        addresses.add(route.getsockname()[0])
    return sorted(
        address
        for address in addresses
        if not ipaddress.ip_address(address).is_loopback
    )


def _udp_client(timeout: float) -> socket.socket:
    client = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    client.bind(("127.0.0.1", 0))
    client.settimeout(timeout)
    return client


def _private_ingress_command(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    memory_mib: int,
    *policy: str,
) -> list[str]:
    command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0",
        network=DIRECTIONAL_NETWORK_CIDR,
    )
    command.extend(policy)
    return command


def _run_private_ingress_guest(
    command: Sequence[str],
    *,
    ingress: str,
    expect: str,
    port: int,
    outbound_port: int,
    completion_marker: bytes,
    timeout: float,
    log_path: Path,
    probe: Callable[[], None],
) -> None:
    """Boot a guest that serves TCP and UDP on ``port``, run ``probe`` from the
    host while it listens, and require the guest to observe ``expect``."""
    with OpenvmmProcess(command, log_path) as process:
        process.wait_for(BOOT_MARKER, timeout)
        _stage_script(
            process,
            "/tmp/nvx-private-network-ingress",
            "NVX_PRIVATE_NETWORK_INGRESS",
            _render_script(
                "private-network-ingress.sh.in",
                INGRESS=ingress,
                EXPECT=expect,
                TCP_PORT=str(port),
                UDP_PORT=str(port),
                GATEWAY_IPV4=DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                OUTBOUND_PORT=str(outbound_port),
                COMPLETION_MARKER=completion_marker.decode(),
            ),
        )
        process.wait_for_line(PRIVATE_INGRESS_LISTENING_MARKER, timeout)
        probe()
        process.send_line(PRIVATE_INGRESS_CHECKED.decode())
        process.wait_for_line(completion_marker, timeout)
        result = process.wait(timeout)
    if result.returncode != 0:
        raise RuntimeError(
            f"private-network ingress guest exited with {result.returncode}"
        )


def run_private_network_ingress(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Verify that ``--network-ingress allow`` publishes the guest's listeners.

    A single host has no private-network peer other than itself, which the
    published listeners refuse unless host loopback is allowed. The scenario
    therefore exchanges TCP and UDP with the guest's servers from the host with
    host loopback allowed and egress denied, which also requires the guest's
    replies to pass; it requires the host's own connections to be refused with
    host loopback denied, and nothing to be published with ingress denied.
    Unit tests classify the peers of other hosts.
    """
    port = _private_ingress_port()
    outbound = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    outbound.bind(("127.0.0.1", 0))
    outbound.listen(1)
    outbound_port = int(outbound.getsockname()[1])

    def exchange() -> None:
        _wait_until(
            lambda: (
                _publishes(port, socket.SOCK_STREAM)
                and _publishes(port, socket.SOCK_DGRAM)
            ),
            timeout,
            f"guest port {port} was not published to the host",
        )
        with socket.create_connection(("127.0.0.1", port), timeout=timeout) as client:
            client.settimeout(timeout)
            reply = b""
            while not reply.endswith(b"\n"):
                chunk = client.recv(4096)
                if not chunk:
                    break
                reply += chunk
            if reply != PRIVATE_INGRESS_TCP_REPLY:
                raise RuntimeError(f"unexpected guest TCP reply: {reply!r}")
            client.sendall(PRIVATE_INGRESS_TCP_REQUEST)
            client.shutdown(socket.SHUT_WR)
        with _udp_client(0.5) as client:
            deadline = time.monotonic() + timeout
            while True:
                client.sendto(PRIVATE_INGRESS_UDP_REQUEST, ("127.0.0.1", port))
                try:
                    reply, source = client.recvfrom(4096)
                    break
                except (TimeoutError, ConnectionResetError):
                    if time.monotonic() >= deadline:
                        raise
            if reply != PRIVATE_INGRESS_UDP_REPLY or source != ("127.0.0.1", port):
                raise RuntimeError(
                    f"unexpected guest UDP reply {reply!r} from {source}"
                )
        # The guest's TCP server accepted its one client and closed, so its port
        # is withdrawn, while the UDP server still uses its own.
        _wait_until(
            lambda: not _publishes(port, socket.SOCK_STREAM),
            timeout,
            f"closed guest TCP port {port} remained published",
        )

    try:
        _run_private_ingress_guest(
            _private_ingress_command(
                executable,
                kernel,
                initrd,
                backend,
                memory_mib,
                "--network-egress",
                "deny",
                "--network-ingress",
                "allow",
                "--host-loopback",
                "allow",
            ),
            ingress="allow",
            expect="reached",
            port=port,
            outbound_port=outbound_port,
            completion_marker=PRIVATE_INGRESS_REACHED_MARKER,
            timeout=timeout,
            log_path=output_dir / "private-network-ingress-host-allowed.log",
            probe=exchange,
        )
        outbound.settimeout(NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS)
        try:
            unexpected, _ = outbound.accept()
        except TimeoutError:
            pass
        else:
            unexpected.close()
            raise RuntimeError("denied egress opened a guest connection to the host")
    finally:
        outbound.close()

    port = _private_ingress_port()

    def refused() -> None:
        _wait_until(
            lambda: (
                _publishes(port, socket.SOCK_STREAM)
                and _publishes(port, socket.SOCK_DGRAM)
            ),
            timeout,
            f"guest port {port} was not published to the host",
        )
        # The host completes the handshake with the published listener, which
        # then resets connections from the host itself.
        for address in ["127.0.0.1", *_host_ipv4_addresses()]:
            with socket.create_connection((address, port), timeout=timeout) as client:
                client.settimeout(timeout)
                try:
                    data = client.recv(4096)
                except (ConnectionResetError, ConnectionAbortedError):
                    data = b""
                if data:
                    raise RuntimeError(
                        f"a refused connection from {address} reached the guest: {data!r}"
                    )
        with _udp_client(NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS) as client:
            for _ in range(3):
                client.sendto(PRIVATE_INGRESS_UDP_REQUEST, ("127.0.0.1", port))
                try:
                    reply = client.recv(4096)
                except (TimeoutError, ConnectionResetError):
                    continue
                raise RuntimeError(f"a refused datagram drew a reply: {reply!r}")

    _run_private_ingress_guest(
        _private_ingress_command(
            executable,
            kernel,
            initrd,
            backend,
            memory_mib,
            "--network-egress",
            "deny",
            "--network-ingress",
            "allow",
            "--host-loopback",
            "deny",
        ),
        ingress="allow",
        expect="unreached",
        port=port,
        outbound_port=0,
        completion_marker=PRIVATE_INGRESS_REFUSED_MARKER,
        timeout=timeout,
        log_path=output_dir / "private-network-ingress-host-denied.log",
        probe=refused,
    )

    port = _private_ingress_port()

    def unpublished() -> None:
        # Longer than the guest needs to announce a UDP listener.
        time.sleep(2)
        if _publishes(port, socket.SOCK_STREAM) or _publishes(port, socket.SOCK_DGRAM):
            raise RuntimeError("ingress deny published a guest port")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=timeout):
                pass
        except OSError:
            return
        raise RuntimeError("ingress deny accepted a connection toward the guest")

    _run_private_ingress_guest(
        _private_ingress_command(
            executable,
            kernel,
            initrd,
            backend,
            memory_mib,
            "--network-egress",
            "allow",
            "--network-ingress",
            "deny",
        ),
        ingress="deny",
        expect="unreached",
        port=port,
        outbound_port=0,
        completion_marker=PRIVATE_INGRESS_UNPUBLISHED_MARKER,
        timeout=timeout,
        log_path=output_dir / "private-network-ingress-denied.log",
        probe=unpublished,
    )

    with tempfile.TemporaryDirectory(prefix="nvx-ingress-snapshot-") as temporary:
        invalid = _private_ingress_command(
            executable,
            kernel,
            initrd,
            backend,
            memory_mib,
            "--network-ingress",
            "allow",
            "--snapshot-destination",
            str(Path(temporary) / "snapshot"),
        )
        with OpenvmmProcess(
            invalid,
            output_dir / "private-network-ingress-snapshot.log",
        ) as process:
            result = process.wait(timeout)
    if (
        result.returncode == 0
        or b"microVM snapshots do not support --network-ingress allow"
        not in result.output
        or BOOT_MARKER in result.output
    ):
        raise RuntimeError(
            "a snapshot with private-network ingress was not rejected before boot"
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


def assert_live_exchange(root: Path) -> None:
    """Check the two files and two directories that the live filesystem stage
    swapped with `RENAME_EXCHANGE` right before its capture.

    Linux hosts pass the flag to `renameat2`, so each pair traded names.
    Windows hosts reject every rename flag with `EINVAL` before changing
    anything, so each name still holds what the guest created there.
    """
    at_a, at_b = ("B", "A") if sys.platform == "linux" else ("A", "B")
    expected = {
        "exchange-a": f"NVX-EXCHANGE-{at_a}",
        "exchange-b": f"NVX-EXCHANGE-{at_b}",
        "exchange-dir-a/entry": f"NVX-EXCHANGE-DIR-{at_a}",
        "exchange-dir-b/entry": f"NVX-EXCHANGE-DIR-{at_b}",
    }
    for name, contents in expected.items():
        actual = (root / name).read_bytes()
        if actual != contents.encode():
            raise RuntimeError(
                f"live filesystem exchange left {name} with {actual!r}, "
                f"expected {contents}"
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
        for role in ("distro", "scratch"):
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


def check_virtio_features(output: str) -> None:
    """Check the report of the virtio-features scenario's guest: one line for
    each fixed virtio device of MICROVM_VIRTIO_DEVICES, with its device ID and
    exactly the feature bits that the device offers and Linux negotiates.

    Raises RuntimeError naming each device that is missing, unknown, or
    reported twice, and each feature bit that a device lacks or has besides
    its documented ones."""
    reported: dict[int, tuple[int, frozenset[int]]] = {}
    problems: list[str] = []
    for line in output.splitlines():
        line = line.strip()
        if not line.startswith(VIRTIO_FEATURES_REPORT_PREFIX):
            continue
        try:
            fields = dict(
                field.split("=", 1)
                for field in line[len(VIRTIO_FEATURES_REPORT_PREFIX) :].split()
            )
            base = int(fields["base"], 16)
            device_id = int(fields["device"], 16)
            bits = fields["features"]
            if not bits or set(bits) - {"0", "1"}:
                raise ValueError(f"features {bits!r} are not a bit string")
        except (KeyError, ValueError) as error:
            raise RuntimeError(
                f"malformed virtio device report {line!r}: {error}"
            ) from error
        if base in reported:
            problems.append(f"the device at {base:#x} is reported twice")
        reported[base] = (
            device_id,
            frozenset(bit for bit, value in enumerate(bits) if value == "1"),
        )
    for base, (device_id, features) in sorted(reported.items()):
        expected = MICROVM_VIRTIO_DEVICES.get(base)
        if expected is None:
            problems.append(f"an unexpected virtio device {device_id} is at {base:#x}")
            continue
        if device_id != expected.device_id:
            problems.append(
                f"{expected.name} at {base:#x} is virtio device {device_id}, "
                f"not {expected.device_id}"
            )
        extra = sorted(features - expected.features)
        missing = sorted(expected.features - features)
        if extra:
            problems.append(
                f"{expected.name} at {base:#x} negotiated feature bits "
                f"{', '.join(map(str, extra))} besides its documented ones"
            )
        if missing:
            problems.append(
                f"{expected.name} at {base:#x} lacks feature bits "
                f"{', '.join(map(str, missing))}"
            )
    for base, expected in sorted(MICROVM_VIRTIO_DEVICES.items()):
        if base not in reported:
            problems.append(f"no {expected.name} is reported at {base:#x}")
    if problems:
        raise RuntimeError(
            "the guest-visible virtio features differ from the architecture "
            "guide's Table 2.8: " + "; ".join(problems)
        )


def run_virtio_features(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    log_path: Path,
) -> None:
    """Boot a microVM with every fixed virtio device, the NIC, the dormant
    host filesystem slot, both consoles, and both sandbox blocks, and check
    the features that the guest negotiated with each."""
    with tempfile.TemporaryDirectory(prefix="nvx-virtio-features-") as temporary:
        root = Path(temporary)
        command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            network=VIRTIO_FEATURES_NETWORK,
        )
        command.extend(("--microvm-control-console", "none"))
        for role in ("distro", "scratch"):
            path = root / f"{role}.img"
            with path.open("wb") as disk:
                disk.truncate(SANDBOX_BLOCK_SIZE)
            read_only = ",ro" if role != "scratch" else ""
            command.extend(
                ("--microvm-sandbox-block", f"{role}:file:{path}{read_only}")
            )
        result = run_guest_script(
            command,
            _read_script("virtio-features.sh"),
            VIRTIO_FEATURES_COMPLETION_MARKER,
            timeout=timeout,
            log_path=log_path,
        )
    check_virtio_features(result["output"])


class InterruptMapEntry(NamedTuple):
    """A virtio device's interrupt, as the interrupt-map scenario's guest
    reports it after a step: the device's name, the interrupt that its driver
    requested, that interrupt's controller (chip) and input (hwirq), its
    trigger, its handlers, how many times it fired, and how many of its
    interrupts lately found no work for any handler (``unhandled`` of
    /proc/irq/<n>/spurious).

    The scenario reports that last count but does not check it. Linux keeps
    it to detect a stuck line, and resets it to 1 when 100 ms pass without
    such an interrupt, so it counts no history. A virtio console's driver
    waits for its output by polling the used ring, so nearly every interrupt
    of a console finds no work, and the NIC's polling can leave one such
    interrupt too, such as after a restore. Each device's single handler, its
    IOAPIC input, and its count rising with its own traffic catch an
    interrupt delivered to the wrong line."""

    device: str
    irq: int
    chip: str
    hwirq: int
    trigger: str
    actions: str
    fired: int
    unhandled: int


def parse_interrupt_map(
    output: str,
) -> tuple[dict[str, dict[int, InterruptMapEntry]], dict[str, int]]:
    """Return the interrupt-map scenario's reports in ``output``, by step and
    by the base of each device's window, and the guest's ERR, MIS, and SPU
    counts.

    Raises RuntimeError for a malformed line or a device that a step reports
    twice."""
    steps: dict[str, dict[int, InterruptMapEntry]] = {}
    errors: dict[str, int] = {}
    for line in output.splitlines():
        line = line.strip()
        for prefix in (INTERRUPT_MAP_ERRORS_PREFIX, INTERRUPT_MAP_REPORT_PREFIX):
            if line.startswith(prefix):
                break
        else:
            continue
        try:
            fields = dict(field.split("=", 1) for field in line[len(prefix) :].split())
            if prefix == INTERRUPT_MAP_ERRORS_PREFIX:
                errors = {
                    name: int(fields[name]) for name in INTERRUPT_MAP_ERROR_COUNTS
                }
                continue
            step = fields["step"]
            base = int(fields["base"], 16)
            entry = InterruptMapEntry(
                fields["device"],
                int(fields["irq"]),
                fields["chip"],
                int(fields["hwirq"]),
                fields["type"],
                fields["actions"],
                int(fields["count"]),
                int(fields["unhandled"]),
            )
        except (KeyError, ValueError) as error:
            raise RuntimeError(
                f"malformed interrupt-map report {line!r}: {error}"
            ) from error
        entries = steps.setdefault(step, {})
        if base in entries:
            raise RuntimeError(
                f"the device at {base:#x} is reported twice after step {step}"
            )
        entries[base] = entry
    return steps, errors


def check_interrupt_map(
    steps: dict[str, dict[int, InterruptMapEntry]],
    errors: dict[str, int],
    bases: Collection[int],
    *,
    before: str,
    after: str = "traffic",
) -> None:
    """Check the interrupt-map scenario's reports against the map of the
    architecture guide (2.4.2 and Table 2.7): after the steps ``before`` and
    ``after``, exactly the devices of ``bases`` are present, and each
    interrupts on the IOAPIC input of its window in MICROVM_VIRTIO_DEVICES,
    edge-triggered and with no other handler; each interrupt fired after
    ``before``, with the traffic that the guest drove; and /proc/interrupts
    counts no APIC error or spurious PIC interrupt (ERR), no misrouted IOAPIC
    interrupt (MIS), and no spurious APIC interrupt (SPU).

    Raises RuntimeError naming each departure."""
    problems: list[str] = []
    for step in (before, after):
        entries = steps.get(step)
        if entries is None:
            problems.append(f"no device is reported after step {step}")
            continue
        for base in sorted(set(entries) - set(bases)):
            problems.append(
                f"an unexpected device {entries[base].device} at {base:#x} is "
                f"reported after step {step}"
            )
        for base in sorted(bases):
            expected = MICROVM_VIRTIO_DEVICES[base]
            entry = entries.get(base)
            if entry is None:
                problems.append(
                    f"no {expected.name} is reported at {base:#x} after step {step}"
                )
                continue
            if entry.irq != expected.irq:
                problems.append(
                    f"{expected.name} at {base:#x} interrupts on IRQ {entry.irq}, "
                    f"not {expected.irq}"
                )
            if entry.chip != "IO-APIC" or entry.hwirq != entry.irq:
                problems.append(
                    f"{expected.name}'s IRQ {entry.irq} is input {entry.hwirq} of "
                    f"{entry.chip}, not the IOAPIC input of its number"
                )
            if entry.trigger != "edge":
                problems.append(
                    f"{expected.name}'s IRQ {entry.irq} is {entry.trigger}-triggered"
                )
            if entry.actions != entry.device:
                problems.append(
                    f"{expected.name} shares IRQ {entry.irq}, whose handlers are "
                    f"{entry.actions}"
                )
    if not problems:
        for base in sorted(bases):
            expected = MICROVM_VIRTIO_DEVICES[base]
            first, last = steps[before][base], steps[after][base]
            if last.fired <= first.fired:
                problems.append(
                    f"{expected.name}'s IRQ {first.irq} fired {first.fired} times "
                    f"after step {before} and {last.fired} after step {after}"
                )
    if not errors:
        problems.append("the guest reported no ERR, MIS, and SPU counts")
    elif any(errors.values()):
        problems.append(
            f"/proc/interrupts counts {errors['err']} APIC errors or spurious PIC "
            f"interrupts (ERR), {errors['mis']} misrouted IOAPIC interrupts "
            f"(MIS), and {errors['spu']} spurious APIC interrupts (SPU)"
        )
    if problems:
        raise RuntimeError(
            "the microVM's interrupts differ from the architecture guide's map: "
            + "; ".join(problems)
        )


def _interrupt_map_script(devices: int, *, capture: str) -> str:
    """Return the interrupt-map scenario's guest script for a guest with
    ``devices`` virtio devices, which runs ``capture`` between its boot report
    and its traffic.

    The script writes INTERRUPT_MAP_GUEST_ATTACH to the control console with
    printf, whose format gives each of its bytes as an octal escape."""
    return _render_script(
        "interrupt-map.sh.in",
        DEVICES=str(devices),
        CAPTURE=capture,
        GUEST_ATTACH="".join(f"\\{byte:03o}" for byte in INTERRUPT_MAP_GUEST_ATTACH),
    )


def run_interrupt_map(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Check the interrupt map of the architecture guide (2.4.2 and Table
    2.7) end to end.

    A microVM with every fixed virtio device, the NIC, the host filesystem
    with a share, both consoles, and both sandbox blocks, reports the
    interrupt that each device's driver requested, drives traffic through
    each, and reports the interrupts again: each must be the IOAPIC input of
    its window, past the ISA IRQs, edge-triggered, with no other handler, and
    must have fired, and /proc/interrupts must count no APIC error, misrouted
    IOAPIC interrupt, or spurious interrupt. A snapshot of a microVM with the
    NIC, the share, and the boot console then restores with the same
    interrupts, which fire again after the restore."""
    with tempfile.TemporaryDirectory(prefix="nvx-interrupt-map-") as temporary:
        root = Path(temporary)
        share = root / "share"
        share.mkdir()
        (share / "seed").write_text(INTERRUPT_MAP_SHARE_SEED, encoding="utf-8")
        mount = f"/mnt/share,{share},ro"
        command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
            network=INTERRUPT_MAP_NETWORK,
            mount=mount,
        )
        command.extend(("--microvm-control-console", "none"))
        for role in ("distro", "scratch"):
            path = root / f"{role}.img"
            with path.open("wb") as disk:
                disk.truncate(SANDBOX_BLOCK_SIZE)
            read_only = ",ro" if role != "scratch" else ""
            command.extend(
                ("--microvm-sandbox-block", f"{role}:file:{path}{read_only}")
            )
        result = run_guest_script(
            command,
            _interrupt_map_script(len(MICROVM_VIRTIO_DEVICES), capture=""),
            INTERRUPT_MAP_COMPLETION_MARKER,
            timeout=timeout,
            log_path=output_dir / "interrupt-map.log",
        )
        steps, errors = parse_interrupt_map(result["output"])
        check_interrupt_map(steps, errors, MICROVM_VIRTIO_DEVICES, before="boot")

        snapshot_path = root / "snapshot"
        capture_command = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                network=INTERRUPT_MAP_NETWORK,
                mount=mount,
            ),
            "--snapshot-destination",
            str(snapshot_path),
        ]
        capture_marker = INTERRUPT_MAP_CAPTURE_MARKER.decode()
        with OpenvmmProcess(
            capture_command,
            output_dir / "interrupt-map-capture.log",
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            _stage_script(
                process,
                "/tmp/nvx-interrupt-map",
                "NVX_INTERRUPT_MAP",
                _interrupt_map_script(
                    len(INTERRUPT_MAP_SNAPSHOT_DEVICES),
                    capture=f"echo {capture_marker}\nnvx-snapshot\nreport restore",
                ),
            )
            source = process.wait(timeout)
        if source.returncode != 0:
            raise RuntimeError(
                f"interrupt-map capture source exited with {source.returncode}"
            )
        if _output_lines(source.output).count(INTERRUPT_MAP_CAPTURE_MARKER) != 1:
            raise RuntimeError(
                "interrupt-map source did not reach capture exactly once"
            )
        captured, _ = parse_interrupt_map(
            source.output.decode("utf-8", errors="replace")
        )
        if set(captured) != {"boot"}:
            raise RuntimeError("interrupt-map source crossed its capture boundary")

        restore_command = snapshot_restore_command(
            executable,
            backend,
            snapshot_path,
            network_profile="portable",
        )
        restore_command.extend(("--mount", mount))
        with OpenvmmProcess(
            restore_command,
            output_dir / "interrupt-map-restore.log",
        ) as process:
            process.wait_for(INTERRUPT_MAP_COMPLETION_MARKER, timeout)
            restored = process.wait(timeout)
        if restored.returncode != 0:
            raise RuntimeError(
                f"interrupt-map restore exited with {restored.returncode}"
            )
    steps, errors = parse_interrupt_map(
        restored.output.decode("utf-8", errors="replace")
    )
    if "boot" in steps:
        raise RuntimeError("interrupt-map restore replayed the steps before capture")
    check_interrupt_map(steps, errors, INTERRUPT_MAP_SNAPSHOT_DEVICES, before="restore")
    for base in INTERRUPT_MAP_SNAPSHOT_DEVICES:
        before = captured["boot"].get(base)
        after = steps["restore"][base]
        if before is None or before._replace(fired=0, unhandled=0) != after._replace(
            fired=0, unhandled=0
        ):
            raise RuntimeError(
                f"{MICROVM_VIRTIO_DEVICES[base].name}'s interrupt changed across "
                f"the snapshot: {before} before capture, {after} after restore"
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


def time_host_correction_hook(ppm: int) -> tuple[str, str]:
    """Return OpenVMM's test hook that publishes host UTC as running ``ppm``
    faster than it does, with the clock correction to match."""
    return ("--x-time-abi-test-hook", f"host-rate-offset-ppm={ppm}")


def time_host_correction_script(
    spacing_seconds: int = TIME_HOST_CORRECTION_SPACING_SECONDS,
    settle_seconds: int = TIME_HOST_CORRECTION_PHASE_SETTLE_SECONDS,
) -> str:
    """Return the guest's report of the time-host-correction scenario: its time
    state, then up to ``TIME_HOST_CORRECTION_ATTEMPTS`` pairs of samples
    ``spacing_seconds`` apart until host UTC's disruption counter and the
    discipline are stable, both samples lie within
    ``TIME_HOST_CORRECTION_SAMPLE_BOUND_NS``, and the host's time service did
    not update host UTC between them, unless Linux's limit holds its frequency
    correction."""
    return _render_script(
        "time-host-correction.sh.in",
        STATE_PREFIX=TIME_HOST_CORRECTION_STATE_PREFIX,
        SAMPLE_PREFIX=TIME_HOST_CORRECTION_SAMPLE_PREFIX,
        SPACING_SECONDS=str(spacing_seconds),
        PHASE_SETTLE_SECONDS=str(settle_seconds),
        ATTEMPTS=str(TIME_HOST_CORRECTION_ATTEMPTS),
        SAMPLE_BOUND_NS=str(TIME_HOST_CORRECTION_SAMPLE_BOUND_NS),
        MAX_ERROR_GROWTH_US=str(LINUX_MAX_ERROR_GROWTH_US),
    )


def _time_host_correction_report(
    output: str, *, context: str
) -> tuple[list[dict[str, str]], list[dict[str, str]]]:
    """Return the bracket states and samples of every measurement window in
    the time-host-correction scenario's guest report."""
    states: list[dict[str, str]] = []
    samples: list[dict[str, str]] = []
    try:
        for raw in output.splitlines():
            line = raw.removesuffix("\r")
            if line.startswith(TIME_HOST_CORRECTION_STATE_PREFIX):
                states.append(
                    parse_fields(line.removeprefix(TIME_HOST_CORRECTION_STATE_PREFIX))
                )
            elif line.startswith(TIME_HOST_CORRECTION_SAMPLE_PREFIX):
                samples.append(
                    parse_fields(line.removeprefix(TIME_HOST_CORRECTION_SAMPLE_PREFIX))
                )
    except ValueError as error:
        raise RuntimeError(f"{context}: malformed guest report: {error}") from error
    if not states or not samples:
        raise RuntimeError(f"{context}: the guest reported no time state or sample")
    return states, samples


def _failed_sample(sample: dict[str, str]) -> str | None:
    """Return the fields of an ``nvx-time sample`` report that paired no
    sample with the guest's clocks, or None if it did."""
    if "theta_ns" in sample and "epsilon_ns" in sample:
        return None
    return " ".join(f"{key}={value}" for key, value in sample.items())


def _time_service_update(first: dict[str, str], second: dict[str, str]) -> str | None:
    """Return how two time samples show that the host's time service updated
    host UTC between them, which may begin a slew of host UTC that neither
    sample's correction shows, or None if they don't.

    While a time service synchronizes host UTC, Linux grows host UTC's maximum
    error by ``LINUX_MAX_ERROR_GROWTH_US`` each second, and the service sets it
    at each update, so the second sample's grew by less over the whole seconds
    between their host UTC. Windows reports no maximum error, so its samples
    show no update. The guest's script applies the same rule.
    """
    if any(sample.get("status") != "synchronized" for sample in (first, second)):
        return None
    try:
        first_error, second_error = (
            int(sample["max_error_us"]) for sample in (first, second)
        )
        utc_ns = int(second["utc_ns"]) - int(first["utc_ns"])
    except (KeyError, ValueError):
        return None
    seconds = max(0, utc_ns // 1_000_000_000)
    least = first_error + LINUX_MAX_ERROR_GROWTH_US * seconds
    if second_error >= least:
        return None
    return (
        "the host's time service updated host UTC between the samples: its "
        f"maximum error went from {first_error} to {second_error} us in "
        f"{seconds} whole seconds, not to at least {least} us"
    )


class _HostRate(NamedTuple):
    """How host UTC ran against the guest TSC between two time samples."""

    # Host UTC's rate against the guest TSC counted at the declared rate.
    host_ppm: float
    # The average of the two samples' published corrections.
    published_ppm: float
    # How far the rate may be from that average.
    limit_ppm: float
    # The guest TSC's seconds between the samples.
    seconds: float

    def failure(self) -> str | None:
        """Return how host UTC did not run at its published correction, or
        None if it did."""
        if abs(self.host_ppm - self.published_ppm) <= self.limit_ppm:
            return None
        return (
            f"host UTC ran at {self.host_ppm:+.3f} ppm against the guest TSC in "
            f"{self.seconds:.1f} s, not at its published correction of "
            f"{self.published_ppm:+.3f} ppm, within {self.limit_ppm:.3f} ppm"
        )


def _host_rate(
    first: dict[str, str],
    second: dict[str, str],
    state: dict[str, str],
    *,
    context: str,
) -> _HostRate:
    """Return how host UTC, which the samples ``first`` and ``second`` pair with
    the guest TSC, ran against that TSC counted at ``state``'s declared rate,
    and how far it may run from the samples' published corrections:
    ``TIME_HOST_CORRECTION_HOST_PPM`` beyond half the correction's change
    between them and their pairing bounds."""
    try:
        elapsed_ns = (
            (int(second["tsc"]) - int(first["tsc"])) * 1e9 / int(state["tsc_hz"])
        )
        utc_ns = int(second["utc_ns"]) - int(first["utc_ns"])
        pairing_error_ns = int(first["pairing_ns"]) + int(second["pairing_ns"])
        first_ppm, second_ppm = (
            int(sample["correction"]) / 65_536 for sample in (first, second)
        )
    except (KeyError, ValueError, ZeroDivisionError) as error:
        raise RuntimeError(f"{context}: malformed guest report: {error!r}") from error
    if elapsed_ns <= 0:
        raise RuntimeError(
            f"{context}: the guest TSC did not advance between the samples"
        )
    return _HostRate(
        host_ppm=(utc_ns / elapsed_ns - 1) * 1e6,
        published_ppm=(first_ppm + second_ppm) / 2,
        limit_ppm=TIME_HOST_CORRECTION_HOST_PPM
        + abs(second_ppm - first_ppm) / 2
        + pairing_error_ns / elapsed_ns * 1e6,
        seconds=elapsed_ns / 1e9,
    )


def _time_host_correction_window(
    states: list[dict[str, str]],
    samples: list[dict[str, str]],
    *,
    context: str,
    limit: bool,
) -> tuple[dict[str, str], list[dict[str, str]]]:
    """Return the state and samples of the first stable and conclusive
    measurement window.

    Each window's states must keep the accepted-sample count, held correction,
    and clock-step fields, and its samples the host disruption counter, and
    each sample must pair within ``TIME_HOST_CORRECTION_SAMPLE_BOUND_NS``. A
    window between whose samples the host's time service updated host UTC must
    also show host UTC running at its published correction: the update may
    begin a slew that neither sample's correction shows. Another window is
    retried, up to ``TIME_HOST_CORRECTION_ATTEMPTS`` times; the limited phase,
    which takes one sample, is not.
    """
    if len(states) % 3:
        raise RuntimeError(
            f"{context}: the guest reported {len(states)} window states, not triples"
        )
    windows = len(states) // 3
    if not 1 <= windows <= TIME_HOST_CORRECTION_ATTEMPTS:
        raise RuntimeError(
            f"{context}: the guest reported {windows} measurement windows, not 1 to "
            f"{TIME_HOST_CORRECTION_ATTEMPTS}"
        )
    for index, (initial, before, after) in enumerate(
        zip(states[::3], states[1::3], states[2::3], strict=True), start=1
    ):
        if (
            initial.get("attempt") != str(index)
            or initial.get("point") != "initial"
            or before.get("attempt") != str(index)
            or before.get("point") != "before"
            or after.get("attempt") != str(index)
            or after.get("point") != "after"
        ):
            raise RuntimeError(
                f"{context}: malformed state brackets for measurement window {index}"
            )
    state_reports_limit = (
        windows == 1 and len(samples) == 1 and states[2].get("frequency_limited") == "1"
    )
    if limit or state_reports_limit:
        if windows != 1 or len(samples) != 1:
            raise RuntimeError(
                f"{context}: the limited phase reported {windows} windows and "
                f"{len(samples)} samples, not one of each"
            )
        failed = _failed_sample(samples[0])
        if failed is not None:
            raise RuntimeError(f"{context}: nvx-time sample failed: {failed}")
        return states[2], samples
    if len(samples) != 2 * windows:
        raise RuntimeError(
            f"{context}: the guest reported {len(samples)} time samples for "
            f"{windows} measurement windows, not {2 * windows}"
        )
    reasons: list[str] = []
    for window in range(windows):
        initial, before, after = states[3 * window : 3 * window + 3]
        first, second = samples[2 * window : 2 * window + 2]
        missing = [
            field
            for field in TIME_HOST_CORRECTION_DISCIPLINE_FIELDS
            if any(field not in state for state in (initial, before, after))
        ]
        if missing:
            raise RuntimeError(
                f"{context}: measurement window {window + 1} states lack "
                + ", ".join(missing)
            )
        signature_initial = tuple(
            initial[field] for field in TIME_HOST_CORRECTION_DISCIPLINE_FIELDS
        )
        signature_before = tuple(
            before[field] for field in TIME_HOST_CORRECTION_DISCIPLINE_FIELDS
        )
        signature_after = tuple(
            after[field] for field in TIME_HOST_CORRECTION_DISCIPLINE_FIELDS
        )
        first_disruption = first.get("disruption")
        failed = next(
            (report for report in map(_failed_sample, (first, second)) if report),
            None,
        )
        if failed is not None:
            reasons.append(f"window {window + 1}: nvx-time sample failed: {failed}")
            continue
        try:
            uncertainty_ns = max(
                int(sample["epsilon_ns"]) for sample in (first, second)
            )
        except (KeyError, ValueError) as error:
            raise RuntimeError(
                f"{context}: malformed guest report: {error!r}"
            ) from error
        if not signature_initial == signature_before == signature_after:
            reasons.append(f"window {window + 1}: the discipline changed")
        elif first_disruption is None or first_disruption != second.get("disruption"):
            reasons.append(
                f"window {window + 1}: host UTC's disruption counter changed"
            )
        elif uncertainty_ns > TIME_HOST_CORRECTION_SAMPLE_BOUND_NS:
            reasons.append(
                f"window {window + 1}: a sample's uncertainty of {uncertainty_ns} ns "
                f"exceeds the {TIME_HOST_CORRECTION_SAMPLE_BOUND_NS} ns bound"
            )
        elif (update := _time_service_update(first, second)) is not None and (
            slewed := _host_rate(first, second, after, context=context).failure()
        ) is not None:
            reasons.append(f"window {window + 1}: {update}, and {slewed}")
        else:
            return after, [first, second]
    raise RuntimeError(
        f"{context}: no measurement window was stable and conclusive: "
        + "; ".join(reasons)
    )


def time_host_correction_host(output: str, *, context: str) -> tuple[float, int]:
    """Return the host's own clock correction, in ppm, and the declared TSC
    rate that the unhooked guest report publishes.

    No test hook may have altered the first sample. The TSC rate determines
    whether Linux's kHz rounding would saturate an ordinary hooked phase.
    """
    states, samples = _time_host_correction_report(output, context=context)
    state, samples = _time_host_correction_window(
        states, samples, context=context, limit=False
    )
    sample = samples[0]
    try:
        if "theta_ns" not in sample:
            raise RuntimeError(
                f"{context}: nvx-time sample failed: "
                + " ".join(f"{key}={value}" for key, value in sample.items())
            )
        if int(sample["flags"], 16) & TIME_SAMPLE_TEST_HOOKS:
            raise RuntimeError(
                f"{context}: time sample flags {sample['flags']} carry the "
                "test-hook flag, so a hook altered the host's own correction"
            )
        return int(sample["correction"]) / 65_536, int(state["tsc_hz"])
    except (KeyError, ValueError) as error:
        raise RuntimeError(f"{context}: malformed guest report: {error!r}") from error


def time_host_correction_host_ppm(output: str, *, context: str) -> float:
    """Return only the host correction from [`time_host_correction_host`]."""
    return time_host_correction_host(output, context=context)[0]


def time_host_correction_composed_ppm(host_ppm: float, hook_ppm: int) -> float:
    """Return the published correction when OpenVMM composes the host's rate
    with ``hook_ppm`` multiplicatively."""
    return host_ppm + hook_ppm + host_ppm * hook_ppm / 1_000_000


def time_host_correction_guest_ppm(correction_ppm: float, tsc_hz: int) -> float:
    """Return the correction that Linux must hold after it rounds
    ``tsc_hz`` down to the kHz."""
    if tsc_hz <= 0:
        raise RuntimeError(f"the declared TSC rate is invalid: {tsc_hz} Hz")
    kernel_hz = tsc_hz // 1000 * 1000
    return (1_000_000 + correction_ppm) * kernel_hz / tsc_hz - 1_000_000


def time_host_correction_hooks(host_ppm: float, tsc_hz: int) -> tuple[int, int, int]:
    """Return the ``host-rate-offset-ppm`` hooks, in ppm, of the
    time-host-correction scenario's cold boot, restore, and limit restore on a
    host whose own clock correction is ``host_ppm``.

    The first two aim to make OpenVMM publish corrections of
    ``TIME_HOST_CORRECTION_BOOT_PPM`` and ``TIME_HOST_CORRECTION_RESTORE_PPM``
    against the sign of the host's own. OpenVMM composes the two rates
    multiplicatively, so solve that composition for the hook. The hooks stay
    within OpenVMM's accepted range; near its edge they publish the closest
    correction that remains below Linux's 500 ppm limit. A host whose
    correction cannot be brought below that limit fails here with an explicit
    diagnostic. The last hook extends the host's own correction beyond Linux's
    limit.
    """
    sign = 1 if host_ppm >= 0 else -1
    denominator = 1_000_000 + host_ppm
    if denominator <= 0:
        raise RuntimeError(f"the host's own correction is invalid: {host_ppm:+.3f} ppm")

    def ordinary(target_ppm: int) -> int:
        target = -sign * target_ppm
        hook = round((target - host_ppm) * 1_000_000 / denominator)
        hook = max(
            -OPENVMM_HOST_RATE_OFFSET_LIMIT_PPM,
            min(OPENVMM_HOST_RATE_OFFSET_LIMIT_PPM, hook),
        )
        published = time_host_correction_composed_ppm(host_ppm, hook)
        guest = time_host_correction_guest_ppm(published, tsc_hz)
        if abs(guest) >= LINUX_FREQUENCY_LIMIT_PPB / 1000:
            raise RuntimeError(
                f"the host's own correction of {host_ppm:+.3f} ppm cannot be "
                "composed with Linux's rounded TSC rate below its 500 ppm "
                "limit within OpenVMM's host-rate-offset-ppm range of +/-"
                f"{OPENVMM_HOST_RATE_OFFSET_LIMIT_PPM:,} ppm (published "
                f"{published:+.3f} ppm, guest {guest:+.3f} ppm at {tsc_hz} Hz)"
            )
        return hook

    return (
        ordinary(TIME_HOST_CORRECTION_BOOT_PPM),
        ordinary(TIME_HOST_CORRECTION_RESTORE_PPM),
        sign * TIME_HOST_CORRECTION_LIMIT_PPM,
    )


def time_host_correction_frequency_allowance_ppb(
    tsc_hz: int, correction_ppb: int
) -> int:
    """Return the rounding allowance between the host and kernel corrections.

    Linux counts the declared TSC rate rounded down to the kHz, up to 999 Hz
    slow. Scale that worst case at ``tsc_hz`` by the correction, and allow one
    more ppb for the correction's integer rounding. The time ABI admits rates
    as low as 500 MHz, where this is nearly 2,000 ppb rather than the 1,001 ppb
    sufficient at 1 GHz.
    """
    if tsc_hz <= 0:
        raise ValueError(f"invalid TSC rate {tsc_hz}")
    numerator = 999 * (1_000_000_000 + abs(correction_ppb))
    return (numerator + tsc_hz - 1) // tsc_hz + 1


def check_time_host_correction(
    output: str, *, context: str, limit: bool = False
) -> str:
    """Check the guest's report of the time-host-correction scenario in its
    console ``output``, and return a summary of it.

    The correction that the guest's boot step or restore repair took must come
    from a sample that OpenVMM's hook altered, as the time state's
    ``host_test_hooks`` reports, so that it isn't a stale one: the capture runs
    without the hook. Its value isn't compared with a later sample's, which the
    host's time service may have slewed since. With ``limit``, that correction
    must exceed Linux's limit, and the guest must hold the limit and report it.
    Otherwise it must lie within the limit, and the guest's frequency
    correction must be it, short of the kernel's kHz rounding of the TSC rate;
    between its two samples, the guest's clock must run at the correction that
    it holds against the guest TSC counted at the declared rate, as each sample
    pairs the clock with the TSC, within an allowance below half that
    correction, and host UTC, which each sample pairs with
    the same TSC, at its published correction, give or take the slews of the
    host's time service, so that the hook skewed both alike. A pair whose
    disruption counter changes, whose samples failed to pair or are uncertain
    beyond the discipline's bound, or between whose samples the host's time
    service updated host UTC, which then ran off its published correction, is
    discarded in favor of one of the bounded retry windows. Every sample must
    carry OpenVMM's test-hook flag, so that the hook reached it."""
    states, samples = _time_host_correction_report(output, context=context)
    state, samples = _time_host_correction_window(
        states, samples, context=context, limit=limit
    )
    try:
        for sample in samples:
            if "theta_ns" not in sample:
                raise RuntimeError(
                    f"{context}: nvx-time sample failed: "
                    + " ".join(f"{key}={value}" for key, value in sample.items())
                )
            if int(sample["flags"], 16) & TIME_SAMPLE_TEST_HOOKS == 0:
                raise RuntimeError(
                    f"{context}: time sample flags {sample['flags']} lack the "
                    "test-hook flag, so OpenVMM's hook did not reach it"
                )
        frequency_ppb = int(state["frequency_ppb"])
        correction_ppb = int(state["host_correction_ppb"])
        limited = state["frequency_limited"] == "1"
        for held_state in (states[0], state):
            if held_state["host_test_hooks"] != "1":
                raise RuntimeError(
                    f"{context}: the guest holds the host's correction of "
                    f"{held_state['host_correction_ppb']} ppb from a sample that "
                    f"OpenVMM's hook did not alter "
                    f"(host_test_hooks={held_state['host_test_hooks']}): its boot "
                    "step or restore repair kept a stale correction"
                )
        tsc_hz = int(state["tsc_hz"])
        required_ppm = time_host_correction_guest_ppm(correction_ppb / 1000, tsc_hz)
        beyond = abs(required_ppm) > LINUX_FREQUENCY_LIMIT_PPB / 1000
        if limit:
            if not beyond:
                raise RuntimeError(
                    f"{context}: Linux's required correction of "
                    f"{required_ppm:+.3f} ppm is within its limit, so this phase "
                    "does not exercise it"
                )
            held = (
                LINUX_FREQUENCY_LIMIT_PPB
                if required_ppm > 0
                else (-LINUX_FREQUENCY_LIMIT_PPB)
            )
            if not limited or frequency_ppb != held:
                raise RuntimeError(
                    f"{context}: Linux's required correction of "
                    f"{required_ppm:+.3f} ppm exceeds its limit, but the guest "
                    f"holds {frequency_ppb} ppb "
                    f"with frequency_limited={state['frequency_limited']}, not "
                    f"{held} ppb with frequency_limited=1"
                )
            return (
                f"{context}: host_correction_ppb={correction_ppb} "
                f"frequency_ppb={frequency_ppb} frequency_limited=1"
            )
        if limited or beyond:
            raise RuntimeError(
                f"{context}: Linux's required correction of {required_ppm:+.3f} "
                f"ppm reaches its limit (frequency_limited="
                f"{state['frequency_limited']}), so "
                "this phase cannot check the guest's rate"
            )
        first, second = samples
        allowance_ppb = time_host_correction_frequency_allowance_ppb(
            tsc_hz, correction_ppb
        )
        if abs(frequency_ppb - correction_ppb) > allowance_ppb:
            raise RuntimeError(
                f"{context}: the guest holds a frequency correction of "
                f"{frequency_ppb} ppb, not the host's correction of "
                f"{correction_ppb} ppb (allowance {allowance_ppb} ppb at "
                f"{tsc_hz} Hz)"
            )
        # The guest TSC, counted at the declared rate, paces both clocks: each
        # sample pairs it with host UTC and, at host UTC less theta, with the
        # guest's realtime clock, within its uncertainty less the pairing bound.
        elapsed_ns = (int(second["tsc"]) - int(first["tsc"])) * 1e9 / tsc_hz
        utc_ns = int(second["utc_ns"]) - int(first["utc_ns"])
        realtime_ns = utc_ns - (int(second["theta_ns"]) - int(first["theta_ns"]))
        realtime_error_ns = sum(
            int(sample["epsilon_ns"]) - int(sample["pairing_ns"]) for sample in samples
        )
    except (KeyError, ValueError, ZeroDivisionError) as error:
        raise RuntimeError(f"{context}: malformed guest report: {error!r}") from error
    if elapsed_ns <= 0:
        raise RuntimeError(
            f"{context}: the guest TSC did not advance between the samples"
        )
    host = _host_rate(first, second, state, context=context)
    seconds = elapsed_ns / 1e9
    held_ppm = correction_ppb / 1000
    guest_ppm = (realtime_ns / elapsed_ns - 1) * 1e6
    guest_limit = TIME_HOST_CORRECTION_GUEST_PPM + realtime_error_ns / elapsed_ns * 1e6
    # A guest that ignored the correction that it holds would run its clock at
    # the TSC's own rate, about 0 ppm, so an allowance of half the correction or
    # more could not tell it from one that applied the correction.
    conclusive_ppm = abs(held_ppm) / 2
    if guest_limit >= conclusive_ppm:
        raise RuntimeError(
            f"{context}: the allowance of {guest_limit:.3f} ppm for the guest's "
            f"clock in {seconds:.1f} s is not below {conclusive_ppm:.3f} ppm, half "
            f"the host's correction of {held_ppm:+.3f} ppm that it holds, so its "
            "rate check is inconclusive"
        )
    if abs(guest_ppm - held_ppm) > guest_limit:
        raise RuntimeError(
            f"{context}: the guest's clock ran at {guest_ppm:+.3f} ppm against its "
            f"TSC in {seconds:.1f} s, not at the host's correction of "
            f"{held_ppm:+.3f} ppm that it holds, within {guest_limit:.3f} ppm"
        )
    if (failure := host.failure()) is not None:
        raise RuntimeError(f"{context}: {failure}")
    return (
        f"{context}: host_correction_ppb={correction_ppb} frequency_ppb={frequency_ppb} "
        f"guest_ppm={guest_ppm:+.3f} host_ppm={host.host_ppm:+.3f} "
        f"published_ppm={host.published_ppm:+.3f} in {seconds:.1f} s"
    )


def run_time_host_correction(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Check that the guest applies the host's clock correction that OpenVMM
    publishes with each time sample, through the ``host-rate-offset-ppm`` test
    hook, which publishes host UTC as running faster or slower than it does.

    An unhooked cold boot's time sample gives the host's own correction, from
    which ``time_host_correction_hooks`` chooses the hooks. A cold boot with the
    hook must hold the published correction from its boot step on, and keep its
    clock on the hooked host UTC. A snapshot, captured without the hook,
    restored with another rate must hold the destination's correction from its
    repair on, and pass the warp probe and its restore checks; restored with a
    rate beyond Linux's 500 ppm limit, in the direction of the host's own
    correction, it must hold the limit and report it."""
    boot_command = workload_boot_command(
        executable,
        backend,
        kernel,
        initrd,
        memory_mib,
        "quiet loglevel=0",
        processors=1,
    )
    completion = f"echo {TIME_HOST_CORRECTION_COMPLETION_MARKER.decode()}\nnvx-exit 0\n"
    context = "host clock correction probe"
    probe = run_guest_script(
        boot_command,
        time_host_correction_script(spacing_seconds=0) + completion,
        TIME_HOST_CORRECTION_COMPLETION_MARKER,
        timeout=timeout,
        log_path=output_dir / "time-host-correction-probe.log",
    )
    host_ppm, tsc_hz = time_host_correction_host(probe["output"], context=context)
    boot_ppm, restore_ppm, limit_ppm = time_host_correction_hooks(host_ppm, tsc_hz)
    print(f"{context}: the host's own correction is {host_ppm:+.3f} ppm", flush=True)
    context = f"host clock correction cold boot (hook {boot_ppm:+d} ppm)"
    result = run_guest_script(
        [*boot_command, *time_host_correction_hook(boot_ppm)],
        time_host_correction_script() + completion,
        TIME_HOST_CORRECTION_COMPLETION_MARKER,
        timeout=timeout,
        log_path=output_dir / "time-host-correction-boot.log",
    )
    print(check_time_host_correction(result["output"], context=context), flush=True)
    with tempfile.TemporaryDirectory(prefix="nvx-time-host-correction-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        capture_snapshot(
            [*boot_command, "--snapshot-destination", str(snapshot_path)],
            snapshot_path,
            timeout=timeout,
            processors=1,
            post_restore_script=time_host_correction_script() + post_restore_checks(),
            log_path=output_dir / "time-host-correction-capture.log",
        )
        for name, ppm, limit in (
            ("restore", restore_ppm, False),
            ("limit", limit_ppm, True),
        ):
            context = f"host clock correction {name} (hook {ppm:+d} ppm)"
            restore_command = [
                *snapshot_restore_command(
                    executable, backend, snapshot_path, processors=1
                ),
                *time_host_correction_hook(ppm),
            ]
            output = _measure_restore(
                restore_command,
                context=context,
                environment=_restore_environment(),
                timeout=timeout,
                log_path=output_dir / f"time-host-correction-{name}.log",
                failure_marker=WARP_PROBE_FAILURE_MARKER,
            )
            print(
                check_time_host_correction(
                    output.decode("utf-8", "replace"), context=context, limit=limit
                ),
                flush=True,
            )
            _check_restore_warp(output, processors=1, context=context)
            _check_restore_status(
                output, restore_command, processors=1, context=context
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


def run_platform_restore_processors(
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
    """Bring a different vCPU prefix online in each clone of one platform
    snapshot.

    The platform capture boots its capacity with ``maxcpus=1``, its only
    caller token. Each restore of a requested count up to the capacity, and
    one without a target, runs the platform repair and the checks of
    ``restore-processors``. OpenVMM refuses unsupported and over-capacity
    counts before the guest runs.
    """
    capacity = PLATFORM_RESTORE_PROCESSORS_CAPACITY
    boot_online = PLATFORM_RESTORE_PROCESSORS_BOOT_ONLINE
    script = _read_script("restore-processors.sh") + post_restore_checks()
    environment = _restore_environment()
    environment["OPENVMM_LOG"] = TIME_ABI_RESTORE_LOG_FILTER
    environment[SNAPSHOT_PROFILE_ENV] = "1"
    with tempfile.TemporaryDirectory(
        prefix="nvx-platform-restore-processors-"
    ) as temporary:
        root = Path(temporary)
        snapshot_path = root / "snapshot"
        layer = root / "distro.erofs"
        capture_scratch = root / "capture-scratch.raw"
        _write_pattern(layer, 1024 * 1024, 0x3C)
        _write_pattern(capture_scratch, 1024 * 1024, 0xA5)
        boot_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            f"maxcpus={boot_online}",
            processors=capacity,
        )
        capture_snapshot(
            [
                *boot_command,
                "--snapshot-destination",
                str(snapshot_path),
                "--snapshot-tier",
                "platform",
                "--microvm-sandbox-block",
                _block_arg("distro", layer, read_only=True),
                "--microvm-sandbox-block",
                _block_arg("scratch", capture_scratch, read_only=False),
            ],
            snapshot_path,
            timeout=timeout,
            processors=boot_online,
            post_restore_script=script,
            log_path=output_dir / "platform-restore-processors-capture.log",
        )
        fingerprint = _scratch_snapshot_fingerprint(snapshot_path)

        def restore_command(target: int | None, name: str) -> list[str]:
            # Each clone gets its own fresh scratch of the captured geometry.
            scratch = root / f"restore-scratch-{name}.raw"
            _write_pattern(scratch, 1024 * 1024, 0x5A)
            command = snapshot_restore_command(
                executable,
                backend,
                snapshot_path,
                processors=capacity,
                restore_processors=target,
            )
            command.extend(
                (
                    "--microvm-sandbox-block",
                    _block_arg("distro", layer, read_only=True),
                    "--microvm-sandbox-block",
                    _block_arg("scratch", scratch, read_only=False),
                )
            )
            return command

        # An untargeted restore keeps the captured boot-online prefix.
        targets: list[int | None] = [
            *(count for count in dict.fromkeys(processor_counts) if count <= capacity),
            None,
        ]
        for target in targets:
            name = "untargeted" if target is None else str(target)
            online = boot_online if target is None else target
            context = f"platform {_restore_label(target)}"
            command = restore_command(target, name)
            output = _measure_restore(
                command,
                context=context,
                environment=environment,
                timeout=timeout,
                log_path=output_dir / f"platform-restore-processors-{name}.log",
                failure_marker=RESTORE_PROCESSORS_FAILURE_MARKER,
            )
            if not contains_output_line(output, PLATFORM_POST_RESTORE_MARKER):
                raise RuntimeError(f"{context} did not run the platform repair")
            marker = f"NVX-RESTORE-PROCESSORS-OK count={online}".encode()
            if not contains_output_line(output, marker):
                raise RuntimeError(f"{context} did not report {marker.decode()!r}")
            _check_restore_warp(output, processors=online, context=context)
            _check_restore_status(output, command, processors=online, context=context)
            _check_restore_vp_bindings(
                output,
                backend,
                target=target,
                capacity=capacity,
            )
            if _scratch_snapshot_fingerprint(snapshot_path) != fingerprint:
                raise RuntimeError(f"{context} modified snapshot artifacts")

        for target, reason in PLATFORM_RESTORE_PROCESSORS_REJECTED:
            _expect_process_failure(
                restore_command(target, f"rejected-{target}"),
                output_dir / f"platform-restore-processors-rejected-{target}.log",
                timeout,
                reason,
                forbidden_markers=(RESTORE_MARKER,),
            )
        if _scratch_snapshot_fingerprint(snapshot_path) != fingerprint:
            raise RuntimeError("a refused platform restore modified snapshot artifacts")


def _zero_fill_image(sectors: int) -> bytes:
    """Return an image of ``sectors`` sectors whose sector N is full of the
    byte N % 251 + 1, so that neither zeros nor another sector match it."""
    return b"".join(bytes((sector % 251 + 1,)) * 512 for sector in range(sectors))


def platform_zero_fill_script(image: bytes, capacity: int, scratch_length: int) -> str:
    """Return the checks that a clone of the zero-fill platform snapshot runs:
    reads inside, across, and past the end of ``image`` in a distro device of
    ``capacity`` bytes, and an absent scratch of ``scratch_length`` bytes."""
    sector = 512
    page = 4096
    if len(image) % sector or not len(image) % page or len(image) >= capacity:
        raise ValueError("the image must end inside a page before the capacity")
    image_sectors = len(image) // sector
    across_page = len(image) // page
    across = image[across_page * page :].ljust(page, b"\0")
    tail_sectors = capacity // sector - image_sectors

    def digest(data: bytes) -> str:
        return hashlib.sha256(data).hexdigest()

    return _render_script(
        "platform-zero-fill.sh.in",
        CAPACITY=str(capacity),
        IMAGE_SECTORS=str(image_sectors),
        IMAGE_SHA256=digest(image),
        ACROSS_PAGE=str(across_page),
        ACROSS_SHA256=digest(across),
        TAIL_SECTORS=str(tail_sectors),
        TAIL_SHA256=digest(bytes(tail_sectors * sector)),
        SCRATCH_LENGTH=str(scratch_length),
        SCRATCH_PAGES=str(scratch_length // page),
        SCRATCH_SHA256=digest(bytes(scratch_length)),
    )


def run_platform_zero_fill(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Restore one platform snapshot with a shorter distro image and with its
    fresh scratch declared absent.

    The capture's distro placeholder fixes the capacity that every clone's
    distro device presents. The clone's guest must read its image inside it,
    zeros past its end, including in the page across it, and zeros from a
    scratch device that stays writable but fails every write. OpenVMM must
    refuse a longer image, an absent scratch together with a scratch file, and
    a restore with neither before the guest runs, and must refuse the option on
    a cold boot. No restore may change the image or the snapshot.
    """
    capacity = PLATFORM_ZERO_FILL_CAPACITY
    image = _zero_fill_image(PLATFORM_ZERO_FILL_IMAGE_SECTORS)
    script = platform_zero_fill_script(image, capacity, PLATFORM_ZERO_FILL_SCRATCH)
    with tempfile.TemporaryDirectory(prefix="nvx-platform-zero-fill-") as temporary:
        root = Path(temporary)
        snapshot_path = root / "snapshot"
        placeholder = root / "placeholder.erofs"
        capture_scratch = root / "capture-scratch.raw"
        image_path = root / "image.erofs"
        longer_image = root / "longer.erofs"
        restore_scratch = root / "restore-scratch.raw"
        _write_pattern(placeholder, capacity, 0x3C)
        _write_pattern(capture_scratch, PLATFORM_ZERO_FILL_SCRATCH, 0xA5)
        image_path.write_bytes(image)
        _write_pattern(longer_image, capacity + 512, 0xC3)
        _write_pattern(restore_scratch, PLATFORM_ZERO_FILL_SCRATCH, 0x5A)
        boot_command = workload_boot_command(
            executable, backend, kernel, initrd, memory_mib, ""
        )
        capture_snapshot(
            [
                *boot_command,
                "--snapshot-destination",
                str(snapshot_path),
                "--snapshot-tier",
                "platform",
                "--microvm-sandbox-block",
                _block_arg("distro", placeholder, read_only=True),
                "--microvm-sandbox-block",
                _block_arg("scratch", capture_scratch, read_only=False),
            ],
            snapshot_path,
            timeout=timeout,
            processors=1,
            post_restore_script=script,
            log_path=output_dir / "platform-zero-fill-capture.log",
        )
        fingerprint = _scratch_snapshot_fingerprint(snapshot_path)

        def restore_command(
            distro: Path, scratch: Path | None, *, absent_scratch: bool
        ) -> list[str]:
            command = snapshot_restore_command(executable, backend, snapshot_path)
            command.extend(
                (
                    "--microvm-sandbox-block",
                    _block_arg("distro", distro, read_only=True),
                )
            )
            if scratch is not None:
                command.extend(
                    (
                        "--microvm-sandbox-block",
                        _block_arg("scratch", scratch, read_only=False),
                    )
                )
            if absent_scratch:
                command.append("--restore-absent-scratch")
            return command

        def check_unchanged(context: str) -> None:
            if image_path.read_bytes() != image:
                raise RuntimeError(f"{context} changed the distro image")
            if _scratch_snapshot_fingerprint(snapshot_path) != fingerprint:
                raise RuntimeError(f"{context} modified snapshot artifacts")

        context = "platform zero-fill restore"
        output = _measure_restore(
            restore_command(image_path, None, absent_scratch=True),
            context=context,
            environment=_restore_environment(),
            timeout=timeout,
            log_path=output_dir / "platform-zero-fill-restore.log",
            failure_marker=PLATFORM_ZERO_FILL_FAILURE_MARKER,
        )
        for marker in (
            PLATFORM_POST_RESTORE_MARKER,
            PLATFORM_ZERO_FILL_IMAGE_MARKER,
            PLATFORM_ZERO_FILL_SCRATCH_MARKER,
        ):
            if not contains_output_line(output, marker):
                raise RuntimeError(f"{context} did not report {marker.decode()!r}")
        check_unchanged(context)

        rejected_commands = {
            "longer-image": restore_command(
                longer_image, restore_scratch, absent_scratch=False
            ),
            "absent-and-file": restore_command(
                image_path, restore_scratch, absent_scratch=True
            ),
            "no-scratch": restore_command(image_path, None, absent_scratch=False),
        }
        for name, reason in PLATFORM_ZERO_FILL_REJECTED:
            _expect_process_failure(
                rejected_commands[name],
                output_dir / f"platform-zero-fill-rejected-{name}.log",
                timeout,
                reason,
                forbidden_markers=(RESTORE_MARKER,),
            )
        _expect_boot_failure(
            [
                *boot_command,
                "--microvm-sandbox-block",
                _block_arg("distro", placeholder, read_only=True),
                "--microvm-sandbox-block",
                _block_arg("scratch", capture_scratch, read_only=False),
                "--restore-absent-scratch",
            ],
            output_dir / "platform-zero-fill-rejected-cold-boot.log",
            timeout,
            b"--restore-snapshot",
        )
        check_unchanged("a refused platform zero-fill restore")


def platform_network_policy_script(
    *, allowed_port: int, other_port: int, guest_port: int
) -> str:
    """Return the checks that each clone of the NIC template runs: it reports
    its NIC's identity, probes the host services at ``allowed_port`` and
    ``other_port`` of the gateway, and serves the host at ``guest_port`` until
    the host has checked its forward."""
    return _render_script(
        "platform-network-policy.sh.in",
        GATEWAY_IPV4=PLATFORM_NETWORK_GATEWAY_IPV4,
        ALLOWED_PORT=str(allowed_port),
        OTHER_PORT=str(other_port),
        GUEST_PORT=str(guest_port),
        PROBE_TIMEOUT=str(PLATFORM_NETWORK_PROBE_TIMEOUT_SECONDS),
    )


@dataclass(frozen=True)
class PlatformNetworkClone:
    """A clone of the NIC template: the policy that its restore installs, and
    whether, under it, the guest reaches the allowed and the other host
    service and the host reaches the guest through a forward."""

    name: str
    policy: tuple[str, ...]
    allowed: bool
    other: bool
    forward: bool

    def probe_report(self) -> bytes:
        """Return the line in which the clone's guest reports its probes."""
        report = (
            f"allowed={'reached' if self.allowed else 'denied'} "
            f"other={'reached' if self.other else 'denied'} "
            f"inbound={'reached' if self.forward else 'none'}"
        )
        return PLATFORM_NETWORK_PROBE_PREFIX + report.encode()


def platform_network_clones(
    *, allowed_port: int, other_port: int, host_port: int, guest_port: int
) -> tuple[PlatformNetworkClone, ...]:
    """Return the clones that restore the NIC template: one without
    connectivity, under the template's own deny-all, and three whose policies
    lead to outcomes that differ from it and from each other's: egress rules;
    explicit host loopback with a forward from ``host_port`` to
    ``guest_port`` and private-network ingress; and a proxy."""
    gateway = PLATFORM_NETWORK_GATEWAY_IPV4
    return (
        PlatformNetworkClone(
            "deny-all",
            PLATFORM_NETWORK_TEMPLATE_POLICY,
            allowed=False,
            other=False,
            forward=False,
        ),
        PlatformNetworkClone(
            "egress-rules",
            (
                "--network-egress",
                "deny",
                "--network-egress-allow",
                f"{gateway}/32:tcp:{allowed_port}",
            ),
            allowed=True,
            other=False,
            forward=False,
        ),
        PlatformNetworkClone(
            "forward",
            (
                "--network-egress",
                "allow",
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                f"tcp:{host_port}:{guest_port}",
                "--network-ingress",
                "allow",
            ),
            allowed=True,
            other=True,
            forward=True,
        ),
        PlatformNetworkClone(
            "proxy",
            (
                "--network-egress",
                "deny",
                "--host-loopback",
                "deny",
                "--network-proxy",
                f"{gateway}:{other_port}",
            ),
            allowed=False,
            other=True,
            forward=False,
        ),
    )


class _HostLoopbackService:
    """An HTTP service on host IPv4 loopback that answers ``GET /NAME`` with
    ``NVX-PLATFORM-NETWORK-NAME`` and counts the requests that it receives."""

    def __init__(self, name: str, timeout: float) -> None:
        self.name = name
        self.errors: list[Exception] = []
        self._timeout = timeout
        self._requests = 0
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            self._listener.bind(("127.0.0.1", 0))
            self._listener.listen(4)
            self._listener.settimeout(0.05)
        except BaseException:
            self._listener.close()
            raise
        self.port = int(self._listener.getsockname()[1])
        self._thread = threading.Thread(
            target=self._serve, name=f"nvx-platform-network-{name}", daemon=True
        )
        self._thread.start()

    def _serve(self) -> None:
        body = f"NVX-PLATFORM-NETWORK-{self.name}".encode()
        request_line = f"GET /{self.name} HTTP/1.".encode()
        while not self._stop.is_set():
            try:
                connection, _ = self._listener.accept()
            except TimeoutError:
                continue
            except OSError as error:
                if not self._stop.is_set():
                    self.errors.append(error)
                return
            with connection:
                try:
                    request = _read_http_request(connection, self._timeout)
                    if not request.startswith(request_line):
                        raise RuntimeError(
                            f"unexpected request to the {self.name} service: "
                            f"{request!r}"
                        )
                    # The guest learns that it reached the service only from
                    # the answer, so the request counts before it.
                    with self._lock:
                        self._requests += 1
                    connection.sendall(
                        b"HTTP/1.1 200 OK\r\nContent-Length: "
                        + str(len(body)).encode("ascii")
                        + b"\r\nConnection: close\r\n\r\n"
                        + body
                    )
                except Exception as error:
                    self.errors.append(error)

    def take_requests(self) -> int:
        """Return the number of requests received since the last call."""
        with self._lock:
            requests, self._requests = self._requests, 0
        return requests

    def close(self) -> None:
        self._stop.set()
        self._thread.join(timeout=NETWORK_SERVER_JOIN_TIMEOUT_SECONDS)
        self._listener.close()


def _run_platform_network_clone(
    command: list[str],
    clone: PlatformNetworkClone,
    *,
    services: tuple[_HostLoopbackService, _HostLoopbackService],
    host_port: int,
    timeout: float,
    log_path: Path,
) -> None:
    """Restore ``clone`` with ``command``, exchange a line with its guest
    through its forward while the guest serves it, and check the identity and
    the probes that the guest reports and the requests that the allowed and
    the other host service in ``services`` received."""
    context = f"platform network policy clone {clone.name}"
    exchange = "forward" if clone.forward else "none"
    try:
        with OpenvmmProcess(command, log_path) as process:
            process.wait_for_line(PLATFORM_NETWORK_LISTENING_MARKER, timeout)
            if clone.forward:
                _host_loopback_tcp_exchange("127.0.0.1", host_port, timeout)
            process.send_line(f"{PLATFORM_NETWORK_CHECKED.decode()} {exchange}")
            result = process.wait(timeout)
    except Exception as error:
        failures = [
            line.decode("utf-8", "replace")
            for line in _output_lines(
                log_path.read_bytes() if log_path.is_file() else b""
            )
            if line.startswith(PLATFORM_NETWORK_FAILURE_MARKER)
        ]
        raise RuntimeError(
            f"{context} failed{f': {failures[0]}' if failures else ''}; see {log_path}"
        ) from error
    if result.returncode != 0:
        raise RuntimeError(f"{context} exited with {result.returncode}")
    for marker in (
        PLATFORM_POST_RESTORE_MARKER,
        PLATFORM_NETWORK_IDENTITY,
        clone.probe_report(),
        RESTORE_MARKER,
    ):
        if not contains_output_line(result.output, marker):
            raise RuntimeError(f"{context} did not report {marker.decode()!r}")
    requests = tuple(service.take_requests() for service in services)
    expected = (int(clone.allowed), int(clone.other))
    if requests != expected:
        raise RuntimeError(
            f"{context}: the allowed and other host services received "
            f"{requests} requests, not {expected}"
        )


def run_platform_network_policy(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Restore one NIC template with four network policies of its clones' own.

    The template is a platform snapshot whose NIC was captured with a complete
    deny-all policy. Each clone's restore installs another policy with
    ``--restore-network-policy``: the template's deny-all, egress rules,
    explicit host loopback with a forward and private-network ingress, and a
    proxy. Each clone must keep the template's NIC identity, reach exactly the
    host services that its policy admits, which must receive no other
    request, and serve the host through a forward only when its policy has
    one. OpenVMM must refuse, before the guest runs, a restore of the template
    that keeps its policy but passes another, or explicit host loopback;
    readdressing; a proxy at another address than the gateway; a forward
    whose host port is busy; the option on a snapshot captured with plain
    deny-all egress, which leaves host loopback allowed; and the option on a
    cold boot. No restore may change the template.
    """
    guest_port = PLATFORM_NETWORK_GUEST_PORT
    with ExitStack() as stack:
        services = (
            _HostLoopbackService("allowed", timeout),
            _HostLoopbackService("other", timeout),
        )
        for service in services:
            stack.callback(service.close)
        allowed, other = services
        _, host_port = _available_tcp_address()
        clones = platform_network_clones(
            allowed_port=allowed.port,
            other_port=other.port,
            host_port=host_port,
            guest_port=guest_port,
        )
        root = Path(
            stack.enter_context(
                tempfile.TemporaryDirectory(prefix="nvx-platform-network-policy-")
            )
        )
        template = root / "template"
        placeholder = root / "placeholder.erofs"
        capture_scratch = root / "capture-scratch.raw"
        _write_pattern(placeholder, PLATFORM_ZERO_FILL_CAPACITY, 0x3C)
        _write_pattern(capture_scratch, PLATFORM_ZERO_FILL_SCRATCH, 0xA5)
        boot_command = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "",
                network=PLATFORM_NETWORK_CIDR,
            ),
            "--microvm-sandbox-block",
            _block_arg("distro", placeholder, read_only=True),
            "--microvm-sandbox-block",
            _block_arg("scratch", capture_scratch, read_only=False),
        ]

        def capture(
            snapshot: Path, policy: Sequence[str], post_restore_script: str | None
        ) -> None:
            capture_snapshot(
                [
                    *boot_command,
                    *policy,
                    "--snapshot-destination",
                    str(snapshot),
                    "--snapshot-tier",
                    "platform",
                ],
                snapshot,
                timeout=timeout,
                processors=1,
                post_restore_script=post_restore_script,
                log_path=output_dir
                / f"platform-network-policy-{snapshot.name}-capture.log",
            )

        capture(
            template,
            PLATFORM_NETWORK_TEMPLATE_POLICY,
            platform_network_policy_script(
                allowed_port=allowed.port,
                other_port=other.port,
                guest_port=guest_port,
            ),
        )
        fingerprint = _scratch_snapshot_fingerprint(template)

        def restore_command(snapshot: Path, *arguments: str) -> list[str]:
            command = snapshot_restore_command(
                executable, backend, snapshot, network_profile="portable"
            )
            command.extend(
                (
                    "--microvm-sandbox-block",
                    _block_arg("distro", placeholder, read_only=True),
                    "--restore-absent-scratch",
                    *arguments,
                )
            )
            return command

        def check_template(context: str) -> None:
            if _scratch_snapshot_fingerprint(template) != fingerprint:
                raise RuntimeError(f"{context} modified the NIC template")
            errors = [error for service in services for error in service.errors]
            if errors:
                raise RuntimeError(f"a host service failed by {context}") from errors[0]

        check_template("the capture")
        # The template's guest probed nothing before its capture.
        if any(service.take_requests() for service in services):
            raise RuntimeError("the NIC template's capture reached a host service")
        for clone in clones:
            _run_platform_network_clone(
                restore_command(template, "--restore-network-policy", *clone.policy),
                clone,
                services=services,
                host_port=host_port,
                timeout=timeout,
                log_path=output_dir / f"platform-network-policy-{clone.name}.log",
            )
            check_template(f"clone {clone.name}")

        rejected_commands = {
            "kept-policy": restore_command(template, "--network-egress", "allow"),
            "kept-host-loopback": restore_command(
                template, "--network-egress", "deny", "--host-loopback", "allow"
            ),
            "readdressed": restore_command(
                template,
                "--restore-network-policy",
                "--net",
                "10.1.0.2/24",
                *PLATFORM_NETWORK_TEMPLATE_POLICY,
            ),
            "foreign-proxy": restore_command(
                template,
                "--restore-network-policy",
                *PLATFORM_NETWORK_TEMPLATE_POLICY,
                "--network-proxy",
                f"10.0.0.9:{other.port}",
            ),
        }
        for name, reason in PLATFORM_NETWORK_REJECTED:
            _expect_process_failure(
                rejected_commands[name],
                output_dir / f"platform-network-policy-rejected-{name}.log",
                timeout,
                reason,
                forbidden_markers=PLATFORM_NETWORK_GUEST_MARKERS,
            )
        # A forward whose host port another socket holds fails the restore.
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as busy:
            busy.bind(("127.0.0.1", 0))
            busy.listen(1)
            _expect_process_failure(
                restore_command(
                    template,
                    "--restore-network-policy",
                    "--host-loopback",
                    "allow",
                    "--host-loopback-forward",
                    f"tcp:{busy.getsockname()[1]}:{guest_port}",
                ),
                output_dir / "platform-network-policy-rejected-busy-forward.log",
                timeout,
                PLATFORM_NETWORK_BUSY_FORWARD_REJECTION,
                forbidden_markers=PLATFORM_NETWORK_GUEST_MARKERS,
            )
        # Deny-all egress alone leaves host loopback allowed, so a capture with
        # it is no NIC template.
        plain = root / "plain-deny-all"
        capture(plain, ("--network-egress", "deny"), None)
        _expect_process_failure(
            restore_command(
                plain, "--restore-network-policy", *PLATFORM_NETWORK_TEMPLATE_POLICY
            ),
            output_dir / "platform-network-policy-rejected-plain-deny-all.log",
            timeout,
            PLATFORM_NETWORK_NOT_TEMPLATE_REJECTION,
            forbidden_markers=PLATFORM_NETWORK_GUEST_MARKERS,
        )
        _expect_boot_failure(
            [
                *boot_command,
                *PLATFORM_NETWORK_TEMPLATE_POLICY,
                "--restore-network-policy",
            ],
            output_dir / "platform-network-policy-rejected-cold-boot.log",
            timeout,
            b"--restore-snapshot",
        )
        check_template("a refused restore")
        if any(service.take_requests() for service in services):
            raise RuntimeError("a refused restore reached a host service")


def untiered_network_policy_script(
    *, allowed_port: int, other_port: int, guest_port: int
) -> str:
    """Return the checks that each restore of the NIC template without a tier
    runs: it reports the nameservers of its resolver, whether the announcer of
    its listeners runs, and whether it reaches the host services at
    ``allowed_port`` and ``other_port`` of the gateway, then serves the host at
    ``guest_port`` until the host releases it."""
    return _render_script(
        "untiered-network-policy.sh.in",
        GATEWAY_IPV4=UNTIERED_NETWORK_GATEWAY_IPV4,
        ALLOWED_PORT=str(allowed_port),
        OTHER_PORT=str(other_port),
        GUEST_PORT=str(guest_port),
        MAX_CLIENTS=str(UNTIERED_NETWORK_MAX_CLIENTS),
        PROBE_TIMEOUT=str(PLATFORM_NETWORK_PROBE_TIMEOUT_SECONDS),
    )


@dataclass(frozen=True)
class UntieredNetworkRestore:
    """A restore of the NIC template without a tier: the policy that it
    installs; the DNS server that its guest learns from the restore packet;
    whether its guest reaches the allowed and the other host service; and how
    the host reaches the guest's server: through a forward, and through the
    port that the policy publishes on host loopback, ``host-loopback``, or to
    the host's private networks, ``private-networks``, for which the guest
    announces its listeners."""

    name: str
    policy: tuple[str, ...]
    dns_server: str | None
    allowed: bool
    other: bool
    forward: bool
    publication: str | None

    @property
    def announces(self) -> bool:
        """Return whether the guest announces its listeners: whether the
        policy publishes them."""
        return self.publication is not None

    @property
    def network_settings(self) -> bool:
        """Return whether the guest's repair applies network settings, after
        which it reports the restore's completion."""
        return self.dns_server is not None or self.announces

    @property
    def exchanges(self) -> int:
        """Return how many lines the host exchanges with the guest's server:
        one through the forward, and one through the published port."""
        return int(self.forward) + int(self.announces)


def untiered_network_restores(
    *, allowed_port: int, host_port: int, guest_port: int
) -> tuple[UntieredNetworkRestore, ...]:
    """Return the restores of the NIC template without a tier: one under the
    template's own deny-all, which names no DNS server and publishes nothing;
    egress rules that admit DNS over IPv4 and the allowed service; explicit
    host loopback with a forward from ``host_port`` to ``guest_port``, under
    egress rules that admit DNS over IPv6 alone; and private-network ingress
    with allowed egress."""
    gateway = UNTIERED_NETWORK_GATEWAY_IPV4
    return (
        UntieredNetworkRestore(
            "deny-all",
            PLATFORM_NETWORK_TEMPLATE_POLICY,
            dns_server=None,
            allowed=False,
            other=False,
            forward=False,
            publication=None,
        ),
        UntieredNetworkRestore(
            "egress-dns",
            (
                "--network-egress",
                "deny",
                "--network-egress-allow",
                f"{gateway}/32:udp:53",
                "--network-egress-allow",
                f"{gateway}/32:tcp:{allowed_port}",
            ),
            dns_server=gateway,
            allowed=True,
            other=False,
            forward=False,
            publication=None,
        ),
        UntieredNetworkRestore(
            "host-loopback",
            (
                "--network-egress",
                "deny",
                "--network-egress-allow",
                f"{UNTIERED_NETWORK_GATEWAY_IPV6}/128:udp:53",
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                f"tcp:{host_port}:{guest_port}",
            ),
            dns_server=UNTIERED_NETWORK_GATEWAY_IPV6,
            allowed=False,
            other=False,
            forward=True,
            publication="host-loopback",
        ),
        UntieredNetworkRestore(
            "private-ingress",
            (
                "--network-egress",
                "allow",
                "--network-ingress",
                "allow",
                "--host-loopback",
                "allow",
            ),
            dns_server=gateway,
            allowed=True,
            other=True,
            forward=False,
            publication="private-networks",
        ),
    )


def _untiered_network_report(output: bytes, context: str) -> dict[str, str]:
    """Return the fields of the one line in ``output`` in which a restored
    guest reported its resolver, its announcer, and its probes."""
    reports = [
        line
        for line in _output_lines(output)
        if line.startswith(UNTIERED_NETWORK_REPORT_PREFIX)
    ]
    if len(reports) != 1:
        raise RuntimeError(f"{context} printed {len(reports)} network reports, not 1")
    fields: dict[str, str] = {}
    text = reports[0].removeprefix(UNTIERED_NETWORK_REPORT_PREFIX)
    for field in text.decode("ascii", "replace").split():
        key, separator, value = field.partition("=")
        if not separator or key in fields:
            raise RuntimeError(f"{context} printed a malformed report: {reports[0]!r}")
        fields[key] = value
    if set(fields) != {"resolver", "announcer", "allowed", "other"}:
        raise RuntimeError(f"{context} printed a malformed report: {reports[0]!r}")
    return fields


def _check_untiered_network_report(
    restore: UntieredNetworkRestore, report: dict[str, str], context: str
) -> None:
    """Check that ``report``, the guest's, has the resolver, the announcer,
    and the probes that ``restore``'s policy gives it. Without a DNS server,
    the resolver keeps what the guest's image names, which is no gateway."""
    resolver = report["resolver"]
    nameservers = [] if resolver == "none" else resolver.split(",")
    if restore.dns_server is not None:
        if nameservers != [restore.dns_server]:
            raise RuntimeError(
                f"{context}: the guest's resolver names {resolver}, not "
                f"{restore.dns_server}"
            )
    elif {UNTIERED_NETWORK_GATEWAY_IPV4, UNTIERED_NETWORK_GATEWAY_IPV6}.intersection(
        nameservers
    ):
        raise RuntimeError(
            f"{context}: the guest's resolver names {resolver}, although the "
            "policy names no DNS server"
        )
    expected = {
        "announcer": "running" if restore.announces else "none",
        "allowed": "reached" if restore.allowed else "denied",
        "other": "reached" if restore.other else "denied",
    }
    for key, value in expected.items():
        if report[key] != value:
            raise RuntimeError(f"{context}: the guest reported {key}={report[key]}")


def _accepts_tcp(address: str, port: int) -> bool:
    """Return whether a TCP connection to ``address:port`` completes."""
    try:
        connection = socket.create_connection((address, port), timeout=1)
    except OSError:
        return False
    connection.close()
    return True


def _untiered_network_exchanges(
    restore: UntieredNetworkRestore,
    *,
    host_port: int,
    guest_port: int,
    timeout: float,
) -> int:
    """Exchange a line with the guest's server through each way that
    ``restore``'s policy gives the host, and return how many it exchanged.

    The forward binds ``host_port`` on host loopback before the guest runs. A
    published port is ``guest_port``, which the gateway publishes once the
    guest's announcer has announced the server: on host loopback alone, which
    no other host address reaches, or on every host address, through one of
    which, other than loopback, the host reaches it as a peer on its private
    networks would. Without an announcer, nothing publishes the guest's port.
    """
    exchanges = 0
    if restore.forward:
        _host_loopback_tcp_exchange("127.0.0.1", host_port, timeout)
        exchanges += 1
    if restore.publication == "host-loopback":
        _wait_until(
            lambda: _holds_loopback(socket.AF_INET, socket.SOCK_STREAM, guest_port),
            timeout,
            f"guest port {guest_port} was not published on host loopback",
        )
        for address in _host_ipv4_addresses():
            if _accepts_tcp(address, guest_port):
                raise RuntimeError(
                    f"host-loopback access published guest port {guest_port} on "
                    f"{address}"
                )
        _host_loopback_tcp_exchange("127.0.0.1", guest_port, timeout)
        exchanges += 1
    elif restore.publication == "private-networks":
        addresses = _host_ipv4_addresses()
        if not addresses:
            raise RuntimeError(
                "the host has no known IPv4 address other than loopback, through "
                "which to reach the port that private-network ingress publishes"
            )
        # A wildcard bind fails over a loopback-only listener too, so only the
        # exchange through another address shows the port's publication to
        # the host's private networks.
        _wait_until(
            lambda: _publishes(guest_port, socket.SOCK_STREAM),
            timeout,
            f"guest port {guest_port} was not published to the host",
        )
        _host_loopback_tcp_exchange(addresses[0], guest_port, timeout)
        exchanges += 1
    elif _holds_loopback(socket.AF_INET, socket.SOCK_STREAM, guest_port):
        raise RuntimeError(f"guest port {guest_port} was published without ingress")
    return exchanges


def _run_untiered_network_restore(
    command: list[str],
    restore: UntieredNetworkRestore,
    *,
    services: tuple[_HostLoopbackService, _HostLoopbackService],
    host_port: int,
    guest_port: int,
    timeout: float,
    log_path: Path,
) -> None:
    """Restore the NIC template with ``command``, which installs
    ``restore``'s policy, exchange lines with its guest's server while the
    guest serves the host, and check what the guest reports, whether its
    repair applied network settings, and the requests that the allowed and the
    other host service in ``services`` received."""
    context = f"untiered network policy restore {restore.name}"
    try:
        with OpenvmmProcess(command, log_path) as process:
            process.wait_for_line(UNTIERED_NETWORK_LISTENING_MARKER, timeout)
            exchanges = _untiered_network_exchanges(
                restore, host_port=host_port, guest_port=guest_port, timeout=timeout
            )
            process.send_line(f"{UNTIERED_NETWORK_CHECKED.decode()} {exchanges}")
            result = process.wait(timeout)
    except Exception as error:
        failures = [
            line.decode("utf-8", "replace")
            for line in _output_lines(
                log_path.read_bytes() if log_path.is_file() else b""
            )
            if line.startswith(UNTIERED_NETWORK_FAILURE_MARKER)
        ]
        raise RuntimeError(
            f"{context} failed{f': {failures[0]}' if failures else ''}; see {log_path}"
        ) from error
    if result.returncode != 0:
        raise RuntimeError(f"{context} exited with {result.returncode}")
    _check_untiered_network_report(
        restore, _untiered_network_report(result.output, context), context
    )
    for marker in (
        UNTIERED_NETWORK_INBOUND_PREFIX + str(restore.exchanges).encode(),
        RESTORE_MARKER,
    ):
        if not contains_output_line(result.output, marker):
            raise RuntimeError(f"{context} did not report {marker.decode()!r}")
    # A restore without network settings to apply finishes quietly, as a plain
    # restore without a tier does.
    if (
        contains_output_line(result.output, UNTIERED_POST_RESTORE_MARKER)
        != restore.network_settings
    ):
        raise RuntimeError(
            f"{context}: the guest's repair "
            f"{'did not report' if restore.network_settings else 'reported'} "
            f"{UNTIERED_POST_RESTORE_MARKER.decode()!r}"
        )
    requests = tuple(service.take_requests() for service in services)
    expected = (int(restore.allowed), int(restore.other))
    if requests != expected:
        raise RuntimeError(
            f"{context}: the allowed and other host services received "
            f"{requests} requests, not {expected}"
        )


def run_untiered_network_policy(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Restore one NIC template without a tier with four network policies.

    The template is a snapshot without sandbox blocks, as the non-sandbox
    mode captures one, whose NIC was captured with a complete deny-all
    policy, so its guest learned no DNS server and announces no listeners.
    Each restore installs another policy with ``--restore-network-policy``:
    the template's deny-all; egress rules that admit DNS over IPv4 and one
    host service; explicit host loopback with a forward, under egress rules
    that admit DNS over IPv6 alone; and private-network ingress with allowed
    egress. Each restore's guest must name the DNS server of its policy in its
    resolver, and no gateway without one, run the announcer of its listeners
    exactly when its policy publishes them, and reach exactly the host
    services that its policy admits, which must receive no other request. The
    host must reach the guest's server through the forward, and through the
    port that the policy publishes: on host loopback alone, which no other
    host address reaches, or to the host's private networks, through a host
    address other than loopback. It must find the server on no host loopback
    port without a publication. OpenVMM must refuse, before the guest runs, a
    restore that keeps the template's policy but passes another, and one that
    passes private-network ingress without the option. No restore may change
    the template.
    """
    with ExitStack() as stack:
        services = (
            _HostLoopbackService("allowed", timeout),
            _HostLoopbackService("other", timeout),
        )
        for service in services:
            stack.callback(service.close)
        allowed, other = services
        guest_port = _host_loopback_publication_port()
        _, host_port = _available_tcp_address()
        while host_port == guest_port:
            _, host_port = _available_tcp_address()
        restores = untiered_network_restores(
            allowed_port=allowed.port, host_port=host_port, guest_port=guest_port
        )
        root = Path(
            stack.enter_context(
                tempfile.TemporaryDirectory(prefix="nvx-untiered-network-policy-")
            )
        )
        template = root / "template"
        capture_snapshot(
            [
                *workload_boot_command(
                    executable,
                    backend,
                    kernel,
                    initrd,
                    memory_mib,
                    "",
                    network=UNTIERED_NETWORK_CIDR,
                ),
                *PLATFORM_NETWORK_TEMPLATE_POLICY,
                "--snapshot-destination",
                str(template),
            ],
            template,
            timeout=timeout,
            processors=1,
            post_restore_script=untiered_network_policy_script(
                allowed_port=allowed.port,
                other_port=other.port,
                guest_port=guest_port,
            ),
            log_path=output_dir / "untiered-network-policy-capture.log",
        )
        fingerprint = _snapshot_fingerprint(template)

        def restore_command(*arguments: str) -> list[str]:
            return [
                *snapshot_restore_command(
                    executable, backend, template, network_profile="portable"
                ),
                *arguments,
            ]

        def check_template(context: str) -> None:
            if _snapshot_fingerprint(template) != fingerprint:
                raise RuntimeError(f"{context} modified the NIC template")
            errors = [error for service in services for error in service.errors]
            if errors:
                raise RuntimeError(f"a host service failed by {context}") from errors[0]

        check_template("the capture")
        # The template's guest probed nothing before its capture.
        if any(service.take_requests() for service in services):
            raise RuntimeError("the NIC template's capture reached a host service")
        for restore in restores:
            _run_untiered_network_restore(
                restore_command("--restore-network-policy", *restore.policy),
                restore,
                services=services,
                host_port=host_port,
                guest_port=guest_port,
                timeout=timeout,
                log_path=output_dir / f"untiered-network-policy-{restore.name}.log",
            )
            check_template(f"restore {restore.name}")

        rejected_commands = {
            "kept-policy": restore_command("--network-egress", "allow"),
            "ingress": restore_command(
                *PLATFORM_NETWORK_TEMPLATE_POLICY, "--network-ingress", "allow"
            ),
        }
        for name, reason in UNTIERED_NETWORK_REJECTED:
            _expect_process_failure(
                rejected_commands[name],
                output_dir / f"untiered-network-policy-rejected-{name}.log",
                timeout,
                reason,
                forbidden_markers=UNTIERED_NETWORK_GUEST_MARKERS,
            )
        check_template("a refused restore")
        if any(service.take_requests() for service in services):
            raise RuntimeError("a refused restore reached a host service")


def run_platform_filesystem(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Attach a host directory to the filesystem slot that a platform snapshot
    left dormant, and use it from the clone.

    A tiered restore starts the clone's devices behind the post-restore input
    gate. Once the guest's platform repair releases it, the guest must mount
    the share, read the file that the host wrote, and write one that the host
    reads. The restore may not change the snapshot.
    """
    with tempfile.TemporaryDirectory(prefix="nvx-platform-filesystem-") as temporary:
        root = Path(temporary)
        snapshot_path = root / "snapshot"
        layer = root / "distro.erofs"
        capture_scratch = root / "capture-scratch.raw"
        restore_scratch = root / "restore-scratch.raw"
        share = root / "share"
        _write_pattern(layer, 1024 * 1024, 0x3C)
        _write_pattern(capture_scratch, 1024 * 1024, 0xA5)
        _write_pattern(restore_scratch, 1024 * 1024, 0x5A)
        share.mkdir()
        (share / "host-seed").write_bytes(PLATFORM_FILESYSTEM_HOST_SEED)
        boot_command = workload_boot_command(
            executable, backend, kernel, initrd, memory_mib, ""
        )
        # Without --mount, the capture leaves the filesystem slot dormant.
        capture_snapshot(
            [
                *boot_command,
                "--snapshot-destination",
                str(snapshot_path),
                "--snapshot-tier",
                "platform",
                "--microvm-sandbox-block",
                _block_arg("distro", layer, read_only=True),
                "--microvm-sandbox-block",
                _block_arg("scratch", capture_scratch, read_only=False),
            ],
            snapshot_path,
            timeout=timeout,
            processors=1,
            post_restore_script=_read_script("platform-filesystem.sh"),
            log_path=output_dir / "platform-filesystem-capture.log",
        )
        fingerprint = _scratch_snapshot_fingerprint(snapshot_path)

        command = snapshot_restore_command(executable, backend, snapshot_path)
        command.extend(
            (
                "--microvm-sandbox-block",
                _block_arg("distro", layer, read_only=True),
                "--microvm-sandbox-block",
                _block_arg("scratch", restore_scratch, read_only=False),
                "--mount",
                f"/mnt/late,{share},rw",
            )
        )
        context = "platform filesystem restore"
        output = _measure_restore(
            command,
            context=context,
            environment=_restore_environment(),
            timeout=timeout,
            log_path=output_dir / "platform-filesystem-restore.log",
            failure_marker=PLATFORM_FILESYSTEM_FAILURE_MARKER,
        )
        for marker in (PLATFORM_POST_RESTORE_MARKER, PLATFORM_FILESYSTEM_MARKER):
            if not contains_output_line(output, marker):
                raise RuntimeError(f"{context} did not report {marker.decode()!r}")
        result = share / "guest-result"
        if (
            not result.is_file()
            or result.read_bytes() != PLATFORM_FILESYSTEM_GUEST_RESULT
        ):
            raise RuntimeError(f"{context} did not write the guest's result")
        if _scratch_snapshot_fingerprint(snapshot_path) != fingerprint:
            raise RuntimeError(f"{context} modified snapshot artifacts")


def run_platform_aggregate_export(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Attach an aggregate of host directories and files to the filesystem
    slot that a platform snapshot left dormant, and use it from the clone.

    The aggregate is the filesystem-shares scenario's: a read-write workspace
    and a read-only tool cache, each with a denied path, a read-write data
    directory whose root exposes only its allowed paths and whose notes alone
    are writable, and a read-only and a read-write file. The clone keeps its
    captured command line, so its guest mounts the aggregate itself, checks
    each child's kind, mode, and policy through the aggregate's root, and binds
    a directory and a file child; the host then checks that the guest changed
    exactly what the policy lets it change. Before the guest runs, OpenVMM must
    refuse the restores whose aggregates a cold boot refuses, and on a Windows
    host caller ownership, and no restore may change the snapshot.
    """
    with tempfile.TemporaryDirectory(
        prefix="nvx-platform-aggregate-export-"
    ) as temporary:
        root = Path(temporary)
        snapshot_path = root / "snapshot"
        layer = root / "distro.erofs"
        capture_scratch = root / "capture-scratch.raw"
        restore_scratch = root / "restore-scratch.raw"
        outside = root / "outside"
        _write_pattern(layer, 1024 * 1024, 0x3C)
        _write_pattern(capture_scratch, 1024 * 1024, 0xA5)
        _write_pattern(restore_scratch, 1024 * 1024, 0x5A)
        outside.mkdir()
        workspace, toolcache, data, files = _create_shared_trees(root)
        settings, output = _shared_files(files)
        toolcache_contents = _tree_contents(toolcache)
        data_contents = _tree_contents(data)
        files_contents = _tree_contents(files)
        boot_command = workload_boot_command(
            executable, backend, kernel, initrd, memory_mib, ""
        )
        # Without --mount or --mount-aggregate, the capture leaves the
        # filesystem slot dormant.
        capture_snapshot(
            [
                *boot_command,
                "--snapshot-destination",
                str(snapshot_path),
                "--snapshot-tier",
                "platform",
                "--microvm-sandbox-block",
                _block_arg("distro", layer, read_only=True),
                "--microvm-sandbox-block",
                _block_arg("scratch", capture_scratch, read_only=False),
            ],
            snapshot_path,
            timeout=timeout,
            processors=1,
            post_restore_script=_read_script("platform-aggregate-export.sh"),
            log_path=output_dir / "platform-aggregate-export-capture.log",
        )
        fingerprint = _scratch_snapshot_fingerprint(snapshot_path)

        def restore_command(*arguments: str) -> list[str]:
            command = snapshot_restore_command(executable, backend, snapshot_path)
            command.extend(
                (
                    "--microvm-sandbox-block",
                    _block_arg("distro", layer, read_only=True),
                    "--microvm-sandbox-block",
                    _block_arg("scratch", restore_scratch, read_only=False),
                    *arguments,
                )
            )
            return command

        aggregate = (
            *_aggregate_arguments(workspace, toolcache, data, files),
            "--mount-deny",
            str(workspace / "secrets"),
            "--mount-deny",
            str(toolcache / "credentials"),
        )
        context = "platform aggregate export restore"
        restored = _measure_restore(
            restore_command(*aggregate),
            context=context,
            environment=_restore_environment(),
            timeout=timeout,
            log_path=output_dir / "platform-aggregate-export-restore.log",
            failure_marker=PLATFORM_AGGREGATE_FAILURE_MARKER,
        )
        for marker in (PLATFORM_POST_RESTORE_MARKER, PLATFORM_AGGREGATE_MARKER):
            if not contains_output_line(restored, marker):
                raise RuntimeError(f"{context} did not report {marker.decode()!r}")
        guest_write = workspace / "from-guest"
        if (
            not guest_write.is_file()
            or guest_write.read_bytes() != b"NVX-GUEST-WRITE\n"
        ):
            raise RuntimeError(f"{context} did not write to the read-write directory")
        if not (workspace / "guest-directory").is_dir():
            raise RuntimeError(f"{context} did not create the guest's directory")
        if (workspace / "secrets" / "token").read_bytes() != b"NVX-SECRET":
            raise RuntimeError(f"{context} changed a denied path")
        if _tree_contents(toolcache) != toolcache_contents:
            raise RuntimeError(f"{context} changed the read-only directory")
        data_contents["notes one.txt"] = b"NVX-NOTES\nNVX-GUEST-NOTE\n"
        if _tree_contents(data) != data_contents:
            raise RuntimeError(
                f"{context} changed the data directory beyond its writable path"
            )
        files_contents["results/output.txt"] = b"NVX-GUEST-OUTPUT\n"
        if _tree_contents(files) != files_contents:
            raise RuntimeError(
                f"{context} changed the shared files' directories, the read-only "
                "file, or the read-write file other than as it wrote"
            )
        if _scratch_snapshot_fingerprint(snapshot_path) != fingerprint:
            raise RuntimeError(f"{context} modified snapshot artifacts")
        workspace_contents = _tree_contents(workspace)

        rejected_arguments = {
            "overlapping-children": (
                "--mount-aggregate",
                "/run/nvx/shares",
                "--mount-child",
                f"0,{toolcache},ro",
                "--mount-child",
                f"1,{toolcache / 'tools'},ro",
            ),
            "file-in-directory": (
                "--mount-aggregate",
                "/run/nvx/shares",
                "--mount-child",
                f"0,{settings.parent},rw",
                "--mount-child",
                f"1,{settings},ro,file",
            ),
            "directory-as-file": (
                "--mount-aggregate",
                "/run/nvx/shares",
                "--mount-child",
                f"0,{settings.parent},ro,file",
            ),
            "file-as-directory": (
                "--mount-aggregate",
                "/run/nvx/shares",
                "--mount-child",
                f"0,{settings},ro",
            ),
            "repeated-name": (
                "--mount-aggregate",
                "/run/nvx/shares",
                "--mount-child",
                f"same,{workspace},rw",
                "--mount-child",
                f"same,{toolcache},ro",
            ),
            "file-policy": (*aggregate, "--mount-deny", str(output)),
            "outside-policy": (*aggregate, "--mount-deny", str(outside)),
            "relative-policy": (*aggregate, "--mount-deny", "secrets"),
        }
        rejected = [
            (name, rejected_arguments[name], reason)
            for name, reason in PLATFORM_AGGREGATE_REJECTED
        ]
        if sys.platform == "win32":
            rejected.append(
                (
                    "caller-owner",
                    (*aggregate, "--mount-owner", "caller"),
                    PLATFORM_AGGREGATE_OWNER_REJECTION,
                )
            )
        for name, arguments, reason in rejected:
            _expect_process_failure(
                restore_command(*arguments),
                output_dir / f"platform-aggregate-export-rejected-{name}.log",
                timeout,
                reason,
                forbidden_markers=PLATFORM_AGGREGATE_GUEST_MARKERS,
            )
        if _scratch_snapshot_fingerprint(snapshot_path) != fingerprint:
            raise RuntimeError(
                "a refused platform aggregate export restore modified snapshot "
                "artifacts"
            )
        if (
            _tree_contents(workspace) != workspace_contents
            or _tree_contents(toolcache) != toolcache_contents
            or _tree_contents(data) != data_contents
            or _tree_contents(files) != files_contents
        ):
            raise RuntimeError(
                "a refused platform aggregate export restore changed a shared tree"
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
    """Restore snapshots after a downtime longer than the RCU stall timeout,
    and again after a simulated year.

    Every snapshot is captured first, so one shared downtime window covers
    them all. The restored guest must finish its time ABI restore, report no
    RCU stall, and show that monotonic time advanced by the downtime. Each
    snapshot is then restored once more with OpenVMM's downtime-add-s hook,
    whose downtime ends just below the time ABI's 365-day bound, as
    :func:`check_simulated_year` checks, and the first once with the bound
    plus one second, which OpenVMM must refuse with E_DOWNTIME_EXCESSIVE
    before the guest runs.
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
    year_preparation = _read_script("restore-year-capture.sh")
    year_report = _render_script(
        "restore-year.sh.in", SETTLE_SECONDS=str(RESTORE_DOWNTIME_SETTLE_SECONDS)
    )
    environment = {"OPENVMM_LOG": TIME_ABI_RESTORE_LOG_FILTER}
    with tempfile.TemporaryDirectory(prefix="nvx-restore-downtime-") as temporary:
        snapshots: list[tuple[str, int, bool, Path, float]] = []
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
            # The real downtime of a restore is at most the time since this.
            started = time.monotonic()
            # The restored guest keeps running after the restore marker so the
            # harness can stage its downtime check; the post-restore status
            # query waits for the restore to finish first.
            capture_snapshot(
                [*boot_command, "--snapshot-destination", str(snapshot_path)],
                snapshot_path,
                timeout=timeout,
                processors=processors,
                teardown_mode="host-terminate",
                pre_capture_script=year_preparation,
                pre_capture_seconds=RESTORE_YEAR_PREPARATION_SECONDS,
                post_restore_script=post_restore_checks(),
                log_path=output_dir / f"restore-downtime-{name}-capture.log",
            )
            snapshots.append((name, processors, expedited, snapshot_path, started))
        restore_after = time.monotonic() + RESTORE_DOWNTIME_SECONDS
        ready: dict[str, float] = {}
        for name, processors, _, snapshot_path, _ in snapshots:
            time.sleep(max(0.0, restore_after - time.monotonic()))
            context = f"{name} restore after a {RESTORE_DOWNTIME_SECONDS:g} s downtime"
            launched = time.monotonic()
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
                ready[name] = time.monotonic() - launched
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
        # The expedited cases simulate a reboot, so that the downtime comes
        # from UTC, at both processor counts.
        for name, processors, expedited, snapshot_path, started in snapshots:
            _restore_simulated_year(
                executable,
                backend,
                snapshot_path,
                name=name,
                processors=processors,
                utc=expedited,
                started=started,
                baseline_ready=ready[name],
                report=year_report,
                timeout=timeout,
                output_dir=output_dir,
            )
        name, processors, _, snapshot_path, _ = snapshots[0]
        _expect_process_failure(
            [
                *snapshot_restore_command(
                    executable, backend, snapshot_path, processors=processors
                ),
                "--x-time-abi-test-hook",
                f"downtime-add-s={MAX_DOWNTIME_SECONDS + 1}",
            ],
            output_dir / f"restore-downtime-{name}-excessive.log",
            timeout,
            b"fatal error: [E_DOWNTIME_EXCESSIVE]",
            forbidden_markers=(RESTORE_MARKER,),
        )


def _restore_simulated_year(
    executable: Path,
    backend: str,
    snapshot_path: Path,
    *,
    name: str,
    processors: int,
    utc: bool,
    started: float,
    baseline_ready: float,
    report: str,
    timeout: float,
    output_dir: Path,
) -> None:
    """Restore ``snapshot_path``, whose capture started at ``started``, after a
    simulated downtime just below the time ABI's bound, from UTC with ``utc``
    and otherwise from the same boot's monotonic clock, and check the guest's
    report as :func:`check_simulated_year` does, and that the restore printed
    its marker at most RESTORE_YEAR_READY_ALLOWANCE_SECONDS later than its
    restore after the real downtime did, ``baseline_ready`` seconds after it
    started."""
    simulated = (
        MAX_DOWNTIME_SECONDS
        - math.ceil(time.monotonic() - started)
        - RESTORE_YEAR_MARGIN_SECONDS
    )
    source = "utc" if utc else "monotonic"
    context = f"{name} restore after a simulated {simulated} s downtime from {source}"
    command = [
        *snapshot_restore_command(
            executable, backend, snapshot_path, processors=processors
        ),
        "--x-time-abi-test-hook",
        f"downtime-add-s={simulated}",
    ]
    if utc:
        command.extend(("--x-time-abi-test-hook", "boot-id-mismatch"))
    launched = time.monotonic()
    with OpenvmmProcess(
        command,
        output_dir / f"restore-downtime-{name}-year.log",
        environment={"OPENVMM_LOG": TIME_ABI_RESTORE_LOG_FILTER},
    ) as process:
        process.wait_for_line(RESTORE_MARKER, timeout)
        ready = time.monotonic() - launched
        process.wait_for_time_abi("restore", TIME_ABI_RESTORE_FINISH_SECONDS)
        _stage_script(process, RESTORE_YEAR_PATH, "NVX_RESTORE_YEAR", report)
        process.wait_for_line(RESTORE_YEAR_COMPLETION_MARKER, timeout)
        host_utc_ns = time.time_ns()
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
        summary = check_simulated_year(
            simulated_year_report(result.output),
            simulated_seconds=simulated,
            source=source,
            host_utc_ns=host_utc_ns,
        )
    except (TimeAbiFailure, RuntimeError) as error:
        raise RuntimeError(f"{context}: {error}") from error
    print(
        f"{context}: {summary}; ready {ready:.1f} s after its start, "
        f"{baseline_ready:.1f} s after a {RESTORE_DOWNTIME_SECONDS:g} s downtime",
        flush=True,
    )
    if ready > baseline_ready + RESTORE_YEAR_READY_ALLOWANCE_SECONDS:
        raise RuntimeError(
            f"{context}: the restore printed its marker {ready:.1f} s after its "
            f"start, {ready - baseline_ready:.1f} s later than after a "
            f"{RESTORE_DOWNTIME_SECONDS:g} s downtime"
        )


def simulated_year_report(output: bytes) -> dict[str, str]:
    """Return the fields of the simulated one-year restore's report line."""
    for line in _output_lines(output):
        text = line.decode("utf-8", "replace")
        if text.startswith(RESTORE_YEAR_REPORT_PREFIX):
            try:
                return parse_fields(text.removeprefix(RESTORE_YEAR_REPORT_PREFIX))
            except ValueError as error:
                raise RuntimeError(
                    f"the simulated one-year restore's report is malformed: {error}"
                ) from error
    raise RuntimeError("the guest printed no simulated one-year restore report")


def _clock_ns(value: str) -> int:
    """Return a clock reading of whole and fractional seconds in nanoseconds."""
    seconds, _, fraction = value.partition(".")
    if not seconds.isdecimal() or not fraction.isdecimal() or len(fraction) > 9:
        raise ValueError(f"malformed clock reading {value!r}")
    return int(seconds) * 1_000_000_000 + int(fraction.ljust(9, "0"))


def check_simulated_year(
    report: dict[str, str],
    *,
    simulated_seconds: int,
    source: str,
    host_utc_ns: int,
) -> str:
    """Check the report of a restore after a simulated year (doc/ci.md), and
    return a summary of its downtime, clocks, timer, and frozen workload.

    The guest must report no RCU stall and no kernel warning since the
    capture; a downtime from ``source`` of at least the ``simulated_seconds``
    that the hook added and at most the time ABI's bound; clocks that
    advanced by the simulated downtime more than its realtime clock, which
    stands on host UTC, ``host_utc_ns`` once the report arrived: its raw
    monotonic clock by the TSC's advance at the declared rate, as the kernel
    counts it to the kHz, and its monotonic and boot-time clocks by that
    advance at the frequency correction that it held before the capture; the
    one-hour timer that it armed before the capture fired within
    RESTORE_YEAR_TIMER_DELAY_SECONDS of the restore's clock step, before or
    after it; a frozen workload that gained no CPU time or cgroup usage while
    it stayed frozen across the restore, and about a second's when it ran for
    one; and a requester that the kernel did not charge the downtime either.
    """
    second = 1_000_000_000
    simulated = simulated_seconds * second
    problems: list[str] = []
    summary: list[str] = []
    try:
        if report["stalls"] != "0":
            problems.append(f"rcu_stall_count is {report['stalls']}")
        if report["kernel_warnings"] != "0":
            problems.append(
                f"the kernel logged {report['kernel_warnings']} warnings since "
                "the capture"
            )
        if report["downtime_source"] != source:
            problems.append(
                f"the downtime came from {report['downtime_source']}, not {source}"
            )
        downtime = int(report["downtime_ns"])
        if not simulated <= downtime <= MAX_DOWNTIME_SECONDS * second:
            problems.append(
                f"the downtime is {downtime} ns, not from the simulated "
                f"{simulated} ns to the {MAX_DOWNTIME_SECONDS} s bound"
            )
        summary.append(f"downtime {downtime} ns")
        clocks = ("realtime", "monotonic", "monotonic_raw", "boottime")
        before = {clock: _clock_ns(report[f"{clock}_before"]) for clock in clocks}
        after = {clock: _clock_ns(report[f"{clock}_after"]) for clock in clocks}
        real = after["realtime"] - before["realtime"]
        # The restore advanced the TSC by the downtime at the declared rate,
        # which the kernel counts to the kHz, raw and at its frequency
        # correction.
        tsc_hz = int(report["tsc_hz"])
        kernel_hz = tsc_hz // 1000 * 1000
        frequency_ppb = int(report["frequency_ppb"])
        raw = simulated * tsc_hz // kernel_hz
        corrected = raw + raw * frequency_ppb // second
        multiplier = simulated * RESTORE_YEAR_MULTIPLIER_PPB // second
        allowances = {
            "monotonic_raw": RESTORE_YEAR_CLOCK_TOLERANCE_SECONDS * second + multiplier,
            "monotonic": RESTORE_YEAR_CLOCK_TOLERANCE_SECONDS * second
            + multiplier
            + simulated * RESTORE_YEAR_SLEW_PPM // 1_000_000,
        }
        allowances["boottime"] = allowances["monotonic"]
        gains: list[str] = []
        for clock, expected in (
            ("monotonic_raw", raw),
            ("monotonic", corrected),
            ("boottime", corrected),
        ):
            gained = after[clock] - before[clock] - real
            gains.append(f"{clock} {gained - simulated:+d} ns")
            if abs(gained - expected) > allowances[clock]:
                problems.append(
                    f"{clock} time advanced {gained} ns more than realtime, not "
                    f"the {expected} ns that the simulated downtime gives it"
                )
        summary.append(
            "beyond the simulated downtime, "
            + ", ".join(gains)
            + f" at {tsc_hz} Hz and {frequency_ppb} ppb"
        )
        lag = host_utc_ns - after["realtime"]
        if abs(lag) > RESTORE_YEAR_REALTIME_TOLERANCE_SECONDS * second:
            problems.append(f"realtime stands {lag} ns behind host UTC")
        if report["timer_s"] == "none":
            problems.append("the timer armed before the capture never fired")
        else:
            fired = _clock_ns(report["timer_s"])
            stepped = after["boottime"] - (
                after["realtime"] - int(report["step_realtime_ns"])
            )
            delay = fired - stepped
            summary.append(f"timer {delay:+d} ns after the step")
            if abs(delay) > RESTORE_YEAR_TIMER_DELAY_SECONDS * second:
                when = "after" if delay > 0 else "before"
                problems.append(
                    f"the timer fired {abs(delay)} ns {when} the restore's step"
                )
        frozen_ticks = int(report["ticks_frozen"]) - int(report["ticks_before"])
        frozen_usage = int(report["usage_frozen"]) - int(report["usage_before"])
        if (
            frozen_ticks > RESTORE_YEAR_FROZEN_TICKS
            or frozen_usage > RESTORE_YEAR_FROZEN_USAGE_USEC
        ):
            problems.append(
                f"the frozen workload gained {frozen_ticks} ticks of CPU time and "
                f"{frozen_usage} us of cgroup usage across the restore"
            )
        ran_ticks = int(report["ticks_thawed"]) - int(report["ticks_frozen"])
        ran_usage = int(report["usage_thawed"]) - int(report["usage_frozen"])
        if not (
            RESTORE_YEAR_THAWED_MIN_TICKS <= ran_ticks <= RESTORE_YEAR_THAWED_MAX_TICKS
            and RESTORE_YEAR_THAWED_MIN_USAGE_USEC
            <= ran_usage
            <= RESTORE_YEAR_THAWED_MAX_USAGE_USEC
        ):
            problems.append(
                f"the thawed workload gained {ran_ticks} ticks of CPU time and "
                f"{ran_usage} us of cgroup usage in a second"
            )
        summary.append(
            f"frozen workload +{frozen_ticks} ticks and +{frozen_usage} us, then "
            f"+{ran_ticks} ticks and +{ran_usage} us in a second"
        )
        # The guest kernel charges a running task at most a second of one
        # runtime update, so the requester, which ran at the capture, is not
        # charged the downtime either.
        reaped = int(report["reaped_ticks"])
        summary.append(
            f"the console shell's reaped children, among them the capture's "
            f"requester, {reaped} ticks"
        )
        if reaped > RESTORE_YEAR_REAPED_MAX_TICKS:
            problems.append(
                f"the console shell's reaped children, among them the capture's "
                f"requester, used {reaped} ticks of CPU time"
            )
    except (KeyError, ValueError) as error:
        problems.append(f"the report is malformed: {error}")
    if problems:
        raise RuntimeError("simulated one-year restore failed: " + "; ".join(problems))
    return "; ".join(summary)


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
    the time ABI status, then whether RCU stall detection is still suppressed.

    They form one console line, which the shell reads whole before it runs
    any of it, so that the terminal echoes none of it into the status lines
    (#785).
    """
    rcu = CANCELED_CAPTURE_RCU_PREFIX.decode()
    done = CANCELED_CAPTURE_DONE_MARKER.decode()
    # A quote pair splits each marker, so the console's echo of the command,
    # which the tty may wrap, never contains it.
    return (
        status_script().removesuffix("\n")
        + f'; echo "{rcu[:4]}""{rcu[4:]}$(cat {RCU_STALL_SUPPRESS_PATH})"; '
        + f'echo "{done[:4]}""{done[4:]}"\n'
    )


def check_canceled_capture(
    output: bytes,
    command: Sequence[str],
    *,
    context: str = "after the released snapshot request",
) -> None:
    """Check that a released snapshot request left the source as it was.

    The snapshot agent saves and overrides the stall detectors' settings
    before the request; when the request returns in the source, it removes its
    barriers and restores the saved values (nvx-time cancel-capture). The
    source then reports a passing status at generation 0 with no restore, and
    RCU stall detection is no longer suppressed.
    """
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
            port_access = _single_marker_value(
                restored.output, b"NVX-SNAPSHOT-PORT-ACCESS-"
            )
            if port_access != b"denied":
                raise RuntimeError(
                    f"snapshot restore {restore_index} started a process that reached "
                    f"OpenVMM's ports: {port_access.decode(errors='replace')}"
                )
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


SNAPSHOT_ROLLBACK_STATUS_PREFIX = b"NVX-SNAPSHOT-ROLLBACK-STATUS-"
SNAPSHOT_ROLLBACK_CONTINUED_MARKER = b"NVX-SNAPSHOT-ROLLBACK-CONTINUED"
SNAPSHOT_ROLLBACK_RESTORED_STATUS_PREFIX = b"NVX-SNAPSHOT-ROLLBACK-RESTORED-STATUS-"
SNAPSHOT_ROLLBACK_RESTORED_MARKER = b"NVX-SNAPSHOT-ROLLBACK-RESTORED"
CAPTURE_PROFILE_PHASE = re.compile(
    rb"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture phase=([a-z_]+) "
)


def snapshot_request_line(status_prefix: bytes, done_marker: bytes) -> str:
    """Return a console line that requests a snapshot, then prints
    ``status_prefix`` with the request's exit status and, on a line of its
    own, ``done_marker``.

    A quote pair splits each marker, so the console's echo of the line, which
    the tty may wrap, never contains it.
    """
    status = status_prefix.decode()
    done = done_marker.decode()
    return (
        f'nvx-snapshot; echo "{status[:4]}""{status[4:]}$?"; '
        f'echo "{done[:4]}""{done[4:]}"'
    )


def capture_profile_phases(output: bytes) -> list[str]:
    """Return the phases of the capture profile records in ``output``, in the
    order in which OpenVMM wrote them.

    OpenVMM writes a record and its newline separately, so console output can
    share its line; the records are found wherever they are.
    """
    return [match.group(1).decode() for match in CAPTURE_PROFILE_PHASE.finditer(output)]


def run_snapshot_rollback(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Fail a capture after a complete quiesce, and check that the source
    resumes and can capture again.

    The snapshot destination's parent disappears after boot, so the capture
    quiesces and saves the VM, and then fails to publish, which rolls it back:
    the guest's request returns in the source, as for a request that OpenVMM
    releases (``snapshot-core``), and the capture's profile records show the
    quiesce and the save, but no publication. Once the parent is back, a
    second request of the same source publishes a snapshot, whose restore
    continues past that request.
    """
    with tempfile.TemporaryDirectory(prefix="nvx-snapshot-rollback-") as temporary:
        root = Path(temporary)
        parent = root / "parent"
        snapshot_path = parent / "snapshot"
        parent.mkdir()
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
        # OpenVMM's own RAM backing would live in the destination's parent and
        # keep it from going away, so the RAM has a file of its own.
        memory = capture_command.index("--memory") + 1
        capture_command[memory] = f"size={memory_mib}M,file={root / 'memory.bin'}"
        with OpenvmmProcess(
            capture_command,
            output_dir / "snapshot-rollback-capture.log",
            environment={SNAPSHOT_PROFILE_ENV: "1"},
        ) as process:
            process.wait_for(BOOT_MARKER, timeout)
            parent.rmdir()
            process.send_line(
                snapshot_request_line(
                    SNAPSHOT_ROLLBACK_STATUS_PREFIX, SNAPSHOT_ROLLBACK_CONTINUED_MARKER
                )
            )
            process.wait_for_line(SNAPSHOT_ROLLBACK_CONTINUED_MARKER, timeout)
            failed = process.output
            status = _single_marker_value(failed, SNAPSHOT_ROLLBACK_STATUS_PREFIX)
            if status != b"0":
                raise RuntimeError(
                    "the snapshot request that OpenVMM rolled back exited with "
                    f"{status.decode(errors='replace')}"
                )
            phases = capture_profile_phases(failed)
            if phases.count("quiesce") != 1 or phases.count("save_state") != 1:
                raise RuntimeError(
                    "the failed capture did not quiesce and save the VM once "
                    f"before it returned: capture phases {phases}"
                )
            if "publication" in phases or "publication_commit" in phases:
                raise RuntimeError(
                    f"the failed capture published a snapshot: capture phases {phases}"
                )
            if parent.exists():
                raise RuntimeError("the failed capture recreated its parent directory")
            checks_start = len(process.output)
            process.send_bytes(canceled_capture_checks().encode())
            process.wait_for_line(
                CANCELED_CAPTURE_DONE_MARKER, max(timeout, STATUS_TIMEOUT_SECONDS)
            )
            check_canceled_capture(
                process.output[checks_start:],
                capture_command,
                context="after the rolled-back capture",
            )
            parent.mkdir()
            process.send_line(
                snapshot_request_line(
                    SNAPSHOT_ROLLBACK_RESTORED_STATUS_PREFIX,
                    SNAPSHOT_ROLLBACK_RESTORED_MARKER,
                )
                + "; nvx-exit 0"
            )
            source = process.wait(timeout)
        if source.returncode != 0:
            raise RuntimeError(
                f"snapshot source OpenVMM exited with {source.returncode}"
            )
        if SNAPSHOT_ROLLBACK_RESTORED_MARKER in _output_lines(source.output):
            raise RuntimeError("snapshot source crossed its terminal capture boundary")
        phases = capture_profile_phases(source.output)
        if phases.count("quiesce") != 2 or phases.count("publication_commit") != 1:
            raise RuntimeError(
                "the source did not quiesce twice and publish once: "
                f"capture phases {phases}"
            )
        if list(parent.iterdir()) != [snapshot_path]:
            raise RuntimeError(
                "the snapshot's parent holds more than the snapshot: "
                f"{sorted(path.name for path in parent.iterdir())}"
            )
        with OpenvmmProcess(
            snapshot_restore_command(executable, backend, snapshot_path),
            output_dir / "snapshot-rollback-restore.log",
        ) as process:
            process.wait_for_line(SNAPSHOT_ROLLBACK_RESTORED_MARKER, timeout)
            restored = process.wait(timeout)
        if restored.returncode != 0:
            raise RuntimeError(f"snapshot restore exited with {restored.returncode}")
        status = _single_marker_value(
            restored.output, SNAPSHOT_ROLLBACK_RESTORED_STATUS_PREFIX
        )
        if status != b"0":
            raise RuntimeError(
                "the restored snapshot request exited with "
                f"{status.decode(errors='replace')}"
            )
        if _output_lines(restored.output).count(SNAPSHOT_ROLLBACK_RESTORED_MARKER) != 1:
            raise RuntimeError("snapshot restore did not continue exactly once")


# The stable fragment of OpenVMM's refusal of a boot console on TCP.
CONSOLE_TCP_REFUSED = b"microVM consoles do not accept TCP endpoints"
# How long OpenVMM may take to refuse a boot console, which it does before it
# opens any host resource.
CONSOLE_TCP_REFUSAL_TIMEOUT_SECONDS = 30.0


def check_tcp_boot_console_refused(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Check that OpenVMM refuses a boot console on TCP before it boots a guest.

    Unless host loopback is denied, a guest's gateway maps the guest's TCP
    flows onto host loopback, so the guest could connect to a loopback TCP
    listener, its own console's or another VM's, and drive the shell on it,
    or to the listener that a client console connects to (microsoft/nvx#551).
    OpenVMM refuses both, with a network and without one.
    """
    _, port = _available_tcp_address()
    for name, network, endpoint in (
        ("listen", None, f"listen=tcp:127.0.0.1:{port}"),
        (
            "networked-listen",
            DIRECTIONAL_NETWORK_CIDR,
            f"listen=tcp:127.0.0.1:{port}",
        ),
        (
            "networked-connect",
            DIRECTIONAL_NETWORK_CIDR,
            f"connect=tcp:127.0.0.1:{port}",
        ),
    ):
        command = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                network=network,
                boot_console=False,
            ),
            "--virtio-console",
            endpoint,
        ]
        with OpenvmmProcess(
            command, output_dir / f"console-tcp-{name}-refused.log"
        ) as process:
            result = process.wait(min(timeout, CONSOLE_TCP_REFUSAL_TIMEOUT_SECONDS))
        if result.returncode == 0 or CONSOLE_TCP_REFUSED not in result.output:
            raise RuntimeError(f"OpenVMM did not refuse the boot console {endpoint}")


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
    check_tcp_boot_console_refused(
        executable,
        kernel,
        initrd,
        backend,
        memory_mib=memory_mib,
        timeout=timeout,
        output_dir=output_dir,
    )
    script, tx_count, queued_rx = _console_snapshot_script(backend)
    with tempfile.TemporaryDirectory(prefix="nvx-console-snapshot-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        # A listener's socket is beside the snapshot that records it.
        endpoint = _console_endpoint(Path(temporary))
        capture_command = [
            *workload_boot_command(
                executable,
                backend,
                kernel,
                initrd,
                memory_mib,
                "quiet loglevel=0",
                boot_console=False,
            ),
            "--snapshot-destination",
            str(snapshot_path),
            "--virtio-console",
            f"listen={endpoint}",
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
                console = BootConsole.connect(
                    endpoint,
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
                _queue_console_rx_and_request_snapshot(console, queued_rx, timeout)
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
                executable, backend, snapshot_path, boot_console=False
            )
            with OpenvmmProcess(
                restore_command,
                output_dir / f"console-snapshot-restore-{restore_index}-process.log",
            ) as process:
                try:
                    console = BootConsole.connect(
                        endpoint, timeout, monitor=TimeAbiMonitor(restore_command)
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


def run_run_console_restore(
    executable: Path,
    kernel: Path,
    initrd: Path,
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
) -> None:
    """Restore through `nvx-rs run` a snapshot whose boot console was on the
    terminal, and type into the restored guest's shell.

    A direct cold boot attaches the boot console to OpenVMM's standard
    streams with `--virtio-console console`, and the guest captures itself
    there. OpenVMM restores such a snapshot only with `--virtio-console
    console` again, which `nvx-rs run` must pass, as its snapshot's manifest
    asks, so that the restored shell takes the terminal's input: the scenario
    types a script into it, which shuts the guest down."""
    with tempfile.TemporaryDirectory(prefix="nvx-run-console-restore-") as temporary:
        snapshot_path = Path(temporary) / "snapshot"
        boot_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "quiet loglevel=0",
        )
        # The restored guest keeps its shell after the restore marker, for the
        # script that the restore types.
        capture_snapshot(
            [*boot_command, "--snapshot-destination", str(snapshot_path)],
            snapshot_path,
            timeout=timeout,
            teardown_mode="host-terminate",
            log_path=output_dir / "run-console-restore-capture.log",
        )
        fingerprint = _snapshot_fingerprint(snapshot_path)
        command = [
            *nvx_rs.command_line(("run",)),
            "--hypervisor",
            backend,
            "--restore-snapshot",
            str(snapshot_path),
        ]
        result = run_guest_script(
            command,
            _read_script("run-console-restore.sh"),
            RUN_CONSOLE_RESTORE_MARKER,
            timeout=timeout,
            log_path=output_dir / "run-console-restore.log",
            boot_marker=RESTORE_MARKER,
            contain_process_tree=True,
        )
        printed = result["text"].split("\n", 1)[0].removesuffix("\r")
        if not printed.startswith(">> ") or not printed.endswith(
            " --virtio-console console"
        ):
            raise RuntimeError(
                "nvx-rs run did not attach the boot console of the snapshot to "
                f"the terminal: {printed}"
            )
        if _snapshot_fingerprint(snapshot_path) != fingerprint:
            raise RuntimeError("the restore of nvx-rs run modified the snapshot")


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
        # Only a platform snapshot's dormant slot binds an aggregate.
        aggregate_restore = snapshot_restore_command(
            executable, backend, dormant_snapshot
        )
        aggregate_restore.extend(
            ("--mount-aggregate", "/mnt/late", "--mount-child", f"0,{late_root},rw")
        )
        _expect_process_failure(
            aggregate_restore,
            output_dir / "filesystem-dormant-aggregate.log",
            timeout,
            DORMANT_AGGREGATE_REJECTION,
        )
        if _snapshot_fingerprint(dormant_snapshot) != dormant_fingerprint:
            raise RuntimeError(
                "dormant filesystem restores modified snapshot artifacts"
            )

        live_root = root / "live-root"
        live_root.mkdir()
        open_handle = live_root / "open-handle"
        open_handle.write_bytes(b"")
        # The guest's write through a shared mapping replaces the page, and
        # each restore's host change replaces it in place, so that the file
        # stays the object that the mapping holds.
        mapped_page = live_root / "mapped-page"
        mapped_page.write_bytes(b"NVX-HOST-INITIAL")
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
        if mapped_page.read_bytes() != b"NVX-GUEST-MAPPED":
            raise RuntimeError(
                "live filesystem capture did not write back a dirty shared mapping"
            )
        assert_guest_symlink(live_root / "handle-link", "open-handle")
        assert_live_exchange(live_root)
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
            host_change = f"NVX-HOST-AFTER-{restore_index}".encode()
            with mapped_page.open("r+b") as page:
                page.write(host_change)
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
            if mapped_page.read_bytes() != host_change:
                raise RuntimeError(
                    f"live filesystem restore {restore_index} wrote captured "
                    "mapped data back to the host file"
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


def _create_shared_trees(root: Path) -> tuple[Path, Path, Path, Path]:
    """Create the workspace, the tool cache, the data directory, and the
    directory of the shared files that the aggregate of
    `_aggregate_arguments` shares, below `root`, and return them. Each holds
    paths that the aggregate's policy hides or exposes, and each shared file a
    sibling that the aggregate does not share."""
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
    return workspace, toolcache, data, files


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
        workspace, toolcache, data, files = _create_shared_trees(root)
        settings, output = _shared_files(files)
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


def run_filesystem_comma_paths(
    backend: str,
    *,
    memory_mib: int,
    timeout: float,
    output_dir: Path,
    guest: str = "alpine",
) -> None:
    """Share host directories whose paths contain commas through `nvx-rs run`:
    a read-write `alpha,beta` and a read-only `2026, final`, each with a
    denied subdirectory, first each alone, as OpenVMM's `--mount`, and then
    both, as the children of an aggregate. The guest must find each exact
    directory, rather than the one beside it that the text before the last
    comma of its path names, with its mode, which OpenVMM enforces, and
    without its denied subdirectory."""
    with tempfile.TemporaryDirectory(prefix="nvx-comma-paths-") as temporary:
        root = Path(temporary)
        alpha = root / "projects" / "alpha,beta"
        reports = root / "reports" / "2026, final"
        shares = {
            alpha: ("/mnt/alpha", "rw", "NVX-ALPHA-BETA"),
            reports: ("/mnt/reports", "ro", "NVX-REPORTS"),
        }
        for share, (_, _, seed) in shares.items():
            (share / "secret").mkdir(parents=True)
            (share / "seed").write_bytes(seed.encode())
            (share / "secret" / "token").write_bytes(b"NVX-SECRET")
            decoy = share.with_name(share.name.rsplit(",", 1)[0])
            decoy.mkdir()
            (decoy / "seed").write_bytes(b"NVX-DECOY")
        contents = _tree_contents(root)
        for name, attached in (
            ("alpha", (alpha,)),
            ("reports", (reports,)),
            ("aggregate", (alpha, reports)),
        ):
            command = [
                *nvx_rs.command_line(("run",)),
                "--guest",
                guest,
                "--hypervisor",
                backend,
                "--memory-mib",
                str(memory_mib),
                "--cmdline",
                "quiet loglevel=0",
            ]
            for share in attached:
                target, mode, _ = shares[share]
                command.extend(("--mount", f"{target},{share},{mode}"))
            for share in attached:
                command.extend(("--mount-deny", str(share / "secret")))
            run_guest_script(
                command,
                _render_script(
                    "filesystem-comma-paths.sh.in",
                    AGGREGATE="1" if len(attached) > 1 else "0",
                    SHARES=" ".join(":".join(shares[share]) for share in attached),
                ),
                FILESYSTEM_COMMA_PATHS_MARKER,
                timeout=timeout,
                log_path=output_dir / f"filesystem-comma-paths-{name}.log",
                contain_process_tree=True,
            )
            expected = dict(contents)
            if alpha in attached:
                expected["projects/alpha,beta/from-guest"] = b"NVX-GUEST-WRITE\n"
            if _tree_contents(root) != expected:
                raise RuntimeError(
                    f"the {name} shares of host paths with commas changed more "
                    "than the read-write directory's guest write"
                )
            (alpha / "from-guest").unlink(missing_ok=True)


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
        _write_pattern(wrong_layer, 2 * 1024 * 1024, 0x3C)
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
            "--snapshot-generation-id",
            SCRATCH_PAIRED_GENERATION,
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

        def paired_restore_command(
            selected_layer: Path,
            generation: str | None = SCRATCH_PAIRED_GENERATION,
        ) -> list[str]:
            command = snapshot_restore_command(executable, backend, paired_snapshot)
            command.extend(
                (
                    "--microvm-sandbox-block",
                    _block_arg("distro", selected_layer, read_only=True),
                )
            )
            if generation is not None:
                command.extend(("--restore-generation-id", generation))
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
        for name, generation, rejection in SCRATCH_PAIRED_GENERATION_REJECTIONS:
            _expect_process_failure(
                paired_restore_command(layer, generation),
                output_dir / f"scratch-paired-{name}-generation.log",
                timeout,
                rejection,
                forbidden_markers=forbidden_paired,
            )
        _expect_process_failure(
            paired_restore_command(wrong_layer),
            output_dir / "scratch-paired-wrong-layer.log",
            timeout,
            SCRATCH_PAIRED_WRONG_LAYER_REJECTION,
            forbidden_markers=forbidden_paired,
        )
        # Only a platform snapshot's fresh scratch may be absent.
        _expect_process_failure(
            [*paired_restore_command(layer), "--restore-absent-scratch"],
            output_dir / "scratch-paired-absent.log",
            timeout,
            PAIRED_ABSENT_SCRATCH_REJECTION,
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
        with published_scratch.open("ab") as scratch_file:
            scratch_file.write(bytes(512))
        try:
            _expect_process_failure(
                paired_restore_command(layer),
                output_dir / "scratch-paired-extended.log",
                timeout,
                f"scratch.img size ({len(scratch_bytes) + 512} bytes) doesn't match "
                f"manifest ({len(scratch_bytes)} bytes)".encode(),
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


def _snapshot_storage_generation(tier: str) -> str | None:
    """Return the storage generation to which a capture of ``tier`` binds its
    sandbox blocks, and which its restore states again: none of the platform
    tier, which binds no block."""
    generations = {
        "platform": None,
        "workload-start": "00112233445566778899aabbccddee01",
        "instance-checkpoint": "00112233445566778899aabbccddee02",
    }
    if tier not in generations:
        raise ValueError(f"unsupported snapshot tier {tier!r}")
    return generations[tier]


def _snapshot_storage_policy(tier: str) -> tuple[str, ...]:
    generation = _snapshot_storage_generation(tier)
    if generation is None:
        return ()
    restore_mode = "private-copy" if tier == "workload-start" else "direct-claimed"
    return (
        "--snapshot-generation-id",
        generation,
        "--snapshot-scratch-restore-mode",
        restore_mode,
    )


def _snapshot_restore_policy(tier: str) -> tuple[str, ...]:
    generation = _snapshot_storage_generation(tier)
    return () if generation is None else ("--restore-generation-id", generation)


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
        # A listener's socket is beside the snapshot that records it.
        endpoint = _console_endpoint(root)

        capture_command = workload_boot_command(
            executable,
            backend,
            kernel,
            initrd,
            memory_mib,
            "" if platform else f"nvx_hostname=restored-{tier}",
            network="10.0.0.2/24",
            boot_console=False,
        )
        capture_command.extend(
            (
                "--allow-host",
                "10.0.0.1",
                "--virtio-console",
                f"listen={endpoint}",
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
        console: BootConsole | None = None
        with OpenvmmProcess(
            capture_command,
            output_dir / f"snapshot-tier-{tier}-capture-process.log",
        ) as process:
            try:
                # The guest's shell is on this virtio console, so it answers
                # the cold boot's status query there.
                console = BootConsole.connect(
                    endpoint,
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
            boot_console=False,
        )
        restore_command.extend(
            (
                "--allow-host",
                "10.0.0.1",
                *_snapshot_restore_policy(tier),
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
            # A restore of storage of another generation fails before it
            # claims the checkpoint.
            wrong_generation = list(restore_command)
            wrong_generation[wrong_generation.index("--restore-generation-id") + 1] = (
                "00112233445566778899aabbccddee0f"
            )
            _expect_process_failure(
                wrong_generation,
                output_dir / f"snapshot-tier-{tier}-wrong-generation.log",
                timeout,
                b"the snapshot binds its sandbox blocks to storage generation",
            )
            if (snapshot / "resume.claim").exists():
                raise RuntimeError(
                    "wrong-generation restore consumed instance checkpoint"
                )

        expected_layer = layer_marker + (b"195" if platform else b"60")
        restore_console_path = output_dir / f"snapshot-tier-{tier}-restore-console.log"
        restore_console = b""
        console = None
        with OpenvmmProcess(
            restore_command,
            output_dir / f"snapshot-tier-{tier}-restore-process.log",
        ) as process:
            try:
                console = BootConsole.connect(
                    endpoint, timeout, monitor=TimeAbiMonitor(restore_command)
                )
                # A status query that one of the milestone reads below consumes
                # must still count for the wait after them.
                status_baseline = console.status_queries
                console.send_bytes(b"Z")
                console.wait_for(repair_marker, timeout)
                if workload_start:
                    console.wait_for(workload_marker, timeout)
                console.wait_for(released_marker, timeout)
                console.wait_for(expected_layer, timeout)
                # The guest's status query waits for the restore's deferred
                # checks. It then waits for one byte from here before nvx-exit,
                # so the query's lines reach the console before the VM stops.
                console.wait_for_time_abi_status(
                    STATUS_TIMEOUT_SECONDS, since=status_baseline
                )
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
            timeout_console: BootConsole | None = None
            timeout_console_path = (
                output_dir / f"snapshot-tier-{tier}-timeout-console.log"
            )
            timeout_output = b""
            with OpenvmmProcess(
                timeout_command,
                output_dir / f"snapshot-tier-{tier}-timeout-process.log",
            ) as process:
                try:
                    timeout_console = BootConsole.connect(
                        endpoint, timeout, monitor=TimeAbiMonitor(timeout_command)
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
    debug_kernel = getattr(args, "debug_kernel", False)
    shard: tuple[int, int] | None = getattr(args, "shard", None)
    if shard is not None and (
        args.scenario is not None or debug_kernel or descriptor.name != "alpine"
    ):
        raise ScriptError(
            "--shard runs a shard of the default Alpine suite on the default "
            "kernel, so it takes neither --scenario, --debug-kernel, nor another "
            "--guest"
        )
    if args.memory_mib is None:
        args.memory_mib = descriptor.default_memory_mib
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
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
    if shard is not None:
        # Fails unless every default scenario is in exactly one shard of every
        # backend, and the backend has as many shards as CI runs.
        scenarios = shard_scenarios(
            load_partition(), args.backend, shard, default_suite()
        )
    elif args.scenario is None:
        defaults = DEBUG_KERNEL_SCENARIOS if debug_kernel else MICROVM_TEST_SCENARIOS
        scenarios = tuple(
            scenario for scenario in defaults if scenario not in unsupported_scenarios
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
    # A default run runs the public sandbox scenarios too, so their Ubuntu
    # artifacts must be there before any scenario runs.
    public = [
        scenario for scenario in scenarios if scenario in PUBLIC_SANDBOX_SCENARIOS
    ]
    if public:
        try:
            for path, description in workload_artifact_paths():
                require_file(path, description)
        except ScriptError as error:
            raise ScriptError(
                f"{error}; {', '.join(public)} "
                f"{'reads' if len(public) == 1 else 'read'} the Ubuntu distro "
                "layer and the sandbox scratch template: install them with "
                "`nvx-rs download` or build them with `build-guest --guest "
                "all`, format the template alone with `build-scratch-template` "
                "on Linux, or name other scenarios with --scenario"
            ) from error

    with DurationReport(
        output_dir / REPORT_NAME,
        backend=args.backend,
        guest=descriptor.name,
        kernel="debug" if debug_kernel else "default",
        processors=args.processors,
        scenarios=scenarios,
    ) as report:
        _run_scenarios(
            report,
            args,
            scenarios,
            descriptor=descriptor,
            executable=executable,
            kernel=kernel,
            initrd=initrd,
            output_dir=output_dir,
        )
        report.finish()
        report_time_abi_evidence(
            output_dir,
            backend=args.backend,
            guest=descriptor.name,
            debug_kernel=debug_kernel,
        )
        print(f"Wrote microVM correctness logs to {output_dir}")
    return 0


def _run_scenarios(
    report: DurationReport,
    args: argparse.Namespace,
    scenarios: Collection[str],
    *,
    descriptor: GuestDescriptor,
    executable: Path,
    kernel: Path,
    initrd: Path,
    output_dir: Path,
) -> None:
    """Run the selected scenarios in their fixed order, and time each
    execution in `report`."""
    if "guest-boot" in scenarios:
        report.start(
            "guest-boot",
            f"Running {descriptor.distribution} initramfs boot correctness "
            f"on OpenVMM/{args.backend}",
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
        report.start(
            "guest-identity",
            f"Running {descriptor.distribution} identity correctness "
            f"on OpenVMM/{args.backend}",
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
            report.start(
                "console-exit",
                f"Running microVM console exit correctness ({processors} vCPU) "
                f"on OpenVMM/{args.backend}",
                processors=[processors],
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
        report.start(
            "console-snapshot",
            f"Running microVM console snapshot correctness on OpenVMM/{args.backend}",
        )
        run_console_snapshot(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "run-console-restore" in scenarios:
        report.start(
            "run-console-restore",
            "Running nvx-rs run restore of a terminal boot console on "
            f"OpenVMM/{args.backend}",
        )
        run_run_console_restore(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "cpu-profile-fallback" in scenarios:
        report.start(
            "cpu-profile-fallback",
            "Running microVM CPU profile fallback correctness on "
            f"OpenVMM/{args.backend}",
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
        report.start(
            "endpoint-policy-snapshot",
            "Running microVM endpoint-policy snapshot correctness on "
            f"OpenVMM/{args.backend}",
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
        report.start(
            "filesystem-shares",
            "Running read-write, read-only, and hidden-root microVM shares "
            f"through one aggregate on OpenVMM/{args.backend}",
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
        report.start(
            "filesystem-policy",
            "Running microVM share write narrowing and read masking "
            f"on OpenVMM/{args.backend}",
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
    if "filesystem-comma-paths" in scenarios:
        report.start(
            "filesystem-comma-paths",
            "Running public nvx-rs shares of host paths with commas "
            f"on OpenVMM/{args.backend}",
        )
        run_filesystem_comma_paths(
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
            guest=descriptor.name,
        )
    if "filesystem-snapshot" in scenarios:
        report.start(
            "filesystem-snapshot",
            f"Running microVM filesystem snapshot correctness on OpenVMM/{args.backend}",
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
        report.start(
            "lifecycle",
            f"Running microVM lifecycle correctness on OpenVMM/{args.backend}",
        )
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
        report.start(
            "workload-identity",
            f"Running microVM workload identity correctness on OpenVMM/{args.backend}",
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
        report.start(
            "managed-exec-config",
            f"Running public managed execution configuration on OpenVMM/{args.backend}",
        )
        run_managed_exec_configuration(
            args.backend, timeout=args.timeout, output_dir=output_dir
        )
    if "sandbox-lifecycle" in scenarios:
        report.start(
            "sandbox-lifecycle",
            f"Running public managed sandbox lifecycle on OpenVMM/{args.backend}",
        )
        run_sandbox_lifecycle(args.backend, timeout=args.timeout, output_dir=output_dir)
    if "sandbox-proxy-only" in scenarios:
        report.start(
            "sandbox-proxy-only",
            f"Running public proxy-only sandbox networking on OpenVMM/{args.backend}",
        )
        run_sandbox_proxy_only(
            args.backend, timeout=args.timeout, output_dir=output_dir
        )
    if "sandbox-ram-upper" in scenarios:
        report.start(
            "sandbox-ram-upper",
            f"Running public RAM-upper sandboxes on OpenVMM/{args.backend}",
        )
        run_sandbox_ram_upper(args.backend, timeout=args.timeout, output_dir=output_dir)
    if "managed-lifecycle" in scenarios:
        report.start(
            "managed-lifecycle",
            f"Running managed microVM lifecycle on OpenVMM/{args.backend}",
        )
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
        report.start(
            "structured-outcome",
            f"Running structured microVM outcomes on OpenVMM/{args.backend}",
        )
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
        report.start(
            "l3-l4-egress-policy",
            "Running public nvx-rs L3/L4 egress policy acceptance "
            f"on OpenVMM/{args.backend}",
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
        report.start(
            "l3-l4-egress-protocols",
            "Running public nvx-rs L3/L4 egress protocol acceptance "
            f"on OpenVMM/{args.backend}",
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
        report.start(
            "l3-l4-egress-port-ranges",
            "Running public nvx-rs L3/L4 egress port-range acceptance "
            f"on OpenVMM/{args.backend}",
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
        report.start(
            "ipv6-egress-policy",
            "Running public nvx-rs IPv6 egress policy acceptance "
            f"on OpenVMM/{args.backend}",
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
        report.start(
            "host-loopback-policy",
            f"Running microVM host-loopback policy on OpenVMM/{args.backend}",
        )
        run_host_loopback_policy(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "private-network-ingress" in scenarios:
        report.start(
            "private-network-ingress",
            f"Running microVM private-network ingress on OpenVMM/{args.backend}",
        )
        run_private_network_ingress(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "denied-filesystem-paths" in scenarios:
        report.start(
            "denied-filesystem-paths",
            f"Running microVM denied filesystem paths on OpenVMM/{args.backend}",
        )
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
        report.start(
            "filesystem-owner",
            f"Running microVM filesystem caller ownership on OpenVMM/{args.backend}",
        )
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
        report.start(
            "network-snapshot",
            f"Running microVM network snapshot correctness on OpenVMM/{args.backend}",
        )
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
            report.start(
                scenario,
                f"Running microVM {scenario} correctness ({processors} vCPU) "
                f"on OpenVMM/{args.backend}",
                processors=[processors],
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
        report.start(
            "sandbox-blocks",
            f"Running microVM sandbox-block correctness on OpenVMM/{args.backend}",
        )
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
        report.start(
            "scratch-snapshot",
            f"Running microVM scratch snapshot correctness on OpenVMM/{args.backend}",
        )
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
        report.start(
            "smp-snapshot",
            f"Running microVM SMP snapshot correctness on OpenVMM/{args.backend}",
            processors=args.processors,
        )
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
        report.start(
            "restore-processors",
            f"Running microVM restore-processor correctness on OpenVMM/{args.backend}",
            processors=args.processors,
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
    if "platform-restore-processors" in scenarios:
        report.start(
            "platform-restore-processors",
            "Running microVM platform-snapshot restore-processor correctness on "
            f"OpenVMM/{args.backend}",
            processors=[
                count
                for count in args.processors
                if count <= PLATFORM_RESTORE_PROCESSORS_CAPACITY
            ],
        )
        run_platform_restore_processors(
            executable,
            kernel,
            initrd,
            args.backend,
            args.processors,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "platform-zero-fill" in scenarios:
        report.start(
            "platform-zero-fill",
            "Running microVM platform-snapshot zero-fill correctness on "
            f"OpenVMM/{args.backend}",
        )
        run_platform_zero_fill(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "platform-network-policy" in scenarios:
        report.start(
            "platform-network-policy",
            "Running microVM platform-snapshot network policy replacement on "
            f"OpenVMM/{args.backend}",
        )
        run_platform_network_policy(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "untiered-network-policy" in scenarios:
        report.start(
            "untiered-network-policy",
            "Running microVM untiered-snapshot network policy replacement on "
            f"OpenVMM/{args.backend}",
        )
        run_untiered_network_policy(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "platform-filesystem" in scenarios:
        report.start(
            "platform-filesystem",
            "Running microVM platform-snapshot filesystem attachment correctness "
            f"on OpenVMM/{args.backend}",
        )
        run_platform_filesystem(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "platform-aggregate-export" in scenarios:
        report.start(
            "platform-aggregate-export",
            "Running microVM platform-snapshot aggregate export correctness on "
            f"OpenVMM/{args.backend}",
        )
        run_platform_aggregate_export(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "restore-downtime" in scenarios:
        report.start(
            "restore-downtime",
            "Running microVM long-downtime restore correctness on "
            f"OpenVMM/{args.backend}",
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
        report.start(
            "restore-memory",
            f"Running microVM restore-memory correctness on OpenVMM/{args.backend}",
        )
        run_restore_memory(
            executable,
            kernel,
            initrd,
            args.backend,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "snapshot-core" in scenarios:
        report.start(
            "snapshot-core",
            f"Running microVM snapshot-core correctness on OpenVMM/{args.backend}",
        )
        run_snapshot_core(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "snapshot-rollback" in scenarios:
        report.start(
            "snapshot-rollback",
            f"Running microVM snapshot rollback correctness on OpenVMM/{args.backend}",
        )
        run_snapshot_rollback(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "snapshot-tiers" in scenarios:
        report.start(
            "snapshot-tiers",
            f"Running microVM snapshot-tier correctness on OpenVMM/{args.backend}",
        )
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
        report.start(
            "directional-network-policy",
            f"Running microVM directional network policy on OpenVMM/{args.backend}",
        )
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
        report.start(
            "time-abi-conformance",
            f"Running microVM time ABI conformance ({processors} vCPU) "
            f"on OpenVMM/{args.backend}",
            processors=[processors],
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
    if "time-host-correction" in scenarios:
        report.start(
            "time-host-correction",
            f"Running microVM host clock correction correctness on OpenVMM/{args.backend}",
        )
        run_time_host_correction(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "virtio-net" in scenarios:
        report.start(
            "virtio-net",
            f"Running microVM virtio-net correctness on OpenVMM/{args.backend}",
        )
        run_virtio_net(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            log_path=output_dir / "virtio-net.log",
        )
    if "virtio-features" in scenarios:
        report.start(
            "virtio-features",
            "Running microVM guest-visible virtio feature correctness on "
            f"OpenVMM/{args.backend}",
        )
        run_virtio_features(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            log_path=output_dir / "virtio-features.log",
        )
    if "interrupt-map" in scenarios:
        report.start(
            "interrupt-map",
            f"Running the microVM interrupt map on OpenVMM/{args.backend}",
        )
        run_interrupt_map(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
    if "command-line-limit" in scenarios:
        report.start(
            "command-line-limit",
            "Running a microVM kernel command line at the guest kernel's limit "
            f"on OpenVMM/{args.backend}",
        )
        run_command_line_limit(
            executable,
            kernel,
            initrd,
            args.backend,
            memory_mib=args.memory_mib,
            timeout=args.timeout,
            output_dir=output_dir,
        )
