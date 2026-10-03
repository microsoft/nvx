"""Host qualification for the NVX time ABI (``nvx.py doctor``).

doc/design/time-abi.md ("Host qualification") defines checks H1 to H7. Each
check prints one ``NVX-DOCTOR: check=<id> status=<pass|fail> detail="..."``
line, and qualification fails closed if any selected check fails. A failure
that matches a time ABI failure code starts its detail with that code in
brackets, as OpenVMM errors do.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import itertools
import os
import platform
import re
import shutil
import subprocess
import sys
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path

from .benchmark import run_guest_script, workload_boot_command
from .build_constants import AlpineBuildConstants, BuildConstants, KernelBuildConstants
from .ci import OPENVMM_TEST_BACKENDS
from .common import ScriptError, artifact_path, openvmm_binary_path
from .time_abi import (
    CI_WARP_GAPS,
    LAPIC_HZ,
    MAX_TSC_HZ,
    MIN_TSC_HZ,
    QUALIFICATION_WARP_GAPS,
    WARP_BOUND_NS,
    WARP_PROBE_COMPLETION_MARKER,
    CpuGeneration,
    TimeAbiFailure,
    TimeAbiMonitor,
    check_warp_probe,
    cpu_generation,
    parse_fields,
    warp_probe_script,
    warp_rounds,
)

CHECK_IDS = ("H1", "H2", "H3", "H4", "H5", "H6", "H7")
CHECK_TITLES: Mapping[str, str] = {
    "H1": "backend device or API",
    "H2": "CPU fingerprint, generation, and profile",
    "H3": "OpenVMM time ABI preflight",
    "H4": "host TSC rate stability",
    "H5": "host cross-CPU TSC skew",
    "H6": "guest warp probe",
    "H7": "host UTC synchronization",
}
# Checks that boot OpenVMM; H2 also runs it unless qualification has none.
OPENVMM_CHECKS = ("H3", "H6")
DOCTOR_PREFIX = "NVX-DOCTOR: "
PROBE_SOURCE = Path(__file__).with_name("host_time_probe.rs")
PROBE_PREFIX = "NVX-HOST-TIME-PROBE "
VERIFY_PREFIX = "NVX-TIME-ABI-VERIFY:"
CPU_PROFILE_PREFIX = "NVX-CPU-PROFILE:"
RUST_ESCAPE = re.compile(r"\\(u\{[0-9a-fA-F]{1,6}\}|.)")
# doc/design/time-abi.md, "Rate stability (H4)".
RATE_UNCERTAINTY_PPM = 0.25
RATE_AGREEMENT_PPM = 1.0
RATE_TOLERANCE_PPM = 100.0
SKEW_PAIR_DURATION_MS = 5
GUEST_VCPU_COUNTS = (8, 4, 2, 1)
GUEST_MEMORY_MIB = 128


@dataclass(frozen=True)
class Schedule:
    """How long H4 samples the TSC rate and how often H6 runs the warp probe."""

    rate_samples: int
    rate_interval_ms: int
    warp_gaps: tuple[str, ...]


# doc/design/time-abi.md, "Rate stability (H4)" and "Warp schedules": doctor
# qualifies on the long schedules, and CI on the short ones.
QUALIFICATION_SCHEDULE = Schedule(13, 10_000, QUALIFICATION_WARP_GAPS)
CI_SCHEDULE = Schedule(3, 1_000, CI_WARP_GAPS)
# Qualification gates only on measured properties, alike on every backend:
# the guest warp probe, the TSC rate stability, and the CPU profile. The host
# OS's invariant-TSC flags and clocksource are recorded as evidence only,
# because they don't decide what a guest observes: Azure WHP hosts show the
# CPUID bit but cannot offer invariant TSC to partitions, and their guests
# measure tens of nanoseconds of skew.
LINUX_INVARIANT_TSC_FLAGS = ("constant_tsc", "nonstop_tsc")
CLOCKSOURCE_PATH = Path(
    "/sys/devices/system/clocksource/clocksource0/current_clocksource"
)
CPUINFO_PATH = Path("/proc/cpuinfo")
ADJTIMEX_TIME_ERROR = 5
ADJTIMEX_STA_UNSYNC = 0x0040
WHP_CAPABILITY_HYPERVISOR_PRESENT = 0


@dataclass
class CheckResult:
    check: str
    passed: bool
    detail: str

    def line(self) -> str:
        status = "pass" if self.passed else "fail"
        detail = self.detail.replace("\\", "\\\\").replace('"', '\\"')
        return f'{DOCTOR_PREFIX}check={self.check} status={status} detail="{detail}"'


@dataclass
class DoctorContext:
    backend: str
    openvmm: Path
    kernel: Path
    initrd: Path
    openvmm_args: tuple[str, ...]
    probe_directory: Path
    timeout: float
    facts: dict[str, str] = field(default_factory=dict[str, str])
    probe: Path | None = None
    # Where H2 writes OpenVMM's CPU fingerprint; None skips the profile check
    # when qualification runs without OpenVMM.
    fingerprint: Path | None = None
    schedule: Schedule = QUALIFICATION_SCHEDULE


def host_is_windows() -> bool:
    return os.name == "nt"


def host_clocksource() -> str:
    return CLOCKSOURCE_PATH.read_text(encoding="utf-8").strip()


def _escape_markdown(text: str) -> str:
    return text.replace("|", "\\|").replace("\n", " ")


def probe_path(directory: Path) -> Path:
    """Return the cached host probe binary for the current probe source."""
    digest = hashlib.sha256(PROBE_SOURCE.read_bytes()).hexdigest()[:16]
    suffix = ".exe" if os.name == "nt" else ""
    return directory / f"nvx-host-time-probe-{digest}{suffix}"


def build_probe(directory: Path) -> Path:
    """Build the host probe once per source version with rustc."""
    target = probe_path(directory)
    if target.is_file():
        return target
    rustc = shutil.which("rustc")
    if rustc is None:
        raise ScriptError("rustc is required to build the host time probe")
    directory.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(f".{target.stem}-{os.getpid()}{target.suffix}")
    completed = subprocess.run(
        [
            rustc,
            "--edition=2021",
            "-C",
            "opt-level=2",
            "-C",
            "debuginfo=0",
            "-o",
            os.fspath(temporary),
            os.fspath(PROBE_SOURCE),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        temporary.unlink(missing_ok=True)
        raise ScriptError(
            f"rustc failed to build the host time probe:\n{completed.stderr.strip()}"
        )
    try:
        os.replace(temporary, target)
    except OSError:
        # Another job built the same version first and may be running it.
        temporary.unlink(missing_ok=True)
        if not target.is_file():
            raise
    temporary.with_suffix(".pdb").unlink(missing_ok=True)
    return target


def run_probe(
    context: DoctorContext, *arguments: str
) -> list[tuple[str, dict[str, str]]]:
    """Run the host probe and parse its ``NVX-HOST-TIME-PROBE`` records."""
    if context.probe is None:
        context.probe = build_probe(context.probe_directory)
    completed = subprocess.run(
        [os.fspath(context.probe), *arguments],
        capture_output=True,
        text=True,
        timeout=context.timeout,
        check=False,
    )
    if completed.returncode != 0:
        message = completed.stderr.strip() or f"exit status {completed.returncode}"
        raise ScriptError(f"host time probe {arguments[0]} failed: {message}")
    records: list[tuple[str, dict[str, str]]] = []
    for line in completed.stdout.splitlines():
        if line.startswith(PROBE_PREFIX):
            kind, _, fields = line.removeprefix(PROBE_PREFIX).partition(" ")
            records.append((kind, parse_fields(fields)))
    return records


def _probe_record(
    records: Sequence[tuple[str, dict[str, str]]], kind: str
) -> dict[str, str]:
    for record_kind, fields in records:
        if record_kind == kind:
            return fields
    raise ScriptError(f"host time probe printed no {kind} record")


def _linux_cpuinfo(path: Path = CPUINFO_PATH) -> dict[str, str]:
    """Return the first processor's /proc/cpuinfo fields."""
    fields: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            if fields:
                break
            continue
        name, separator, value = line.partition(":")
        if separator:
            fields.setdefault(name.strip(), value.strip())
    return fields


