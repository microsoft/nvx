"""Harness checks of the NVX time ABI v1 guest obligations.

doc/design/time-abi.md defines what a conforming guest prints and how it fails:
the ``NVX-TIME-ABI`` marker of each conformance check, the
``NVX-TIME-ABI-VIOLATION`` event, and the power-off statuses 193 (conformance),
194 (runtime violation), and 195 (restore repair). It also fixes the warp
probe's 1 us skew bound and the CPU generation names used in reports.
"""

from __future__ import annotations

import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

MARKER_PREFIX = "NVX-TIME-ABI: "
VIOLATION_PREFIX = "NVX-TIME-ABI-VIOLATION: "
REPORT_ONLY_PREFIX = "NVX-TIME-REPORT"
# OpenVMM's exit message leads with the time ABI code that its error chain
# carries: "fatal error: [E_CODE] <outermost context>".
OPENVMM_FATAL_PREFIX = "fatal error: "
OPENVMM_FATAL_CODE_PREFIX = f"{OPENVMM_FATAL_PREFIX}[E_"
ABI_VERSION = "1"
STATUS_CLASSES: Mapping[int, str] = {
    193: "conformance",
    194: "runtime violation",
    195: "restore repair",
}
LAPIC_HZ: Mapping[str, int] = {
    "kvm": 1_000_000_000,
    "mshv": 200_000_000,
    "whp": 200_000_000,
}
MIN_TSC_HZ = 500_000_000
MAX_TSC_HZ = 10_000_000_000
# doc/design/time-abi.md, "Performance expectations and acceptance gate",
# "cpu_us budgets": each backend has a CPU-time (cpu_us) budget per phase
# (boot, capture, and restore): a base plus an increment per additional online
# CPU, in microseconds, held per sample. These are the spec's final values, each
# at least 1.2 times the largest sample from all fleet validation (bare metal
# and Azure, production and debug kernels). Triaged outliers don't count, and
# neither does the first capture after a rolled-back capture, which the spec
# exempts and CI never makes. This table is the one place the harness reads
# them. The checks' wall time (elapsed_us) has no budget. CI reports both and
# gates on neither: the performance gate is the A/B comparison outside CI.
CHECK_CPU_BUDGET_US: Mapping[str, Mapping[str, tuple[int, int]]] = {
    "kvm": {"boot": (6_000, 2_000), "capture": (1_500, 400), "restore": (7_000, 2_000)},
    "mshv": {"boot": (3_000, 1_000), "capture": (1_000, 400), "restore": (2_500, 500)},
    "whp": {
        "boot": (25_000, 1_500),
        "capture": (1_200, 400),
        "restore": (35_000, 6_000),
    },
}
WARP_BOUND_NS = 1000
WARP_PROBE_PATH = "/sbin/nvx-time-probe"
WARP_SUMMARY_PREFIX = "NVX-TIME-PROBE warp "
WARP_DETAIL_PREFIX = "NVX-TIME-PROBE warp-detail "
WARP_PROBE_COMPLETION_MARKER = b"NVX-WARP-PROBE-OK"
WARP_PROBE_FAILURE_MARKER = b"NVX-WARP-PROBE-FAIL"
WARP_PROBE_FAILURE_STATUS = 97
# The idle-inducing warp schedule: probe rounds separated by halted vCPUs.
WARP_PROBE_ROUNDS = 2
WARP_PROBE_IDLE_SECONDS = 1
# Idle gaps between probe rounds, in seconds (doc/design/time-abi.md, "Warp
# schedules"): CI's schedule after boot and every restore, and H6's.
CI_WARP_GAPS: tuple[str, ...] = (str(WARP_PROBE_IDLE_SECONDS),) * (
    WARP_PROBE_ROUNDS - 1
)
QUALIFICATION_WARP_GAPS: tuple[str, ...] = ("0.1", "1", "5", "1")
PROFILE_VENDORS: Mapping[str, str] = {"GenuineIntel": "intel"}


@dataclass(frozen=True)
class CpuGeneration:
    """One CPU generation of the spec's CPU profile catalog."""

    name: str
    vendor: str
    family: int
    model: int
    steppings: range

    @property
    def profile_id(self) -> str:
        """The catalog's v1 profile, which every backend shares."""
        return f"{PROFILE_VENDORS[self.vendor]}.{self.name}.v1"


