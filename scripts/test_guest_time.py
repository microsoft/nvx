#!/usr/bin/env python3
"""Spec-derived tests for the guest time component, guest/common/nvx-time.c.

The fixtures are encoded here from doc/design/time-abi.md, independently of
the C decoders, so the tests check the guest against the specification.
"""

from __future__ import annotations

import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SOURCE = REPO_ROOT / "guest" / "common" / "nvx-time.c"

DOWNTIME_UTC = 0x01
MEMORY_TARGET = 0x02
ACK_REQUIRED = 0x04
TEST_HOOKS = 0x08
UTC_NS = 1_790_841_600_123_456_789
ENTROPY = bytes(range(64))
PREVIOUS_ID = "ff" * 16
THIRTY_DAYS_NS = 2_592_000 * 1_000_000_000


def encode_packet(
    *,
    flags: int = 0,
    online: int = 0,
    ranges: tuple[tuple[int, int], ...] = (),
    range_count: int | None = None,
    generation: int = 1,
    rate_deviation: int = 0,
    downtime_ns: int = 0,
    utc_ns: int = UTC_NS,
    magic: bytes = b"OVR",
    version: int = 4,
    reserved: int = 0,
    entropy: bytes = ENTROPY,
) -> bytes:
    """Encode a restore packet v4 (spec: "Restore packet")."""
    count = len(ranges) if range_count is None else range_count
    header = magic + bytes((version, flags, online, count, reserved))
    header += struct.pack("<IiQQ", generation, rate_deviation, downtime_ns, utc_ns)
    assert len(header) == 32
    body = b"".join(struct.pack("<QQ", start, length) for start, length in ranges)
    return header + body + entropy


def encode_sample(
    *,
    version: int = 1,
    flags: int = 0,
    reserved: int = 0,
    generation: int = 0,
    utc_ns: int = UTC_NS,
) -> bytes:
    """Encode a time sample (spec: "Time sample")."""
    return struct.pack("<BBHIQ", version, flags, reserved, generation, utc_ns)


def can_leave_idle_priority() -> bool:
    """Whether this process may switch from SCHED_IDLE back to SCHED_OTHER.

    That needs CAP_SYS_NICE, which the guest has, or an RLIMIT_NICE of at
    least 20; containers and CI runners usually have neither.
    """
    status = Path("/proc/self/status").read_text(encoding="utf-8")
    capabilities = re.search(r"^CapEff:\s*([0-9a-f]+)$", status, re.MULTILINE)
    if capabilities is not None and int(capabilities.group(1), 16) & (1 << 23):
        return True
    limits = Path("/proc/self/limits").read_text(encoding="utf-8")
    nice = re.search(r"^Max nice priority\s+(\S+)", limits, re.MULTILINE)
    return nice is not None and (
        nice.group(1) == "unlimited" or int(nice.group(1)) >= 20
    )


TIMER_LIST_CPU = """\
Tick Device: mode:     1
Per CPU device: {cpu}
Clock Event Device: {device}
 max_delta_ns:   1374389534634
 min_delta_ns:   1000
 mult:           6710886
 shift:          26
 mode:           {state}
 next_event:     1342000000 nsecs
 set_next_event: lapic_next_event
 shutdown:       lapic_timer_shutdown
 periodic:       lapic_timer_set_periodic
 oneshot:        lapic_timer_set_oneshot
 oneshot stopped: lapic_timer_shutdown
 event_handler:  {handler}
 retries:        0

"""

TIMER_LIST_BROADCAST = """\
Tick Device: mode:     1
Broadcast device
Clock Event Device: {device}
tick_broadcast_mask: 00
tick_broadcast_oneshot_mask: 00

"""

CPUINFO_CPU = """\
processor\t: {cpu}
vendor_id\t: GenuineIntel
cpu family\t: 6
model\t\t: 85
cpu MHz\t\t: 2194.843
flags\t\t: fpu vme {flags}
bugs\t\t: spectre_v1

"""

GOOD_FLAGS = (
    "tsc msr pae constant_tsc rdtscp nonstop_tsc tsc_known_freq tsc_reliable "
    "hypervisor arat x2apic"
)