def _windows_registry_value(key: str, name: str) -> object:
    if sys.platform != "win32":
        raise ScriptError("the Windows registry is unavailable")
    import winreg

    with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, key) as handle:
        value, _ = winreg.QueryValueEx(handle, name)
        return value


def host_cpu(context: DoctorContext) -> dict[str, str]:
    """Collect the host CPU identity and the host OS's view of its TSC."""
    if host_is_windows():
        match = re.search(
            r"Family (\d+) Model (\d+) Stepping (\d+), (\S+)", platform.processor()
        )
        if match is None:
            raise ScriptError(f"cannot parse the processor {platform.processor()!r}")
        family, model, stepping, vendor = match.groups()
        processor_key = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0"
        info = {
            "vendor": vendor,
            "family": family,
            "model": model,
            "stepping": stepping,
            "brand": str(
                _windows_registry_value(processor_key, "ProcessorNameString")
            ).strip(),
        }
        revision = _windows_registry_value(processor_key, "Update Revision")
        if isinstance(revision, bytes) and len(revision) in (4, 8):
            info["microcode"] = hex(int.from_bytes(revision[-4:], "little"))
        build = _windows_registry_value(
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "UBR"
        )
        info["os"] = f"Windows {platform.version()}.{build}"
        try:
            probed = _probe_record(run_probe(context, "cpu"), "cpu")
            info["invariant_tsc"] = "yes" if probed["invariant_tsc"] == "1" else "no"
        except (ScriptError, KeyError, subprocess.TimeoutExpired) as error:
            info["invariant_tsc"] = f"unknown ({error})"
        return info
    cpuinfo = _linux_cpuinfo()
    flags = cpuinfo.get("flags", "").split()
    missing = [flag for flag in LINUX_INVARIANT_TSC_FLAGS if flag not in flags]
    return {
        "vendor": cpuinfo.get("vendor_id", "unknown"),
        "family": cpuinfo.get("cpu family", "?"),
        "model": cpuinfo.get("model", "?"),
        "stepping": cpuinfo.get("stepping", "?"),
        "microcode": cpuinfo.get("microcode", "unknown"),
        "brand": cpuinfo.get("model name", "unknown"),
        "os": f"Linux {platform.release()}",
        "invariant_tsc": "yes" if not missing else f"no (missing {' '.join(missing)})",
    }