# Model 85 also covers Cascade Lake (steppings 5-7) and Cooper Lake (10-11),
# which have no profile.
CPU_GENERATIONS: tuple[CpuGeneration, ...] = (
    CpuGeneration("skylake-sp", "GenuineIntel", 6, 85, range(5)),
    CpuGeneration("icelake-sp", "GenuineIntel", 6, 106, range(16)),
    CpuGeneration("emeraldrapids", "GenuineIntel", 6, 207, range(16)),
)
# Guest boot markers that init prints once the guest is shell-ready. Only the
# time ABI's initial clock step (C12) precedes them; the other boot checks
# finish asynchronously.
GUEST_BOOT_MARKERS = ("ALPINE-MICROVM-BOOT-OK", "NVX-GUEST-BOOT-OK:")
# Guests keep the console quiet, because every console byte is a port exit:
# they print one line per recorded check phase (boot, capture, restore, oldest
# first) and a runtime line only when nvx-time status asks. Test runners ask
# once after a cold boot, before any other input, and after each restore;
# benchmarks never ask, so measured intervals stay quiet.
STATUS_COMMAND = "/sbin/nvx-time status"
# nvx-time status waits this long for pending checks, then reports
# status=pending and exits 1.
STATUS_WAIT_SECONDS = 30
STATUS_EXIT_PREFIX = "NVX-TIME-STATUS-EXIT status="
# The token ends its line even when the query's last output lacked a newline;
# the shell's echo of the query line ends in "$?" instead of a status.
_STATUS_EXIT = re.compile(rf"{re.escape(STATUS_EXIT_PREFIX)}(\d+)$")
# The exit status of an nvx-time that has no status subcommand.
STATUS_USAGE_EXIT = 2
# Leaves the guest time to report a check that is still pending.
STATUS_TIMEOUT_SECONDS = STATUS_WAIT_SECONDS + 30.0
_MAX_PENDING_LINE = 64 * 1024


class TimeAbiFailure(RuntimeError):
    """Raised when a guest or its console output violates the time ABI."""


def cpu_generation(
    vendor: str, family: int, model: int, stepping: int
) -> CpuGeneration | None:
    """Return the time ABI generation of a CPU, or None if it has none."""
    for generation in CPU_GENERATIONS:
        if (
            generation.vendor == vendor
            and generation.family == family
            and generation.model == model
            and stepping in generation.steppings
        ):
            return generation
    return None


def parse_fields(text: str) -> dict[str, str]:
    """Parse space-separated ``key=value`` fields; values may be quoted.

    Quoted values use the guest's escapes: ``\\"``, ``\\\\``, and ``\\xNN``.
    """
    fields: dict[str, str] = {}
    index = 0
    length = len(text)
    while index < length:
        if text[index] == " ":
            index += 1
            continue
        separator = text.find("=", index)
        if separator < 0:
            raise ValueError(f"field without a value: {text[index:]!r}")
        key = text[index:separator]
        if not key or " " in key or '"' in key:
            raise ValueError(f"invalid field name {key!r}")
        index = separator + 1
        if index < length and text[index] == '"':
            index += 1
            value: list[str] = []
            while True:
                if index >= length:
                    raise ValueError(f"unterminated quoted value for {key!r}")
                character = text[index]
                if character == '"':
                    index += 1
                    break
                if character != "\\":
                    value.append(character)
                    index += 1
                    continue
                escape = text[index + 1 : index + 2]
                if escape in ('"', "\\"):
                    value.append(escape)
                    index += 2
                elif escape == "x":
                    digits = text[index + 2 : index + 4]
                    if len(digits) != 2:
                        raise ValueError(f"truncated escape in {key!r}")
                    value.append(chr(int(digits, 16)))
                    index += 4
                else:
                    raise ValueError(f"invalid escape in {key!r}")
            parsed = "".join(value)
        else:
            end = text.find(" ", index)
            if end < 0:
                end = length
            parsed = text[index:end]
            index = end
        if key in fields:
            raise ValueError(f"duplicate field {key!r}")
        fields[key] = parsed
    return fields


def _clean_line(line: str) -> str:
    return line.removesuffix("\r").removesuffix("\n").removesuffix("\r")


def parse_marker(line: str) -> dict[str, str] | None:
    """Parse an ``NVX-TIME-ABI:`` marker line, or return None for other lines."""
    line = _clean_line(line)
    if not line.startswith(MARKER_PREFIX):
        return None
    fields = parse_fields(line.removeprefix(MARKER_PREFIX))
    for name in ("v", "phase", "status"):
        if name not in fields:
            raise ValueError(f"time ABI marker lacks {name!r}: {line!r}")
    return fields