@unittest.skipUnless(sys.platform.startswith("linux"), "nvx-time builds on Linux")
class GuestTimeTests(unittest.TestCase):
    binary: Path
    directory: tempfile.TemporaryDirectory[str]

    @classmethod
    def setUpClass(cls) -> None:
        compiler = shutil.which("cc") or shutil.which("gcc")
        if compiler is None:
            raise unittest.SkipTest("a C compiler is unavailable")
        cls.directory = tempfile.TemporaryDirectory()
        cls.binary = Path(cls.directory.name) / "nvx-time"
        subprocess.run(
            [
                compiler,
                "-O1",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-o",
                str(cls.binary),
                str(SOURCE),
            ],
            check=True,
            capture_output=True,
            timeout=120,
        )

    @classmethod
    def tearDownClass(cls) -> None:
        cls.directory.cleanup()

    def run_test(self, *arguments: str) -> str:
        result = subprocess.run(
            [str(self.binary), "test", *arguments],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def fixture(self, name: str, content: bytes | str) -> str:
        path = Path(self.directory.name) / name
        if isinstance(content, str):
            path.write_text(content, encoding="utf-8")
        else:
            path.write_bytes(content)
        return str(path)

    def packet(self, packet: bytes, recorded: int = 0, previous: str = PREVIOUS_ID):
        return self.run_test(
            "packet", self.fixture("packet.bin", packet), str(recorded), previous
        ).strip()

    def test_packet_v4_golden_vector(self):
        packet = encode_packet(
            flags=DOWNTIME_UTC | ACK_REQUIRED | TEST_HOOKS,
            online=4,
            generation=3,
            rate_deviation=-13_107_200,
            downtime_ns=30_000_000_000,
        )

        self.assertEqual(len(packet), 96)
        self.assertEqual(
            self.packet(packet, recorded=2),
            "ok flags=13 online=4 ranges=0 generation=3 "
            "rate_deviation=-13107200 frequency=13107200 "
            "downtime_ns=30000000000 "
            f"utc_ns={UTC_NS} generation_id={ENTROPY[:16].hex()}",
        )

    def test_musl_build_matches_the_reference_build(self):
        musl = shutil.which("musl-gcc")
        if musl is None:
            self.skipTest("musl-gcc is unavailable")
        binary = Path(self.directory.name) / "nvx-time-musl"
        subprocess.run(
            [musl, "-static", "-Os", "-s", "-Wall", "-Wextra", "-Werror"]
            + ["-o", str(binary), str(SOURCE)],
            check=True,
            capture_output=True,
            timeout=120,
        )
        packet = self.fixture(
            "musl-packet.bin", encode_packet(flags=ACK_REQUIRED, generation=2)
        )
        arguments = ["test", "packet", packet, "1", PREVIOUS_ID]
        result = subprocess.run(
            [str(binary), *arguments],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, self.run_test(*arguments[1:]))
        # musl has no sched_setscheduler(); the daemon must still reach
        # SCHED_IDLE after a restore and leave it after the bound.
        policy = 0 if can_leave_idle_priority() else 5
        for name, expected in (
            ("idle-priority", f"idle=5 normal={policy}\n"),
            ("promote", f"promoted caller={policy} thread={policy}\n"),
        ):
            result = subprocess.run(
                [str(binary), "test", name],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(result.stdout.startswith(expected), result.stdout)
            self.assertTrue(self.run_test(name).startswith(expected))

    def test_packet_memory_ranges_follow_the_header(self):
        packet = encode_packet(
            flags=MEMORY_TARGET,
            ranges=((0x8000_0000, 0x0800_0000), (0x1_0000_0000, 0x1000_0000)),
        )

        self.assertEqual(len(packet), 32 + 2 * 16 + 64)
        self.assertTrue(
            self.packet(packet).endswith(
                " range=2147483648:134217728 range=4294967296:268435456"
            )
        )
        self.assertIn(" ranges=0 ", self.packet(encode_packet(flags=MEMORY_TARGET)))

    def test_packet_rejections_name_the_repair_code(self):
        cases = {
            "magic": encode_packet(magic=b"OVX"),
            "version 3": encode_packet(version=3),
            "reserved flag": encode_packet(flags=0x10),
            "reserved byte": encode_packet(reserved=1),
            "online count": encode_packet(online=3),
            "ranges without target": encode_packet(range_count=1),
            "generation zero": encode_packet(generation=0),
            "rate above": encode_packet(rate_deviation=16_384_001),
            "rate below": encode_packet(rate_deviation=-16_384_001),
            "downtime": encode_packet(downtime_ns=THIRTY_DAYS_NS + 1),
            "utc zero": encode_packet(utc_ns=0),
            "empty range": encode_packet(flags=MEMORY_TARGET, ranges=((0, 0),)),
            "range overflow": encode_packet(
                flags=MEMORY_TARGET, ranges=((2**64 - 4096, 8192),)
            ),
        }
        for name, packet in cases.items():
            with self.subTest(case=name):
                self.assertTrue(
                    self.packet(packet).startswith("error G_REPAIR_PACKET "),
                    name,
                )

    def test_packet_bounds_are_inclusive(self):
        for packet in (
            encode_packet(rate_deviation=16_384_000),
            encode_packet(rate_deviation=-16_384_000),
            encode_packet(downtime_ns=THIRTY_DAYS_NS),
            encode_packet(online=8),
        ):
            with self.subTest(packet=packet[:32].hex()):
                self.assertTrue(self.packet(packet).startswith("ok "))

    def test_packet_generation_must_follow_the_recorded_generation(self):
        self.assertEqual(self.packet(encode_packet(generation=5), recorded=5), "stale")
        self.assertTrue(
            self.packet(encode_packet(generation=7), recorded=5).startswith(
                "error G_REPAIR_GENERATION "
            )
        )
        self.assertTrue(
            self.packet(
                encode_packet(generation=6), recorded=5, previous=ENTROPY[:16].hex()
            ).startswith("error G_REPAIR_GENERATION ")
        )

    def sample(self, sample: bytes, generation: int = 0) -> str:
        return self.run_test(
            "sample", self.fixture("sample.bin", sample), str(generation)
        ).strip()

    def test_time_sample_layout(self):
        self.assertEqual(len(encode_sample()), 16)
        self.assertEqual(
            self.sample(encode_sample(generation=4, flags=TEST_HOOKS), 4),
            f"ok flags=8 generation=4 utc_ns={UTC_NS}",
        )
        for name, sample in {
            "version": encode_sample(version=2),
            "flags": encode_sample(flags=0x01),
            "reserved": encode_sample(reserved=1),
            "utc": encode_sample(utc_ns=0),
            "generation": encode_sample(generation=3),
        }.items():
            with self.subTest(case=name):
                self.assertTrue(self.sample(sample).startswith("error "), name)

    def test_utc_pairing_uses_the_bracket_midpoint(self):
        self.assertEqual(
            self.run_test("pair", "1000", "3000", "5000").strip(),
            "theta_ns=3000 epsilon_ns=1000",
        )
        self.assertEqual(
            self.run_test("pair", "10000", "10400", "9000").strip(),
            "theta_ns=-1200 epsilon_ns=200",
        )

    def test_restore_frequency_compensates_and_clamps(self):
        self.assertEqual(
            self.run_test("frequency", "13107200").strip(),
            "frequency=-13107200 ppb=-200000",
        )
        self.assertEqual(
            self.run_test("frequency", "-40000000").strip(),
            "frequency=32768000 ppb=500000",
        )
        self.assertEqual(
            self.run_test("frequency", "-65536").strip(),
            "frequency=65536 ppb=1000",
        )

    def test_discipline_steps_at_128_ms_and_slews_below(self):
        self.assertEqual(
            self.run_test("correction", "127999999", "10002", "fast").strip(),
            "slew modes=0x203d offset=127999999 status=0x2001 constant=4 "
            "maxerror=128011 esterror=11",
        )
        self.assertEqual(
            self.run_test("correction", "-500", "20", "slow").strip(),
            "slew modes=0x203d offset=-500 status=0x2001 constant=6 "
            "maxerror=1 esterror=1",
        )
        self.assertEqual(
            self.run_test("correction", "128000000", "10", "fast").strip(),
            "step modes=0x2100 sec=0 nsec=128000000",
        )
        self.assertEqual(
            self.run_test("correction", "-1128000000", "10", "slow").strip(),
            "step modes=0x2100 sec=-2 nsec=872000000",
        )

    def test_watcher_rules_match_linux_6_18_messages(self):
        messages = {
            "tsc: Marking TSC unstable due to clocksource watchdog": ("G_TSC_UNSTABLE"),
            "clocksource: timekeeping watchdog on CPU0: Marking clocksource "
            "'tsc-early' as unstable because the skew is too large:": (
                "G_CLOCKSOURCE_UNSTABLE"
            ),
            "clocksource: Switched to clocksource refined-jiffies": (
                "G_CLOCKSOURCE_SWITCH"
            ),
            "clocksource:         CPUs 1 ahead of CPU 0 for clocksource tsc.": (
                "G_CLOCKSOURCE_SKEW"
            ),
            "clocksource:         CPUs 2-3 behind CPU 0 for clocksource tsc.": (
                "G_CLOCKSOURCE_SKEW"
            ),
            "TSC synchronization [CPU#0 -> CPU#1]:": "G_TSC_WARP",
            "Measured 45 cycles TSC warp between CPUs, turning off TSC clock.": (
                "G_TSC_WARP"
            ),
            "TSC warped randomly between CPUs": "G_TSC_WARP",
            "[Firmware Bug]: TSC ADJUST differs: CPU1 0 --> 100. Restoring": (
                "G_TSC_ADJUST"
            ),
            "rcu: INFO: rcu_preempt detected stalls on CPUs/tasks:": "G_RCU_STALL",
            "rcu: INFO: rcu_preempt self-detected stall on CPU": "G_RCU_STALL",
            "rcu: INFO: rcu_preempt detected expedited stalls on CPUs/tasks: "
            "{ 1-... }": "G_RCU_STALL",
            "rcu: rcu_preempt kthread starved for 2603 jiffies! g345 f0x0": (
                "G_RCU_STARVED"
            ),
            "rcu: rcu_preempt kthread timer wakeup didn't happen for 2602 "
            "jiffies! g345 f0x0": "G_RCU_STARVED",
            "watchdog: BUG: soft lockup - CPU#0 stuck for 22s! [sh:123]": (
                "G_SOFT_LOCKUP"
            ),
            "watchdog: Watchdog detected hard LOCKUP on cpu 1": "G_HARD_LOCKUP",
            "INFO: task kworker/0:1:12 blocked for more than 120 seconds.": (
                "G_HUNG_TASK"
            ),
            "unchecked MSR access error: RDMSR from 0x1ad at rIP: "
            "0xffffffff816f11ee (__rdmsr_on_cpu+0x1e/0x30)": "G_UNCHECKED_MSR",
            "[Firmware Bug]: CPU   1: APIC ID mismatch. CPUID: 0x0000 "
            "APIC: 0x0001": "G_APIC_ID_MISMATCH",
            "clocksource: tsc: mask: 0xffffffffffffffff max_cycles: 0x1fa3": "none",
            "Hyper-V: LAPIC Timer Frequency: 0x1e8480": "none",
            "rcu: Hierarchical RCU implementation.": "none",
            'NVX-TIME-ABI-VIOLATION: v=1 code=G_UNCHECKED_MSR detail="unchecked '
            'MSR access error"': "none",
        }
        output = self.run_test(
            "kmsg", self.fixture("kmsg.txt", "\n".join(messages) + "\n")
        )
        self.assertEqual(output.splitlines(), list(messages.values()))

    def boot_log(self, lines: list[str]) -> str:
        return self.run_test(
            "boot-log", self.fixture("boot.txt", "\n".join(lines) + "\n")
        ).strip()

    BOOT_LOG = [
        "Hypervisor detected: Microsoft Hyper-V",
        "Hyper-V: privilege flags low 0x8860, high 0x0, ext 0x0, hints 0x0, misc 0x100",
        "Hyper-V: LAPIC Timer Frequency: 0x1e8480",
        "tsc: Detected 2194.843 MHz processor",
        "clocksource: Switched to clocksource tsc-early",
        "clocksource: Switched to clocksource tsc",
    ]

    def test_boot_log_requires_the_identity_and_a_final_switch_to_tsc(self):
        self.assertEqual(self.boot_log(self.BOOT_LOG), "ok")
        kvm_rate = [line.replace("0x1e8480", "0x989680") for line in self.BOOT_LOG]
        self.assertEqual(self.boot_log(kvm_rate), "ok")
        failures = {
            "missing identity": self.BOOT_LOG[1:],
            "wrong LAPIC rate": [
                line.replace("0x1e8480", "0x1e8481") for line in self.BOOT_LOG
            ],
            "last switch": [
                *self.BOOT_LOG,
                "clocksource: Switched to clocksource refined-jiffies",
            ],
            "kvm-clock": [*self.BOOT_LOG, "kvm-clock: Using msrs 4b564d01"],
            "refinement": [
                *self.BOOT_LOG,
                "tsc: Refined TSC clocksource calibration: 2194.843 MHz",
            ],
            "lapic token": [
                *self.BOOT_LOG,
                "APIC timer: using supplied frequency 200000000 Hz",
            ],
            "watcher record": [
                *self.BOOT_LOG,
                "tsc: Marking TSC unstable due to clocksource watchdog",
            ],
        }
        for name, lines in failures.items():
            with self.subTest(case=name):
                self.assertTrue(self.boot_log(lines).startswith("fail "), name)

    def test_boot_log_reads_syslog_read_all_lines(self):
        # SYSLOG_ACTION_READ_ALL prefixes every line with "<level>" and, with
        # printk timestamps, "[seconds.micros] ".
        lines = [
            f"<6>[{index:5d}.{index * 7919 % 1000000:06d}] {line}"
            for index, line in enumerate(self.BOOT_LOG)
        ]
        lines.insert(1, "<6>[    0.123456] [drm] a bracketed message is kept")
        own_event = (
            "<2>[    3.000000] NVX-TIME-REPORT-VIOLATION: v=1 code=G_CONFORMANCE_C4 "
            'detail="forbidden record: kvm-clock: Using msrs"'
        )

        def syslog(text_lines: list[str]) -> str:
            return self.run_test(
                "syslog", self.fixture("syslog.txt", "\n".join(text_lines) + "\n")
            ).strip()

        self.assertEqual(syslog(lines), "ok")
        self.assertEqual(syslog([*lines, own_event]), "ok")
        self.assertTrue(
            syslog([*lines, "<6>[    4.000000] kvm-clock: Using msrs"]).startswith(
                "fail forbidden record: kvm-clock"
            )
        )
        self.assertTrue(syslog(lines[2:]).startswith("fail no 'Hypervisor"))

    def test_boot_log_reports_a_kernel_wx_mapping_apart_from_c4(self):
        # Linux 6.18's CONFIG_DEBUG_WX audit, as logged when the ITS
        # mitigation's thunk pages are left writable and executable.
        warning = [
            "<4>[    0.412345] ------------[ cut here ]------------",
            "<4>[    0.412346] x86/mm: Found insecure W+X mapping at address "
            "0xffffffffc0000000",
            "<4>[    0.412400] WARNING: CPU: 0 PID: 1 at "
            "arch/x86/mm/dump_pagetables.c:246 note_wx+0x5a/0x70",
            "<6>[    0.412500] x86/mm: Checked W+X mappings: FAILED, 4 W+X pages "
            "found.",
        ]
        lines = [f"<6>[    0.100000] {line}" for line in self.BOOT_LOG]

        def syslog(text_lines: list[str]) -> list[str]:
            return (
                self.run_test(
                    "syslog",
                    self.fixture("syslog.txt", "\n".join(text_lines) + "\n"),
                )
                .strip()
                .splitlines()
            )

        self.assertEqual(
            syslog([*lines[:2], *warning, *lines[2:]]),
            [
                "ok",
                "wx x86/mm: Found insecure W+X mapping at address 0xffffffffc0000000",
            ],
        )
        self.assertEqual(
            syslog(
                [
                    *lines,
                    "<6>[    0.412500] x86/mm: Checked W+X mappings: passed, "
                    "no W+X pages found.",
                ]
            ),
            ["ok"],
        )
        failed = syslog([*warning, *lines[1:]])
        self.assertEqual(len(failed), 2)
        self.assertTrue(failed[0].startswith("fail no 'Hypervisor"))
        self.assertTrue(failed[1].startswith("wx x86/mm: Found insecure W+X"))
        own_event = (
            "<2>[    3.000000] NVX-TIME-REPORT-VIOLATION: v=1 code=G_KERNEL_WX "
            'detail="x86/mm: Found insecure W+X mapping at address 0x0"'
        )
        self.assertEqual(syslog([*lines, own_event]), ["ok"])

    def test_daemon_lines_start_on_a_fresh_console_line(self):
        # The daemon prints violation events while a shell may be in the
        # middle of a line (its prompt, for example).
        line = "NVX-TIME-ABI-VIOLATION: v=1 code=G_RCU_STALL source=watcher\n"
        self.assertEqual(self.run_test("console", line, "sync"), line)
        self.assertEqual(self.run_test("console", line, "async"), "\n" + line)

    def test_exhaustive_leaf_rules(self):
        # X1 and X2: a leaf reads zero or the highest basic leaf's result,
        # except the explicit zero leaves, which read zero.
        out_of_range = ("0x00000d0b", "0x00000001", "0x00000002", "0x00000003")
        zero = ("0", "0", "0", "0")

        def leaf(number: str, value: tuple[str, ...]) -> str:
            return self.run_test(
                "exhaustive-leaf", number, *value, *out_of_range
            ).strip()

        self.assertEqual(leaf("0x40000006", zero), "pass")
        self.assertEqual(leaf("0x40000010", out_of_range), "pass")
        self.assertEqual(leaf("0x40000100", zero), "pass")
        self.assertEqual(leaf("0x40000100", out_of_range), "pass")
        explicit = leaf("0x40000081", out_of_range)
        self.assertTrue(explicit.startswith("fail leaf 0x40000081 is "), explicit)
        self.assertTrue(explicit.endswith("expected zero"), explicit)
        # KVM's signature leaf moved to a higher base.
        kvm = ("0x40000101", "0x4b4d564b", "0x564b4d56", "0x0000004d")
        signature = leaf("0x40000100", kvm)
        self.assertIn('(signature "KVMKVMKVM...")', signature)
        self.assertTrue(signature.endswith("or the highest basic leaf"), signature)

    def test_exhaustive_report_lines_follow_the_spec(self):
        # The spec's exhaustive check prints one line per check and CPU, with
        # an escaped detail, then a summary.
        line = re.compile(
            r"^NVX-TIME-ABI-EXHAUSTIVE: v=1 check=X[1-6] cpu=\d+ "
            r'status=(pass|fail) detail="(?:[^"\\]|\\.)*"$'
        )
        passed = self.run_test(
            "exhaustive-report", "X3", "2", "pass", "ok"
        ).splitlines()
        self.assertEqual(
            passed,
            [
                'NVX-TIME-ABI-EXHAUSTIVE: v=1 check=X3 cpu=2 status=pass detail="ok"',
                "failures=0",
            ],
        )
        failed = self.run_test(
            "exhaustive-report",
            "X2",
            "7",
            "fail",
            'base 0x40000100 "KVMKVMKVM" \\ x\ty',
        ).splitlines()
        self.assertRegex(failed[0], line)
        self.assertIn(r'detail="base 0x40000100 \"KVMKVMKVM\" \\ x\x09y"', failed[0])
        self.assertEqual(failed[1], "failures=1")
        self.assertEqual(
            self.run_test("exhaustive-summary", "4", "0").splitlines(),
            ["NVX-TIME-ABI-EXHAUSTIVE: v=1 status=ok cpus=4 failures=0", "exit=0"],
        )
        self.assertEqual(
            self.run_test("exhaustive-summary", "8", "3").splitlines(),
            ["NVX-TIME-ABI-EXHAUSTIVE: v=1 status=fail cpus=8 failures=3", "exit=1"],
        )

    def timer_list(
        self,
        sections: list[tuple[int, str, int, str, int]],
        broadcast: str = "<NULL>",
        cpus: str = "0-1",
    ) -> str:
        text = TIMER_LIST_BROADCAST.format(device=broadcast)
        for cpu, device, state, handler, mode in sections:
            text += TIMER_LIST_CPU.format(
                cpu=cpu, device=device, state=state, handler=handler
            ).replace("Tick Device: mode:     1", f"Tick Device: mode:     {mode}")
        return self.run_test(
            "timer-list", self.fixture("timer_list.txt", text), cpus
        ).strip()

    def test_timer_list_requires_one_shot_lapic_ticks(self):
        good = [
            (0, "lapic", 3, "hrtimer_interrupt", 1),
            (1, "lapic", 4, "hrtimer_interrupt", 1),
        ]
        self.assertEqual(self.timer_list(good), "ok")
        failures = {
            "deadline": [good[0], (1, "lapic-deadline", 3, "hrtimer_interrupt", 1)],
            "periodic tick": [good[0], (1, "lapic", 3, "hrtimer_interrupt", 0)],
            "periodic state": [good[0], (1, "lapic", 2, "hrtimer_interrupt", 1)],
            "handler": [good[0], (1, "lapic", 3, "tick_handle_periodic", 1)],
            "missing cpu": [good[0]],
        }
        for name, sections in failures.items():
            with self.subTest(case=name):
                self.assertTrue(self.timer_list(sections).startswith("fail "))
        self.assertTrue(self.timer_list(good, broadcast="pit").startswith("fail "))

    def test_cpuinfo_flags_and_rate(self):
        def cpuinfo(flags: str, cpus: int = 2) -> str:
            text = "".join(
                CPUINFO_CPU.format(cpu=cpu, flags=flags) for cpu in range(cpus)
            )
            return self.run_test(
                "cpuinfo", self.fixture("cpuinfo.txt", text), "0-1"
            ).strip()

        self.assertEqual(cpuinfo(GOOD_FLAGS), "ok cpu0_khz=2194843 cpu1_khz=2194843")
        for name, flags in {
            "tsc_reliable": GOOD_FLAGS.replace("tsc_reliable ", ""),
            "deadline": GOOD_FLAGS + " tsc_deadline_timer",
            "adjust": GOOD_FLAGS + " tsc_adjust",
        }.items():
            with self.subTest(case=name):
                self.assertIn("code=G_CONFORMANCE_C5", cpuinfo(flags))
        self.assertIn("code=G_CONFORMANCE_C5", cpuinfo(GOOD_FLAGS, cpus=1))

    def test_command_line_rejects_clock_workarounds(self):
        accepted = "earlycon=xe9 console=hvc0 quiet clocksource=tsc tsc=reliable"
        self.assertEqual(self.run_test("cmdline", accepted).strip(), "ok")
        for token in (
            "tsc_early_khz=2194843",
            "lapic_timer_hz=200000000",
            "notsc",
            "nolapic",
            "nolapic_timer",
            "tsc=unstable",
            "hpet=force",
            "clocksource=kvm-clock",
        ):
            with self.subTest(token=token):
                self.assertIn(
                    "code=G_CONFORMANCE_C9",
                    self.run_test("cmdline", f"{accepted} {token}"),
                )

    def test_clocksource_must_be_tsc_with_only_jiffies_fallbacks(self):
        self.assertEqual(self.run_test("clocksource", "tsc\n", "tsc \n").strip(), "ok")
        self.assertEqual(
            self.run_test(
                "clocksource", "tsc\n", "tsc refined-jiffies jiffies\n"
            ).strip(),
            "ok",
        )
        for current, available in (
            ("tsc-early\n", "tsc-early \n"),
            ("tsc\n", "tsc kvm-clock \n"),
            ("refined-jiffies\n", "refined-jiffies \n"),
        ):
            with self.subTest(current=current, available=available):
                self.assertIn(
                    "code=G_CONFORMANCE_C6",
                    self.run_test("clocksource", current, available),
                )

    def test_violation_event_format_escaping_and_limit(self):
        line = self.run_test(
            "event",
            "G_UNCHECKED_MSR",
            "watcher",
            "runtime",
            "3",
            "123456789",
            'unchecked "MSR"\\ access\x01',
        )
        self.assertEqual(
            line,
            "NVX-TIME-ABI-VIOLATION: v=1 code=G_UNCHECKED_MSR source=watcher "
            "phase=runtime generation=3 boottime_ns=123456789 "
            'detail="unchecked \\"MSR\\"\\\\ access\\x01"\n',
        )
        long_line = self.run_test(
            "event", "G_HUNG_TASK", "watcher", "runtime", "0", "1", "\x02" * 400
        )
        self.assertLessEqual(len(long_line), 512)
        self.assertTrue(long_line.endswith('\\x02"\n'))

    def test_state_file_round_trip(self):
        # The keys and their order follow the spec's state file table.
        text = (
            "version=1\ngeneration=2\nboot_status=ok\ncapture_status=ok\n"
            "restore_status=pending\nboot_cpus=8\ncapture_cpus=8\nrestore_cpus=0\n"
            "boot_elapsed_us=2400\ncapture_elapsed_us=310\nrestore_elapsed_us=1830\n"
            "boot_cpu_us=1900\ncapture_cpu_us=290\nrestore_cpu_us=640\n"
            "capture_generation=1\nrestore_generation=2\ntsc_hz=2194843000\n"
            "lapic_hz=200000000\ndiscontinuities=3\nlast_discontinuity=step\n"
            "last_step_ns=-200000000\nlast_step_realtime_ns=1790841600000000000\n"
            "last_downtime_ns=31000000000\nlast_downtime_source=utc\n"
            "synchronized=1\noffset_ns=-42\nuncertainty_ns=1800\n"
            "frequency_ppb=-1200\nsamples=17\nrejected_samples=1\n"
            "last_sample_error=none\nviolations=0\n"
        )
        self.assertEqual(self.run_test("state", self.fixture("state.txt", text)), text)
        # Report-only mode adds the failure counts, reserved for it, after the
        # generations, and a failed check's status is fail.
        report = text.replace("restore_status=pending", "restore_status=fail").replace(
            "restore_generation=2\n",
            "restore_generation=2\nboot_failures=0\ncapture_failures=0\n"
            "restore_failures=3\n",
        )
        self.assertEqual(
            self.run_test("state", self.fixture("state.txt", report)), report
        )
        # A fresh state is the pending boot check; the capture and restore keys
        # are absent until those checks first run.
        self.assertTrue(
            self.run_test("state", self.fixture("state.txt", "version=1\n")).startswith(
                "version=1\ngeneration=0\nboot_status=pending\nboot_cpus=0\n"
                "boot_elapsed_us=0\nboot_cpu_us=0\ntsc_hz=0\nlapic_hz=0\n"
            )
        )
        self.assertEqual(
            self.run_test(
                "state", self.fixture("state.txt", "unknown=x\nsamples=bad\n")
            ).strip(),
            "error",
        )

    def status(self, text: str, mode: str = "enforcing") -> list[str]:
        return self.run_test(
            "status", self.fixture("state.txt", text), mode
        ).splitlines()

    def test_status_prints_every_recorded_check_and_the_runtime_line(self):
        state = (
            "version=1\ngeneration=2\nboot_status=ok\ncapture_status=ok\n"
            "restore_status=ok\nboot_cpus=8\ncapture_cpus=8\nrestore_cpus=4\n"
            "boot_elapsed_us=2400\ncapture_elapsed_us=310\nrestore_elapsed_us=1830\n"
            "boot_cpu_us=1900\ncapture_cpu_us=290\nrestore_cpu_us=640\n"
            "capture_generation=1\nrestore_generation=2\ntsc_hz=2194843000\n"
            "lapic_hz=200000000\ndiscontinuities=2\nsynchronized=1\n"
            "offset_ns=-42\nuncertainty_ns=1800\nrejected_samples=2\n"
            "last_sample_error=G_SAMPLE_UNCERTAIN\n"
        )
        rates = "tsc_hz=2194843000 lapic_hz=200000000"
        # One line per recorded phase, each with the generation its check ran
        # in and its wall and CPU times, then the runtime line; the hook adds
        # whether status exits 0.
        self.assertEqual(
            self.status(state),
            [
                f"NVX-TIME-ABI: v=1 phase=boot status=ok cpus=8 {rates} "
                "generation=0 elapsed_us=2400 cpu_us=1900",
                f"NVX-TIME-ABI: v=1 phase=capture status=ok cpus=8 {rates} "
                "generation=1 elapsed_us=310 cpu_us=290",
                f"NVX-TIME-ABI: v=1 phase=restore status=ok cpus=4 {rates} "
                "generation=2 elapsed_us=1830 cpu_us=640",
                "NVX-TIME-ABI: v=1 phase=runtime status=synchronized generation=2 "
                "discontinuities=2 offset_ns=-42 uncertainty_ns=1800 "
                "rejected_samples=2 last_sample_error=G_SAMPLE_UNCERTAIN",
                "ok=1",
            ],
        )
        # A check still pending after the wait has neither elapsed_us nor
        # cpu_us and fails the exit status. A cold boot has a boot line only,
        # and a state without a boot record counts as a pending boot check.
        pending = state.replace("restore_status=ok", "restore_status=pending")
        self.assertEqual(
            self.status(pending)[2::2],
            [
                f"NVX-TIME-ABI: v=1 phase=restore status=pending cpus=4 {rates} "
                "generation=2",
                "ok=0",
            ],
        )
        boot_pending = (
            "NVX-TIME-ABI: v=1 phase=boot status=pending cpus=0 tsc_hz=0 "
            "lapic_hz=0 generation=0"
        )
        for text in ("version=1\ngeneration=0\nboot_status=pending\n", "version=1\n"):
            lines = self.status(text)
            self.assertEqual(lines[0], boot_pending, text)
            self.assertTrue(lines[1].startswith("NVX-TIME-ABI: v=1 phase=runtime "))
            self.assertEqual(lines[2:], ["ok=0"], text)
        # Report-only mode marks every line and appends each check's failure
        # count from the state file, where a failed check's status is fail.
        report = self.status(
            state.replace("restore_status=ok", "restore_status=fail").replace(
                "restore_generation=2\n",
                "restore_generation=2\nboot_failures=0\nrestore_failures=3\n",
            ),
            "report-only",
        )
        self.assertEqual(
            report[0],
            f"NVX-TIME-REPORT: v=1 phase=boot status=ok cpus=8 {rates} "
            "generation=0 elapsed_us=2400 cpu_us=1900 failures=0",
        )
        self.assertEqual(
            report[2],
            f"NVX-TIME-REPORT: v=1 phase=restore status=fail cpus=4 {rates} "
            "generation=2 elapsed_us=1830 cpu_us=640 failures=3",
        )
        self.assertTrue(report[3].startswith("NVX-TIME-REPORT: v=1 phase=runtime "))
        self.assertEqual(report[4], "ok=0")

    def test_deferred_work_starts_150_ms_late_and_is_promoted_100_ms_later(self):
        # Step 13 and the asynchronous boot checks start 150 ms after the
        # acknowledgement or the boot step and leave SCHED_IDLE 100 ms after
        # they start.
        self.assertEqual(
            self.run_test("deferral"), "deferred_start_ms=150 idle_bound_ms=100\n"
        )

    def test_restore_cpu_time_is_step_13_with_the_daemons_fork(self):
        # The worker's step 13 CPU time adds the daemon's preparation before
        # the fork, the fork's cost to the daemon, which arrives on a pipe
        # after the worker's copy of the timing was taken, and its own.
        fields = dict(
            field.split("=")
            for field in self.run_test("step13-cpu", "20", "30", "10").split()
        )
        self.assertGreaterEqual(int(fields["prior_ms"]), 20)
        self.assertLess(int(fields["prior_ms"]), 30)
        self.assertGreaterEqual(int(fields["total_ms"]), 60)
        self.assertLess(int(fields["total_ms"]), 90)

    def restore_record(self, text: str) -> str:
        return self.run_test(
            "restore-record", self.fixture("restore.txt", text)
        ).strip()

    def test_restore_record_names_the_cpus_that_restore_finish_onlined(self):
        # restore-finish records the CPUs that activation brought online.
        self.assertEqual(
            self.restore_record("generation=3\nnew_cpus=4-6,9\n"),
            "generation=3 new_cpus=4,5,6,9 count=4",
        )
        self.assertEqual(
            self.restore_record("generation=3\nnew_cpus=none\n"),
            "generation=3 new_cpus=none count=0",
        )
        self.assertEqual(
            self.restore_record("generation=3\n"), "generation=3 new_cpus=none count=0"
        )
        for text in (
            "",
            "new_cpus=4\ngeneration=3\n",
            "generation=3\nnew_cpus=7-4\n",
            "generation=3\nnew_cpus=64\n",
        ):
            self.assertEqual(self.restore_record(text), "error", text)

    def test_parallel_checks_merge_every_failure(self):
        if sys.platform != "linux":
            self.skipTest("CPU affinity is Linux-only")
        allowed: list[int] = sorted(os.sched_getaffinity(0))
        # Three side jobs fail 1, 2, and 3 times and the foreground once,
        # whether one CPU runs them in the caller or each has a thread.
        for size in sorted({1, min(2, len(allowed)), min(4, len(allowed))}):
            output = self.run_test("parallel", ",".join(map(str, allowed[:size])))
            self.assertTrue(output.endswith("failures=7\n"), output)
            self.assertEqual(output.count("code=G_CONFORMANCE_C4"), 6, output)
            self.assertEqual(output.count("code=G_CONFORMANCE_C9"), 1, output)
        # A CPU the checks cannot run on fails C1 in its job.
        unusable = [cpu for cpu in range(64) if cpu not in allowed]
        if not unusable:
            self.skipTest("every CPU is usable")
        output = self.run_test("parallel", f"{allowed[0]},{unusable[0]}")
        self.assertTrue(output.endswith("failures=8\n"), output)
        self.assertIn(f"cannot run on cpu {unusable[0]}", output)

    def test_idle_work_continues_at_normal_priority_after_the_bound(self):
        # The asynchronous checks leave SCHED_IDLE (5) for SCHED_OTHER (0)
        # 100 ms after they start, every thread included; checks that end
        # first stop the promoter without waiting for the bound.
        policy = 0 if can_leave_idle_priority() else 5
        promoted, stopped = self.run_test("promote").splitlines()
        self.assertEqual(promoted, f"promoted caller={policy} thread={policy}")
        fields = dict(field.split("=") for field in stopped.split()[1:])
        self.assertEqual(fields["caller"], "5")
        self.assertLess(int(fields["stop_ms"]), 1000)


if __name__ == "__main__":
    unittest.main()