def _whp_hypervisor_present() -> bool:
    if sys.platform != "win32":
        raise ScriptError("WHP qualification requires Windows")
    try:
        library = ctypes.WinDLL("WinHvPlatform.dll")
    except OSError as error:
        raise ScriptError(f"WinHvPlatform.dll is unavailable: {error}") from error
    present = ctypes.c_int32(0)
    written = ctypes.c_uint32(0)
    result = int(
        library.WHvGetCapability(
            WHP_CAPABILITY_HYPERVISOR_PRESENT,
            ctypes.byref(present),
            ctypes.sizeof(present),
            ctypes.byref(written),
        )
    )
    if result != 0:
        raise ScriptError(
            f"WHvGetCapability failed with HRESULT 0x{result & 0xFFFFFFFF:08x}"
        )
    return bool(present.value)


def check_backend(context: DoctorContext) -> CheckResult:
    if context.backend == "whp":
        if not host_is_windows():
            return CheckResult("H1", False, "WHP qualification requires Windows")
        if not _whp_hypervisor_present():
            return CheckResult(
                "H1", False, "the Windows Hypervisor Platform reports no hypervisor"
            )
        return CheckResult("H1", True, "the Windows Hypervisor Platform is present")
    if host_is_windows():
        return CheckResult(
            "H1", False, f"{context.backend} qualification requires Linux"
        )
    device = Path("/dev") / context.backend
    if not device.exists():
        return CheckResult("H1", False, f"{device} does not exist")
    if not os.access(device, os.R_OK | os.W_OK):
        return CheckResult("H1", False, f"{device} is not readable and writable")
    if context.backend == "kvm" and Path("/dev/mshv").exists():
        return CheckResult(
            "H1", False, "/dev/mshv exists, so OpenVMM would select MSHV, not KVM"
        )
    return CheckResult("H1", True, f"{device} is readable and writable")


def check_cpu(context: DoctorContext) -> CheckResult:
    info = host_cpu(context)
    signature = f"{info['vendor']} {info['family']}/{info['model']}/{info['stepping']}"
    try:
        generation = cpu_generation(
            info["vendor"],
            int(info["family"]),
            int(info["model"]),
            int(info["stepping"]),
        )
    except ValueError:
        generation = None
    profile = generation.profile_id if generation else None
    name = generation.name if generation else "unknown"
    context.facts.update(
        {
            "generation": name,
            "profile": profile or "none",
            "cpu": signature,
            "brand": info["brand"],
            "microcode": info.get("microcode", "unknown"),
            "os": info["os"],
            "invariant_tsc": info["invariant_tsc"],
        }
    )
    detail = (
        f"generation={name} profile={profile or 'none'} cpu={signature} "
        f"microcode={info.get('microcode', 'unknown')} os={info['os']} "
        f"invariant_tsc={info['invariant_tsc']} brand={info['brand']}"
    )
    problems: list[str] = []
    if generation is None:
        problems.append(
            f"[E_PROFILE_HOST_UNKNOWN] {signature} is not a time ABI generation "
            "(skylake-sp 6/85 steppings 0-4, icelake-sp 6/106, emeraldrapids 6/207)"
        )
    if context.fingerprint is None:
        detail += "; CPU profile not checked: qualification runs without OpenVMM"
    else:
        profile_detail, profile_problems = _check_cpu_profile(context, generation)
        detail += f"; {profile_detail}"
        problems.extend(profile_problems)
    if problems:
        return CheckResult("H2", False, "; ".join(problems) + f"; {detail}")
    return CheckResult("H2", True, detail)


def parse_cpu_profile_line(line: str) -> dict[str, str]:
    """Parse OpenVMM's ``NVX-CPU-PROFILE:`` line; ``detail`` is a Rust string."""
    text = line.strip().removeprefix(CPU_PROFILE_PREFIX)
    head, quoted, detail = text.partition(' detail="')
    fields = parse_fields(head)
    if quoted:
        fields["detail"] = RUST_ESCAPE.sub(_rust_unescape, detail.removesuffix('"'))
    return fields


def _rust_unescape(match: re.Match[str]) -> str:
    escape = match.group(1)
    if escape.startswith("u{"):
        return chr(int(escape[2:-1], 16))
    return {"n": "\n", "r": "\r", "t": "\t", "0": "\0"}.get(escape, escape)


def _same_profile_lineage(selected: str, expected: str) -> bool:
    """Whether ``selected`` is a revision of the profile ``expected`` names."""
    return selected.startswith(expected.rpartition(".v")[0] + ".v")