def parse_violation(line: str) -> dict[str, str] | None:
    """Parse an ``NVX-TIME-ABI-VIOLATION:`` event anywhere in a console line."""
    line = _clean_line(line)
    start = line.find(VIOLATION_PREFIX)
    if start < 0:
        return None
    fields = parse_fields(line[start + len(VIOLATION_PREFIX) :])
    if "code" not in fields:
        raise ValueError(f"time ABI violation lacks a code: {line!r}")
    return fields


def check_cpu_budget_us(backend: str, phase: str, cpus: int) -> int | None:
    """Return the CPU-time budget of one time ABI ``phase`` check at ``cpus`` CPUs."""
    budget = CHECK_CPU_BUDGET_US.get(backend, {}).get(phase)
    if budget is None or cpus < 1:
        return None
    base, per_cpu = budget
    return base + per_cpu * (cpus - 1)


def describe_exit_status(returncode: int | None) -> str | None:
    """Describe an OpenVMM exit status that a time ABI guest failure produced."""
    if returncode is None or returncode not in STATUS_CLASSES:
        return None
    return f"guest powered off with time ABI status {returncode} ({STATUS_CLASSES[returncode]})"


def _command_option(command: Sequence[str], option: str) -> str | None:
    for index, argument in enumerate(command[:-1]):
        if argument == option:
            return command[index + 1]
    return None


def is_cold_boot_command(command: Sequence[str]) -> bool:
    """Whether an OpenVMM command boots a kernel rather than restoring."""
    return "--kernel" in command and "--restore-snapshot" not in command


def _validate_marker(
    fields: Mapping[str, str],
    *,
    phase: str,
    backend: str | None,
    online_cpus: int | None,
    generation: int | None = None,
) -> None:
    """Check a boot or restore line's fields against the backend's rates."""
    problems: list[str] = []
    if fields.get("v") != ABI_VERSION:
        problems.append(f"version {fields.get('v')!r} is not {ABI_VERSION}")
    if fields.get("phase") != phase or fields.get("status") != "ok":
        problems.append(f"it is not a passing {phase} check")
    try:
        tsc_hz = int(fields.get("tsc_hz", ""))
        if not MIN_TSC_HZ <= tsc_hz <= MAX_TSC_HZ:
            problems.append(f"tsc_hz={tsc_hz} is outside 500 MHz to 10 GHz")
    except ValueError:
        problems.append(f"tsc_hz={fields.get('tsc_hz')!r} is not an integer")
    lapic = fields.get("lapic_hz")
    expected_lapic = LAPIC_HZ.get(backend) if backend is not None else None
    if expected_lapic is not None and lapic != str(expected_lapic):
        problems.append(f"lapic_hz={lapic} is not the {backend} rate {expected_lapic}")
    actual = fields.get("generation", "")
    if phase == "boot" and actual != "0":
        problems.append(f"generation={actual} is not 0 at cold boot")
    if phase == "restore" and not (actual.isdecimal() and int(actual) >= 1):
        problems.append(f"generation={actual} is not 1 or later after a restore")
    elif generation is not None and actual != str(generation):
        problems.append(f"generation={actual} is not {generation}")
    if online_cpus is not None and fields.get("cpus") != str(online_cpus):
        problems.append(f"cpus={fields.get('cpus')} is not {online_cpus}")
    if problems:
        raise TimeAbiFailure(
            f"guest time ABI {phase} marker is invalid: " + "; ".join(problems)
        )


def validate_boot_marker(
    fields: Mapping[str, str],
    *,
    backend: str | None,
    online_cpus: int | None = None,
) -> None:
    """Check the boot marker's fields against the backend's declared rates."""
    _validate_marker(fields, phase="boot", backend=backend, online_cpus=online_cpus)


def validate_restore_marker(
    fields: Mapping[str, str],
    *,
    backend: str | None,
    online_cpus: int | None = None,
    generation: int | None = None,
) -> None:
    """Check a restore marker's fields against the backend's declared rates.

    ``generation`` is the lineage generation the restore must carry: one more
    than the generation of the process that was captured.
    """
    _validate_marker(
        fields,
        phase="restore",
        backend=backend,
        online_cpus=online_cpus,
        generation=generation,
    )