def _check_cpu_profile(
    context: DoctorContext, generation: CpuGeneration | None
) -> tuple[str, list[str]]:
    """Fingerprint the host with OpenVMM and check its generation's profile."""
    assert context.fingerprint is not None
    if not context.openvmm.is_file():
        return "CPU profile not checked", [
            f"OpenVMM was not found at {context.openvmm}; H2 checks the CPU "
            "profile with its --cpu-fingerprint tool (or pass --no-openvmm)"
        ]
    context.fingerprint.parent.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    completed = subprocess.run(
        [
            os.fspath(context.openvmm),
            "--hypervisor",
            context.backend,
            "--cpu-fingerprint",
            os.fspath(context.fingerprint),
        ],
        capture_output=True,
        text=True,
        timeout=context.timeout,
        env=environment,
        check=False,
    )
    output = f"{completed.stdout}\n{completed.stderr}"
    line = next(
        (
            line.strip()
            for line in output.splitlines()
            if line.strip().startswith(CPU_PROFILE_PREFIX)
        ),
        None,
    )
    if line is None:
        last = next(
            (line.strip() for line in reversed(output.splitlines()) if line.strip()),
            "no output",
        )
        if "--cpu-fingerprint" in output and "unexpected argument" in output:
            last = "this OpenVMM predates the --cpu-fingerprint tool"
        return "CPU profile not checked", [
            f"OpenVMM's CPU fingerprint exited {completed.returncode} without "
            f"an {CPU_PROFILE_PREFIX} line: {last}"
        ]
    fields = parse_cpu_profile_line(line)
    for name in ("profile", "profile_digest", "surface_digest"):
        if fields.get(name, "none") != "none":
            context.facts[name] = fields[name]
    context.facts["cpu_fingerprint"] = os.fspath(context.fingerprint)
    detail = (
        f"CPU profile check status={fields.get('status')} "
        f"profile={fields.get('profile')} "
        f"profile_digest={fields.get('profile_digest', 'none')} "
        f"surface_digest={fields.get('surface_digest')} "
        f"fingerprint={context.fingerprint}"
    )
    problems: list[str] = []
    if completed.returncode != 0 or fields.get("status") != "pass":
        code = fields.get("code", "")
        prefix = f"[{code}] " if code.startswith("E_") else ""
        problems.append(
            f"{prefix}OpenVMM's CPU profile check failed (exit "
            f"{completed.returncode}): {fields.get('detail', line)}"
        )
        return detail, problems
    if fields.get("backend") != context.backend:
        problems.append(f"OpenVMM fingerprinted the {fields.get('backend')} backend")
    if generation is not None:
        if fields.get("generation") != generation.name:
            problems.append(
                f"OpenVMM maps the host to generation {fields.get('generation')}, "
                f"not {generation.name}"
            )
        if not _same_profile_lineage(fields.get("profile", ""), generation.profile_id):
            problems.append(
                f"OpenVMM selected CPU profile {fields.get('profile')}, not a "
                f"revision of {generation.profile_id}"
            )
    return detail, problems


def _guest_vcpus() -> int:
    host_cpus = os.cpu_count() or 1
    return next(count for count in GUEST_VCPU_COUNTS if count <= host_cpus)


def check_openvmm_preflight(context: DoctorContext) -> CheckResult:
    for path, description in (
        (context.openvmm, "OpenVMM"),
        (context.kernel, "guest kernel"),
        (context.initrd, "guest initramfs"),
    ):
        if not path.is_file():
            return CheckResult("H3", False, f"{description} was not found at {path}")
    command = [
        os.fspath(context.openvmm),
        "--machine",
        "microvm",
        "--processors",
        str(_guest_vcpus()),
        "--hypervisor",
        context.backend,
        "--kernel",
        os.fspath(context.kernel),
        "--initrd",
        os.fspath(context.initrd),
        *context.openvmm_args,
        "--x-time-abi-verify",
    ]
    environment = os.environ.copy()
    environment["OPENVMM_LOG"] = "off"
    completed = subprocess.run(
        command,
        capture_output=True,
        text=True,
        timeout=context.timeout,
        env=environment,
        check=False,
    )
    output = f"{completed.stdout}\n{completed.stderr}"
    verify = next(
        (
            line.strip()
            for line in output.splitlines()
            if line.strip().startswith(VERIFY_PREFIX)
        ),
        None,
    )
    if verify is None:
        last = next(
            (line.strip() for line in reversed(output.splitlines()) if line.strip()),
            "no output",
        )
        if "unexpected argument '--x-time-abi-verify'" in output:
            last = "this OpenVMM predates the time ABI's --x-time-abi-verify mode"
        return CheckResult(
            "H3",
            False,
            f"OpenVMM verification exited {completed.returncode} without a "
            f"verification line: {last}",
        )
    fields = parse_fields(verify.removeprefix(VERIFY_PREFIX).strip())
    if fields.get("status") != "ok" or completed.returncode != 0:
        code = fields.get("code", "none")
        prefix = f"[{code}] " if code.startswith("E_") else ""
        return CheckResult(
            "H3",
            False,
            f"{prefix}OpenVMM verification failed (exit {completed.returncode}): "
            f"{fields.get('detail', verify)}",
        )
    problems: list[str] = []
    if fields.get("backend") != context.backend:
        problems.append(f"OpenVMM verified backend {fields.get('backend')}")
    for name in ("tsc_hz", "native_tsc_hz"):
        try:
            rate = int(fields.get(name, ""))
            if not MIN_TSC_HZ <= rate <= MAX_TSC_HZ:
                problems.append(f"[E_TSC_RATE_IMPLAUSIBLE] {name}={rate}")
        except ValueError:
            problems.append(f"{name}={fields.get(name)!r} is not an integer")
    expected_lapic = LAPIC_HZ[context.backend]
    if fields.get("lapic_hz") != str(expected_lapic):
        problems.append(
            f"[E_LAPIC_RATE_MISMATCH] lapic_hz={fields.get('lapic_hz')} is not "
            f"{expected_lapic}"
        )
    expected_profile = context.facts.get("profile", "none")
    selected_profile = fields.get("cpu_profile", "")
    if expected_profile != "none" and not _same_profile_lineage(
        selected_profile, expected_profile
    ):
        problems.append(
            f"OpenVMM selected CPU profile {selected_profile}, but H2 maps the "
            f"host to {expected_profile}"
        )
    detail = " ".join(f"{name}={value}" for name, value in fields.items())
    context.facts.update(
        {
            "tsc_hz": fields.get("tsc_hz", ""),
            "native_tsc_hz": fields.get("native_tsc_hz", ""),
            "lapic_hz": fields.get("lapic_hz", ""),
            "profile": selected_profile,
            "route": fields.get("msr_route", ""),
            "sync": fields.get("sync", ""),
        }
    )
    if problems:
        return CheckResult("H3", False, "; ".join(problems) + f"; {detail}")
    return CheckResult("H3", True, detail)