class TimeAbiMonitor:
    """Scan OpenVMM console output for time ABI markers and violations.

    ``feed`` raises TimeAbiFailure as soon as a completed line carries a
    violation event or a failed conformance check, so a scenario fails fast
    with the guest's own explanation instead of a marker timeout. Guests print
    their boot and restore lines only for ``nvx-time status``: when a query
    ends, a cold boot must have reported a valid boot line, and a restored
    guest a restore line. The query's runtime line is kept as ``runtime``.
    """

    def __init__(self, command: Sequence[str] = ()) -> None:
        self.backend = _command_option(command, "--hypervisor")
        self.cold_boot = is_cold_boot_command(command)
        self.restored = "--restore-snapshot" in command
        self.boot: dict[str, str] | None = None
        self.restores: list[dict[str, str]] = []
        self.runtime: dict[str, str] | None = None
        self.violation: str | None = None
        self.fatal: str | None = None
        self.report_only = False
        self.guest_booted = False
        self.status_queries = 0
        self._pending = bytearray()

    def feed(self, chunk: bytes | bytearray) -> None:
        self._pending.extend(chunk)
        while True:
            newline = self._pending.find(b"\n")
            if newline < 0:
                break
            line = bytes(self._pending[:newline])
            del self._pending[: newline + 1]
            self._line(line.decode("utf-8", "replace"))
        if len(self._pending) > _MAX_PENDING_LINE:
            del self._pending[: len(self._pending) - 4096]

    def finish(self) -> None:
        """Scan an unterminated final line once the output reached EOF."""
        if self._pending:
            line = bytes(self._pending)
            self._pending.clear()
            self._line(line.decode("utf-8", "replace"))

    def _line(self, line: str) -> None:
        line = _clean_line(line)
        # A guest shell prompt without a newline can precede OpenVMM's line.
        fatal = line.find(OPENVMM_FATAL_CODE_PREFIX)
        if fatal >= 0:
            if self.fatal is None:
                self.fatal = line[fatal + len(OPENVMM_FATAL_PREFIX) :]
            return
        if VIOLATION_PREFIX in line:
            try:
                event = parse_violation(line)
            except ValueError:
                event = None
            self.violation = line[line.find(VIOLATION_PREFIX) :]
            code = event["code"] if event is not None else "unparsed"
            raise TimeAbiFailure(
                f"guest reported time ABI violation {code}: {self.violation}"
            )
        if line.startswith(REPORT_ONLY_PREFIX):
            self.report_only = True
            return
        if any(line.startswith(marker) for marker in GUEST_BOOT_MARKERS) or any(
            f" {marker}" in line for marker in GUEST_BOOT_MARKERS
        ):
            self.guest_booted = True
        status_exit = _STATUS_EXIT.search(line)
        if status_exit is not None:
            self._finish_status(int(status_exit.group(1)))
            return
        if not line.startswith(MARKER_PREFIX):
            return
        try:
            fields = parse_marker(line)
        except ValueError as error:
            raise TimeAbiFailure(f"malformed time ABI marker: {error}") from error
        assert fields is not None
        status = fields["status"]
        phase = fields["phase"]
        if phase == "runtime":
            # The wall-clock discipline's state; it never gates.
            self.runtime = fields
            return
        if status == "fail":
            raise TimeAbiFailure(
                f"guest time ABI {phase} check {fields.get('check', '?')} "
                f"failed: {fields.get('detail', line)}"
            )
        if status == "pending":
            raise TimeAbiFailure(
                f"guest time ABI {phase} check was still pending when nvx-time "
                f"status stopped waiting after {STATUS_WAIT_SECONDS} s"
            )
        if status != "ok":
            raise TimeAbiFailure(f"unknown time ABI marker status: {line}")
        if phase == "boot":
            if self.boot is None:
                self.boot = fields
        elif phase == "restore":
            self.restores.append(fields)

    def check_exit(self, returncode: int | None) -> None:
        """Fail on an OpenVMM exit status produced by a time ABI power-off."""
        description = describe_exit_status(returncode)
        if description is None:
            return
        event = self.violation or "no NVX-TIME-ABI-VIOLATION event was observed"
        raise TimeAbiFailure(f"{description}: {event}")

    def exit_error(
        self, returncode: int | None, subject: str = "OpenVMM", when: str = ""
    ) -> RuntimeError:
        """Return the error for a failed exit, led by its time ABI code if any."""
        message = f"{subject} exited with status {returncode}"
        if when:
            message += f" {when}"
        if self.fatal is not None:
            message += f": {self.fatal}"
        return RuntimeError(message)

    def _finish_status(self, code: int) -> None:
        """Check what one ``nvx-time status`` query printed before it exited."""
        self.status_queries += 1
        if code == STATUS_USAGE_EXIT:
            raise TimeAbiFailure(
                f"nvx-time status exited {code}: the guest image has no status "
                "subcommand and predates the quiet console"
            )
        # A report-only guest exits 1 when a check failed; the requirement
        # below names report-only mode instead.
        if code != 0 and not self.report_only:
            raise TimeAbiFailure(f"nvx-time status exited {code}")
        if self.cold_boot:
            self.require_boot("nvx-time status exited")
        elif self.restored:
            self.require_restore("nvx-time status exited")

    def require_status(self, context: str) -> None:
        """Fail unless an ``nvx-time status`` query finished before ``context``."""
        if self.status_queries == 0:
            raise TimeAbiFailure(
                f"nvx-time status did not finish before {context}; the guest never "
                "reported its time ABI state"
            )

    def _missing_reason(self) -> str:
        if self.report_only:
            return "the guest ran nvx-time in report-only mode"
        return "the guest image or OpenVMM does not implement time ABI v1"

    def require_boot(
        self,
        context: str,
        *,
        online_cpus: int | None = None,
    ) -> dict[str, str]:
        """Return the validated boot marker of a cold boot, or fail clearly."""
        if self.boot is None:
            raise TimeAbiFailure(
                f"guest did not report the NVX-TIME-ABI boot marker before {context}; "
                f"{self._missing_reason()}"
            )
        validate_boot_marker(
            self.boot,
            backend=self.backend,
            online_cpus=online_cpus,
        )
        return self.boot

    def require_restore(
        self,
        context: str,
        *,
        online_cpus: int | None = None,
        generation: int | None = None,
    ) -> dict[str, str]:
        """Return the validated latest restore marker, or fail clearly.

        After a restore, nvx-time status prints every recorded phase, oldest
        first, and each line keeps the values from when its check ran: only
        the restore line describes the CPUs that are online now.
        """
        if not self.restores:
            raise TimeAbiFailure(
                f"guest did not report an NVX-TIME-ABI restore marker before "
                f"{context}; {self._missing_reason()}"
            )
        validate_restore_marker(
            self.restores[-1],
            backend=self.backend,
            online_cpus=online_cpus,
            generation=generation,
        )
        return self.restores[-1]