def _rate_points(
    samples: Sequence[Mapping[str, str]], clock: Mapping[str, str], role: str
) -> list[tuple[int, int, float]]:
    """Return each sample's (TSC, bracket midpoint in half clock ticks,
    uncertainty in clock ticks: half the bracket plus half the resolution)."""
    half_resolution = float(clock["resolution_ns"]) * int(clock["hz"]) / 2e9
    return [
        (
            int(sample[f"{role}_tsc"]),
            2 * int(sample[f"{role}_clock"]) + int(sample[f"{role}_bracket"]),
            int(sample[f"{role}_bracket"]) / 2 + half_resolution,
        )
        for sample in samples
    ]


def _window_rate(points: Sequence[tuple[int, int, float]], clock_hz: int) -> float:
    seconds = (points[-1][1] - points[0][1]) / (2 * clock_hz)
    if seconds <= 0:
        raise ScriptError("the host clock did not advance between samples")
    return (points[-1][0] - points[0][0]) / seconds


def check_rate(context: DoctorContext) -> CheckResult:
    clocksource = ""
    if not host_is_windows():
        clocksource = host_clocksource()
        context.facts["host_clocksource"] = clocksource
    schedule = context.schedule
    records = run_probe(
        context,
        "rate",
        "--samples",
        str(schedule.rate_samples),
        "--interval-ms",
        str(schedule.rate_interval_ms),
    )
    clocks = {fields.get("role"): fields for kind, fields in records if kind == "clock"}
    for role in ("rate", "stability"):
        if role not in clocks:
            raise ScriptError(f"host time probe printed no {role} clock")
    samples = [fields for kind, fields in records if kind == "sample"]
    if len(samples) != schedule.rate_samples:
        raise ScriptError(
            f"host time probe printed {len(samples)} samples, not "
            f"{schedule.rate_samples}"
        )
    # The rate clock, which time synchronization disciplines, gives the true
    # rate. The stability clock, which it never steers, shows whether the TSC
    # keeps its rate across idle: chrony's frequency updates move
    # CLOCK_MONOTONIC's rate by several ppm between seconds.
    rate_clock, stability_clock = clocks["rate"], clocks["stability"]
    measured = _window_rate(
        _rate_points(samples, rate_clock, "rate"), int(rate_clock["hz"])
    )
    clock_hz = int(stability_clock["hz"])
    points = _rate_points(samples, stability_clock, "stability")
    rates: list[float] = []
    uncertainties_ppm: list[float] = []
    for (tsc0, mid0, error0), (tsc1, mid1, error1) in itertools.pairwise(points):
        if mid1 <= mid0:
            raise ScriptError("the host clock did not advance between samples")
        seconds = (mid1 - mid0) / (2 * clock_hz)
        rates.append((tsc1 - tsc0) / seconds)
        uncertainties_ppm.append((error0 + error1) / clock_hz / seconds * 1e6)
    stable_rate = _window_rate(points, clock_hz)
    agreement_ppm = (max(rates) - min(rates)) / stable_rate * 1e6
    uncertainty_ppm = max(uncertainties_ppm)
    context.facts["measured_tsc_hz"] = f"{measured:.0f}"
    context.facts["rate_agreement_ppm"] = f"{agreement_ppm:.3f}"
    context.facts["rate_uncertainty_ppm"] = f"{uncertainty_ppm:.3f}"
    detail = (
        f"tsc_hz={measured:.0f} against {rate_clock['name']} from {len(samples)} "
        f"samples {schedule.rate_interval_ms / 1000:g} s apart; against "
        f"{stability_clock['name']} (resolution "
        f"{float(stability_clock['resolution_ns']):g} ns) the interval rates agree "
        f"within {agreement_ppm:.3f} ppm, largest interval uncertainty "
        f"{uncertainty_ppm:.3f} ppm"
    )
    problems: list[str] = []
    if not MIN_TSC_HZ <= measured <= MAX_TSC_HZ:
        problems.append(f"[E_TSC_RATE_IMPLAUSIBLE] measured {measured:.0f} Hz")
    if uncertainty_ppm > RATE_UNCERTAINTY_PPM:
        problems.append(
            f"the measurement is inconclusive: an interval's uncertainty is "
            f"{uncertainty_ppm:.3f} ppm > {RATE_UNCERTAINTY_PPM} ppm"
        )
    if agreement_ppm > RATE_AGREEMENT_PPM:
        problems.append(
            f"the TSC rate is unstable: interval rates differ by "
            f"{agreement_ppm:.3f} ppm > {RATE_AGREEMENT_PPM} ppm"
        )
    declared = context.facts.get("native_tsc_hz") or context.facts.get("tsc_hz")
    if declared is None:
        # validate-runner runs before the OpenVMM binary is available; the
        # VM jobs run H3 with H4 to compare against the backend's rate.
        detail += "; not compared with the backend's rate (H3 did not run)"
    else:
        deviation_ppm = abs(measured - int(declared)) / int(declared) * 1e6
        context.facts["rate_deviation_ppm"] = f"{deviation_ppm:.1f}"
        detail += f"; {deviation_ppm:.1f} ppm from the backend's {declared} Hz"
        if deviation_ppm > RATE_TOLERANCE_PPM:
            problems.append(
                f"the backend rate is {deviation_ppm:.1f} ppm from the measured "
                f"rate (limit {RATE_TOLERANCE_PPM:g} ppm)"
            )
    if clocksource:
        detail += f"; host clocksource {clocksource} (evidence)"
    if problems:
        return CheckResult("H4", False, "; ".join(problems) + f"; {detail}")
    return CheckResult("H4", True, detail)