def status_script() -> str:
    """Return a guest command line that prints the time ABI status lines.

    nvx-time status first waits for pending checks: the asynchronous boot
    checks, or a restore's deferred work. The line then echoes
    STATUS_EXIT_PREFIX with the exit status, where the monitor checks what
    the query printed.
    """
    return f'{STATUS_COMMAND}; echo "{STATUS_EXIT_PREFIX}$?"\n'


def warp_probe_command(*, cpus: str | None = None) -> str:
    """Return the guest command line that runs the warp probe."""
    command = f"{WARP_PROBE_PATH} warp --bound-ns {WARP_BOUND_NS}"
    return command if cpus is None else f"{command} --cpus {cpus}"


def warp_rounds(cpus: int, gaps: Sequence[str] = CI_WARP_GAPS) -> int:
    """Return how many probe rounds a warp schedule runs on ``cpus`` CPUs."""
    return 1 if cpus < 2 else len(gaps) + 1


def warp_probe_script(gaps: Sequence[str] = CI_WARP_GAPS) -> str:
    """Return a guest shell fragment that runs an idle-inducing warp schedule.

    The probe runs over every online CPU, and again after each idle gap in
    ``gaps`` (seconds), during which every vCPU halts: a host without an
    invariant TSC corrects guest TSC timing when an idle host CPU wakes,
    which busy probe rounds alone never trigger (#265). A single-CPU guest
    runs it once, because it has no CPU pair to compare across a gap. The
    CPU list is read when the fragment runs, so one post-restore script
    serves every restore target. A failed round prints
    WARP_PROBE_FAILURE_MARKER and powers the guest off with
    WARP_PROBE_FAILURE_STATUS; otherwise the fragment prints
    WARP_PROBE_COMPLETION_MARKER.
    """
    if any(not re.fullmatch(r"[0-9]+(\.[0-9]+)?", gap) for gap in gaps):
        raise ValueError(f"invalid warp probe idle gaps: {gaps!r}")
    probe = warp_probe_command(cpus='"$warp_cpus"')
    return (
        'warp_cpus="$(cat /sys/devices/system/cpu/online)"\n'
        f'warp_gaps="{" ".join(gaps)}"\n'
        'case "$warp_cpus" in\n'
        "    *[-,]*) ;;\n"
        "    *) warp_gaps= ;;\n"
        "esac\n"
        "warp_round=0\n"
        "for warp_gap in 0 $warp_gaps; do\n"
        '    [ "$warp_round" -eq 0 ] || sleep "$warp_gap"\n'
        "    warp_round=$((warp_round + 1))\n"
        "    warp_status=0\n"
        f"    {probe} || warp_status=$?\n"
        '    if [ "$warp_status" -ne 0 ]; then\n'
        f'        echo "{WARP_PROBE_FAILURE_MARKER.decode()} '
        'status=$warp_status round=$warp_round"\n'
        f"        nvx-exit {WARP_PROBE_FAILURE_STATUS}\n"
        f"        exit {WARP_PROBE_FAILURE_STATUS}\n"
        "    fi\n"
        "done\n"
        f"echo {WARP_PROBE_COMPLETION_MARKER.decode()}\n"
    )


def check_warp_probe(
    text: str,
    *,
    cpus: int,
    context: str,
    rounds: int | None = None,
) -> list[dict[str, str]]:
    """Validate every warp probe summary in console output.

    The spec bounds both the backward step and the ping-pong offset by 1 us
    and forbids stalled pairs; an inconclusive measurement proves nothing.
    ``rounds``, when given, is the exact number of probe rounds expected.
    """
    summaries: list[dict[str, str]] = []
    details: list[dict[str, str]] = []
    for raw in text.splitlines():
        line = _clean_line(raw)
        try:
            if line.startswith(WARP_DETAIL_PREFIX):
                details.append(parse_fields(line.removeprefix(WARP_DETAIL_PREFIX)))
            elif line.startswith(WARP_SUMMARY_PREFIX):
                summaries.append(parse_fields(line.removeprefix(WARP_SUMMARY_PREFIX)))
        except ValueError as error:
            raise TimeAbiFailure(
                f"{context}: malformed warp probe output: {error}"
            ) from error
    if not summaries:
        raise TimeAbiFailure(f"{context}: the guest warp probe printed no summary")
    if len(details) != len(summaries):
        raise TimeAbiFailure(
            f"{context}: the guest warp probe printed {len(summaries)} summaries "
            f"and {len(details)} detail lines"
        )
    if rounds is not None and len(summaries) != rounds:
        raise TimeAbiFailure(
            f"{context}: the guest warp probe ran {len(summaries)} rounds instead "
            f"of {rounds}"
        )
    expected_pairs = cpus * (cpus - 1) // 2
    results: list[dict[str, str]] = []
    for index, (summary, detail) in enumerate(
        zip(summaries, details, strict=True), start=1
    ):
        merged = {**summary, **detail}
        problems: list[str] = []
        try:
            backward = int(merged["max_backward_ns"])
            offset = int(merged["max_abs_offset_ns"])
            pairs = int(merged["pairs"])
            stalled = int(merged["stalled_pairs"])
        except (KeyError, ValueError) as error:
            raise TimeAbiFailure(
                f"{context}: incomplete warp probe output: {error}"
            ) from error
        if pairs != expected_pairs:
            problems.append(f"measured {pairs} CPU pairs instead of {expected_pairs}")
        if backward > WARP_BOUND_NS:
            problems.append(f"max_backward_ns={backward} exceeds {WARP_BOUND_NS}")
        if offset > WARP_BOUND_NS:
            problems.append(f"max_abs_offset_ns={offset} exceeds {WARP_BOUND_NS}")
        if stalled:
            problems.append(f"{stalled} CPU pair(s) stalled")
        if merged.get("conclusive") != "1":
            problems.append(
                "the measurement is inconclusive "
                f"(max_uncertainty_ns={merged.get('max_uncertainty_ns')})"
            )
        if merged.get("verdict") != "PASS":
            problems.append(f"verdict={merged.get('verdict')}")
        if problems:
            where = f" in round {index}" if len(summaries) > 1 else ""
            raise TimeAbiFailure(
                f"{context}: cross-vCPU TSC skew check failed{where}: "
                + "; ".join(problems)
            )
        results.append(merged)
    return results