def check_host_skew(context: DoctorContext) -> CheckResult:
    arguments = ["skew", "--duration-ms", str(SKEW_PAIR_DURATION_MS)]
    rate = context.facts.get("measured_tsc_hz") or context.facts.get("tsc_hz")
    if rate is not None:
        arguments.extend(("--tsc-hz", rate))
    summary = _probe_record(run_probe(context, *arguments), "skew")
    offset = int(summary["max_abs_offset_ns"])
    uncertainty = int(summary["max_uncertainty_ns"])
    stalled = int(summary["stalled_pairs"])
    context.facts["host_skew_ns"] = str(offset)
    detail = (
        f"pairs={summary['pairs']} cpus={summary['cpus']} max_abs_offset_ns={offset} "
        f"max_uncertainty_ns={uncertainty}"
    )
    problems: list[str] = []
    if offset > WARP_BOUND_NS:
        problems.append(f"max_abs_offset_ns={offset} exceeds {WARP_BOUND_NS}")
    if stalled:
        problems.append(f"{stalled} CPU pair(s) stalled")
    if summary.get("conclusive") != "1":
        problems.append(
            f"the measurement is inconclusive (uncertainty {uncertainty} ns)"
        )
    if problems:
        return CheckResult("H5", False, "; ".join(problems) + f"; {detail}")
    return CheckResult("H5", True, detail)


def _warp_guest(
    context: DoctorContext, vcpus: int, gaps: Sequence[str]
) -> tuple[dict[str, str], list[dict[str, str]]]:
    """Boot a guest, check its boot marker, and run a warp schedule in it."""
    command = workload_boot_command(
        context.openvmm,
        context.backend,
        context.kernel,
        context.initrd,
        GUEST_MEMORY_MIB,
        "quiet loglevel=0",
        processors=vcpus,
    )
    command.extend(context.openvmm_args)
    result = run_guest_script(
        command,
        warp_probe_script(gaps) + "nvx-exit 0\n",
        WARP_PROBE_COMPLETION_MARKER,
        timeout=context.timeout,
        time_abi_status=True,
    )
    monitor = TimeAbiMonitor(command)
    monitor.feed(result["text"].encode())
    monitor.finish()
    monitor.require_status("the warp probe")
    boot = monitor.require_boot("the warp probe", online_cpus=vcpus)
    rounds = check_warp_probe(
        result["text"], cpus=vcpus, context="H6", rounds=warp_rounds(vcpus, gaps)
    )
    return boot, rounds


def check_guest_warp(context: DoctorContext) -> CheckResult:
    for path, description in (
        (context.openvmm, "OpenVMM"),
        (context.kernel, "guest kernel"),
        (context.initrd, "guest initramfs"),
    ):
        if not path.is_file():
            return CheckResult("H6", False, f"{description} was not found at {path}")
    vcpus = _guest_vcpus()
    # The largest guest runs the schedule, then a 1-vCPU guest runs the
    # probe once.
    guests: list[tuple[int, tuple[str, ...]]] = []
    if vcpus > 1:
        guests.append((vcpus, context.schedule.warp_gaps))
    guests.append((1, ()))
    boots: list[dict[str, str]] = []
    rounds: list[dict[str, str]] = []
    runs: list[str] = []
    for count, count_gaps in guests:
        try:
            boot, count_rounds = _warp_guest(context, count, count_gaps)
        except (RuntimeError, TimeAbiFailure) as error:
            first = str(error).splitlines()[0]
            return CheckResult("H6", False, f"vcpus={count}: {first}")
        boots.append(boot)
        rounds.extend(count_rounds)
        # The boot check's wall time and, where the guest reports it, its CPU
        # time, which the spec budgets; both are evidence only.
        cpu_us = f" boot_cpu_us={boot['cpu_us']}" if "cpu_us" in boot else ""
        runs.append(
            f"vcpus={count} rounds={len(count_rounds)} "
            f"idle_gaps_s={','.join(count_gaps) or 'none'} "
            f"boot_elapsed_us={boot.get('elapsed_us', '?')}{cpu_us}"
        )

    def worst(name: str) -> int:
        return max(int(warp[name]) for warp in rounds)

    context.facts["guest_warp_ns"] = str(worst("max_abs_offset_ns"))
    context.facts.setdefault("tsc_hz", boots[0]["tsc_hz"])
    context.facts.setdefault("lapic_hz", boots[0]["lapic_hz"])
    return CheckResult(
        "H6",
        True,
        "; ".join(runs) + f"; max_backward_ns={worst('max_backward_ns')} "
        f"max_abs_offset_ns={worst('max_abs_offset_ns')} "
        f"max_uncertainty_ns={worst('max_uncertainty_ns')}",
    )


def _linux_clock_state() -> tuple[int, int]:
    """Return adjtimex's clock state and status without changing the clock."""
    libc = ctypes.CDLL(None, use_errno=True)
    buffer = ctypes.create_string_buffer(256)
    state = int(libc.adjtimex(buffer))
    if state < 0:
        raise ScriptError(f"adjtimex failed: {os.strerror(ctypes.get_errno())}")
    # struct timex: unsigned modes, then the long offset, freq, maxerror, and
    # esterror fields, then the int status.
    status = int.from_bytes(buffer.raw[40:44], sys.byteorder)
    return state, status


def _windows_time_source() -> tuple[bool, str]:
    completed = subprocess.run(
        ["w32tm", "/query", "/status"],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    if completed.returncode != 0:
        raise ScriptError(
            "w32tm /query /status failed: "
            + (completed.stdout.strip() or completed.stderr.strip())
        )
    fields: dict[str, str] = {}
    for line in completed.stdout.splitlines():
        name, separator, value = line.partition(":")
        if separator:
            fields[name.strip()] = value.strip()
    source = fields.get("Source", "")
    leap = fields.get("Leap Indicator", "")
    # w32tm prints its labels in the display language. Output without both
    # English labels and a 0-3 leap indicator names no source, so the check
    # fails rather than certify an unverified one.
    if not source or not leap.startswith(("0", "1", "2", "3")):
        raise ScriptError(
            "cannot verify the host time source: w32tm /query /status reported "
            f"source={source or 'none'} leap_indicator={leap or 'none'}"
        )
    unsynchronized = (
        leap.startswith("3")
        or "Local CMOS Clock" in source
        or "Free-running System Clock" in source
    )
    return not unsynchronized, f"source={source} leap_indicator={leap}"


def check_utc(context: DoctorContext) -> CheckResult:
    del context
    if host_is_windows():
        synchronized, detail = _windows_time_source()
        if not synchronized:
            return CheckResult("H7", False, f"host UTC is not synchronized: {detail}")
        return CheckResult("H7", True, detail)
    state, status = _linux_clock_state()
    detail = f"adjtimex state={state} status=0x{status:04x}"
    if state == ADJTIMEX_TIME_ERROR or status & ADJTIMEX_STA_UNSYNC:
        return CheckResult(
            "H7", False, f"host UTC is not synchronized (STA_UNSYNC); {detail}"
        )
    return CheckResult("H7", True, detail)


CHECKS: Mapping[str, Callable[[DoctorContext], CheckResult]] = {
    "H1": check_backend,
    "H2": check_cpu,
    "H3": check_openvmm_preflight,
    "H4": check_rate,
    "H5": check_host_skew,
    "H6": check_guest_warp,
    "H7": check_utc,
}


def run_checks(context: DoctorContext, checks: Sequence[str]) -> list[CheckResult]:
    """Run checks in spec order; an exception fails only its own check."""
    results: list[CheckResult] = []
    for check in CHECK_IDS:
        if check not in checks:
            continue
        try:
            result = CHECKS[check](context)
        except (
            KeyError,
            OSError,
            RuntimeError,
            subprocess.SubprocessError,
            ValueError,
        ) as error:
            first = str(error).splitlines()[0] if str(error) else type(error).__name__
            result = CheckResult(check, False, first)
        results.append(result)
        print(result.line(), flush=True)
    return results


def summary_markdown(
    backend: str, results: Sequence[CheckResult], facts: Mapping[str, str]
) -> str:
    status = "passed" if all(result.passed for result in results) else "failed"
    lines = [
        f"### Time ABI host qualification ({backend}): {status}",
        "",
        "| Check | Name | Status | Detail |",
        "| --- | --- | --- | --- |",
    ]
    for result in results:
        lines.append(
            f"| {result.check} | {CHECK_TITLES[result.check]} | "
            f"{'pass' if result.passed else '**fail**'} | "
            f"{_escape_markdown(result.detail)} |"
        )
    reported = (
        ("generation", "Generation"),
        ("cpu", "CPU"),
        ("microcode", "Microcode"),
        ("os", "Host OS"),
        ("profile", "Profile"),
        ("profile_digest", "Profile digest"),
        ("surface_digest", "CPU surface digest"),
        ("tsc_hz", "Declared TSC rate (Hz)"),
        ("lapic_hz", "LAPIC rate (Hz)"),
        ("measured_tsc_hz", "Measured TSC rate (Hz)"),
        ("rate_deviation_ppm", "Rate deviation from the backend (ppm)"),
        ("rate_agreement_ppm", "Interval rate agreement (ppm)"),
        ("rate_uncertainty_ppm", "Largest interval uncertainty (ppm)"),
        ("host_skew_ns", "Host skew (ns)"),
        ("guest_warp_ns", "Guest warp offset (ns)"),
        ("invariant_tsc", "Host invariant TSC (evidence)"),
        ("host_clocksource", "Host clocksource (evidence)"),
    )
    facts_line = " · ".join(
        f"{label}: `{facts[name]}`" for name, label in reported if name in facts
    )
    if facts_line:
        lines.extend(("", facts_line))
    return "\n".join(lines) + "\n\n"


def run(args: argparse.Namespace) -> int:
    if args.backend not in OPENVMM_TEST_BACKENDS:
        raise ScriptError(f"unsupported backend {args.backend!r}")
    checks = tuple(dict.fromkeys(args.checks or CHECK_IDS))
    if args.no_openvmm:
        booting = [check for check in checks if check in OPENVMM_CHECKS]
        if booting:
            raise ScriptError(
                f"--no-openvmm cannot run {' and '.join(booting)}, which boot OpenVMM"
            )
    probe_directory = args.probe_dir or default_probe_directory()
    fingerprint: Path | None = None
    if not args.no_openvmm:
        fingerprint = args.cpu_fingerprint or (
            probe_directory / f"nvx-cpu-fingerprint-{args.backend}.json"
        )
    context = DoctorContext(
        backend=args.backend,
        openvmm=args.openvmm or openvmm_binary_path(),
        kernel=args.kernel or artifact_path(KernelBuildConstants.BINARY_NAME),
        initrd=args.initrd or artifact_path(AlpineBuildConstants.INITRAMFS_NAME),
        openvmm_args=tuple(args.openvmm_arg),
        probe_directory=probe_directory,
        timeout=args.timeout,
        fingerprint=fingerprint,
        schedule=CI_SCHEDULE if args.ci_schedule else QUALIFICATION_SCHEDULE,
    )
    results = run_checks(context, checks)
    failed = [result.check for result in results if not result.passed]
    generation = context.facts.get("generation")
    profile = context.facts.get("profile")
    print(
        f"Time ABI host qualification on {args.backend}: "
        + ("passed" if not failed else f"failed ({', '.join(failed)})")
        + (f"; generation {generation}" if generation else "")
        + (f", profile {profile}" if profile else ""),
        flush=True,
    )
    if args.summary is not None:
        with args.summary.open("a", encoding="utf-8") as summary:
            summary.write(summary_markdown(args.backend, results, context.facts))
    return 1 if failed else 0


def default_probe_directory() -> Path:
    tool_cache = os.environ.get("RUNNER_TOOL_CACHE")
    if tool_cache:
        return Path(tool_cache) / "nvx-host-time-probe"
    return BuildConstants.BUILD_DIR / "host-time-probe"


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Qualify this host for the NVX time ABI (doc/design/time-abi.md, "
        "'Host qualification')."
    )
    parser.add_argument("--backend", choices=OPENVMM_TEST_BACKENDS, required=True)
    parser.add_argument(
        "--checks",
        nargs="+",
        choices=CHECK_IDS,
        metavar="ID",
        help="checks to run, in spec order (default: H1 to H7)",
    )
    parser.add_argument(
        "--openvmm", type=Path, help="OpenVMM binary for H2, H3, and H6"
    )
    parser.add_argument("--kernel", type=Path, help="guest kernel for H6")
    parser.add_argument("--initrd", type=Path, help="guest initramfs for H6")
    openvmm = parser.add_mutually_exclusive_group()
    openvmm.add_argument(
        "--cpu-fingerprint",
        type=Path,
        help="where H2 writes OpenVMM's CPU fingerprint "
        "(default: nvx-cpu-fingerprint-<backend>.json in the probe directory)",
    )
    openvmm.add_argument(
        "--no-openvmm",
        action="store_true",
        help="qualify without an OpenVMM binary: H2 checks the CPU identity and "
        "generation but not the CPU profile, and H3 and H6 cannot run",
    )
    parser.add_argument(
        "--ci-schedule",
        action="store_true",
        help="run H4 and H6 on CI's short schedules: H4 takes 3 samples 1 s "
        "apart instead of 13 samples 10 s apart, and H6 runs the warp probe "
        "twice instead of five times",
    )
    parser.add_argument(
        "--probe-dir",
        type=Path,
        help="cache directory for the host probe binary "
        "(default: $RUNNER_TOOL_CACHE or build/)",
    )
    parser.add_argument(
        "--summary",
        type=Path,
        help="append a Markdown summary, for example $GITHUB_STEP_SUMMARY",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=120.0,
        help="seconds allowed for each probe or guest (default: 120)",
    )
    parser.add_argument(
        "--openvmm-arg",
        action="append",
        default=[],
        help=argparse.SUPPRESS,
    )
    parser.set_defaults(handler=run)
