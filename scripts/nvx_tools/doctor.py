"""Host qualification for the NVX time ABI (``nvx.py doctor``).

doc/ci.md ("Host qualification") describes checks H1 to H7. Each
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
import math
import os
import platform
import re
import shutil
import subprocess
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path

from .benchmark import run_guest_script, workload_boot_command
from .build_constants import AlpineBuildConstants, BuildConstants, KernelBuildConstants
from .ci import OPENVMM_TEST_BACKENDS
from .common import (
    ScriptError,
    artifact_path,
    openvmm_binary_path,
    require_tool,
)
from .time_abi import (
    CI_WARP_GAPS,
    LAPIC_HZ,
    MAX_TSC_HZ,
    MIN_TSC_HZ,
    QUALIFICATION_WARP_GAPS,
    WARP_BOUND_NS,
    WARP_PROBE_COMPLETION_MARKER,
    HostCpu,
    TimeAbiFailure,
    TimeAbiMonitor,
    check_warp_probe,
    cpu_generation,
    describe_cpu_generations,
    is_catalog_profile_id,
    is_host_profile_id,
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
# H4's rate stability bounds (doc/ci.md, "Host qualification").
RATE_UNCERTAINTY_PPM = 0.25
RATE_AGREEMENT_PPM = 1.0
# H4's limit is the guest's budget for the host's clock correction. The
# guest's wall-clock discipline follows host UTC, so it applies that
# correction on top of a restore's rate deviation, which the time ABI's
# restore tolerance bounds, within Linux's frequency limit
# (doc/design/3.2-clock-and-time.md, "Wall-clock discipline").
GUEST_FREQUENCY_LIMIT_PPM = 500.0
RESTORE_TOLERANCE_PPM = 250.0
RATE_TOLERANCE_PPM = GUEST_FREQUENCY_LIMIT_PPM - RESTORE_TOLERANCE_PPM
# H4 warns above the time ABI v1's limit, which bounded the TSC's error alone:
# one run can't tell that error from the wander of the host's reference clock.
RATE_WARNING_PPM = 100.0
# chronyd's tracking report, `chronyc -c tracking`, has 14 comma-separated
# fields, among them the reference's name (1), the frequency in ppm, positive
# when the undisciplined clock runs fast (7), its skew in ppm (9), and the
# leap status (13). chronyc asks chronyd over localhost UDP without privileges.
CHRONY_TRACKING_FIELDS = 14
CHRONY_UNSYNCHRONIZED = "Not synchronised"
CHRONYC_TIMEOUT_SECONDS = 5.0
# When chronyd's skew leaves H4's range straddling the limit, H4 re-reads
# chronyd's estimate this often, for at most this long, and uses the first
# estimate whose range is conclusive.
CHRONY_REREAD_INTERVAL_SECONDS = 2.0
CHRONY_REREAD_SECONDS = 60.0
SKEW_PAIR_DURATION_MS = 5
GUEST_VCPU_COUNTS = (8, 4, 2, 1)
GUEST_MEMORY_MIB = 128


@dataclass(frozen=True)
class Schedule:
    """How long H4 samples the TSC rate and how often H6 runs the warp probe."""

    rate_samples: int
    rate_interval_ms: int
    warp_gaps: tuple[str, ...]


# H4's rate samples and H6's warp gaps (doc/ci.md, "Host qualification"):
# doctor qualifies on the long schedules, and CI on the short ones.
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
# The processor that Windows reports, such as
# "Intel64 Family 6 Model 154 Stepping 3, GenuineIntel": the display family,
# model, and stepping, and the CPUID vendor.
WINDOWS_PROCESSOR = re.compile(r"Family (\d+) Model (\d+) Stepping (\d+), (\S+)")
# The registry key of the first processor's identity on Windows.
WINDOWS_PROCESSOR_KEY = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0"
ADJTIMEX_TIME_ERROR = 5
ADJTIMEX_STA_PLL = 0x0001
ADJTIMEX_STA_UNSYNC = 0x0040
# adjtimex's frequency is in ppm scaled by 2^16, and its tick is the
# microseconds that the clock advances per USER_HZ tick, 100 per second, so
# 10,000 leaves the clock's rate alone.
ADJTIMEX_SCALED_PPM = 1 << 16
ADJTIMEX_NOMINAL_TICK = 10_000
# Every H7 failure to read the host's synchronization state starts with this
# phrase; a clock that is not synchronized starts "host UTC is not
# synchronized" instead.
UTC_UNVERIFIED = "cannot verify host UTC synchronization"
WHP_CAPABILITY_HYPERVISOR_PRESENT = 0


@dataclass
class CheckResult:
    check: str
    passed: bool
    detail: str
    # A concern that doesn't fail the check, which the detail also states.
    warning: str = ""

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


def _annotation(text: str) -> str:
    """Escape text for the message of a GitHub Actions workflow command."""
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


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
    rustc = require_tool("rustc", "rustc is required to build the host time probe")
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
    context: DoctorContext, *arguments: str, duration: float = 0.0
) -> list[tuple[str, dict[str, str]]]:
    """Run the host probe and parse its ``NVX-HOST-TIME-PROBE`` records.

    ``duration`` is the time the probe spends sleeping by design, which the
    timeout allows on top of ``context.timeout``.
    """
    if context.probe is None:
        context.probe = build_probe(context.probe_directory)
    completed = subprocess.run(
        [os.fspath(context.probe), *arguments],
        capture_output=True,
        text=True,
        timeout=context.timeout + duration,
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


def host_cpu_signature() -> HostCpu | None:
    """Return the host CPU's vendor and display family, model, and stepping, as
    the host OS reports them, with its brand string where the OS reports one,
    or None where they cannot be read."""
    try:
        if host_is_windows():
            match = WINDOWS_PROCESSOR.search(platform.processor())
            if match is None:
                return None
            family, model, stepping, vendor = match.groups()
            try:
                brand = str(
                    _windows_registry_value(
                        WINDOWS_PROCESSOR_KEY, "ProcessorNameString"
                    )
                ).strip()
            except (OSError, ScriptError):
                brand = ""
        else:
            cpuinfo = _linux_cpuinfo()
            vendor = cpuinfo["vendor_id"]
            family = cpuinfo["cpu family"]
            model = cpuinfo["model"]
            stepping = cpuinfo["stepping"]
            brand = cpuinfo.get("model name", "")
        return HostCpu(vendor, int(family), int(model), int(stepping), brand)
    except (OSError, KeyError, ValueError):
        return None


def host_cpu(context: DoctorContext) -> dict[str, str]:
    """Collect the host CPU identity and the host OS's view of its TSC."""
    if host_is_windows():
        match = WINDOWS_PROCESSOR.search(platform.processor())
        if match is None:
            raise ScriptError(f"cannot parse the processor {platform.processor()!r}")
        family, model, stepping, vendor = match.groups()
        info = {
            "vendor": vendor,
            "family": family,
            "model": model,
            "stepping": stepping,
            "brand": str(
                _windows_registry_value(WINDOWS_PROCESSOR_KEY, "ProcessorNameString")
            ).strip(),
        }
        revision = _windows_registry_value(WINDOWS_PROCESSOR_KEY, "Update Revision")
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
    context.facts.update(
        {
            "cpu": signature,
            "brand": info["brand"],
            "microcode": info.get("microcode", "unknown"),
            "os": info["os"],
            "invariant_tsc": info["invariant_tsc"],
        }
    )
    problems: list[str] = []
    if context.fingerprint is None:
        # Without OpenVMM, NVX's copy of OpenVMM's catalog maps the host.
        try:
            generation = cpu_generation(
                info["vendor"],
                int(info["family"]),
                int(info["model"]),
                int(info["stepping"]),
            )
        except ValueError:
            generation = None
        name = generation.name if generation else "unknown"
        profile = generation.profile_id if generation else "none"
        if generation is None:
            problems.append(
                f"[E_PROFILE_HOST_UNKNOWN] {signature} is not a time ABI generation "
                f"({describe_cpu_generations()})"
            )
        profile_detail = "CPU profile not checked: qualification runs without OpenVMM"
    else:
        name, profile, profile_detail, profile_problems = _check_cpu_profile(context)
        problems.extend(profile_problems)
    context.facts.update({"generation": name, "profile": profile})
    detail = (
        f"generation={name} profile={profile} cpu={signature} "
        f"microcode={info.get('microcode', 'unknown')} os={info['os']} "
        f"invariant_tsc={info['invariant_tsc']} brand={info['brand']}; "
        f"{profile_detail}"
    )
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
    lineage = re.escape(expected.rpartition(".v")[0])
    return re.fullmatch(rf"{lineage}\.v\d+", selected) is not None


def _check_cpu_profile(context: DoctorContext) -> tuple[str, str, str, list[str]]:
    """Fingerprint the host with OpenVMM and check the profile that it
    selects. Return the generation and the profile as OpenVMM reports them,
    the detail, and the problems: OpenVMM's catalog, not NVX's copy, decides
    which hosts it serves."""
    assert context.fingerprint is not None
    if not context.openvmm.is_file():
        return (
            "unknown",
            "none",
            "CPU profile not checked",
            [
                f"OpenVMM was not found at {context.openvmm}; H2 checks the CPU "
                "profile with its --cpu-fingerprint tool (or pass --no-openvmm)"
            ],
        )
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
        return (
            "unknown",
            "none",
            "CPU profile not checked",
            [
                f"OpenVMM's CPU fingerprint exited {completed.returncode} without "
                f"an {CPU_PROFILE_PREFIX} line: {last}"
            ],
        )
    fields = parse_cpu_profile_line(line)
    generation = fields.get("generation") or "none"
    profile = fields.get("profile") or "none"
    for name in ("profile_digest", "surface_digest"):
        if fields.get(name, "none") != "none":
            context.facts[name] = fields[name]
    context.facts["cpu_fingerprint"] = os.fspath(context.fingerprint)
    detail = (
        f"CPU profile check status={fields.get('status')} "
        f"profile={profile} "
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
        return generation, profile, detail, problems
    if fields.get("backend") != context.backend:
        problems.append(f"OpenVMM fingerprinted the {fields.get('backend')} backend")
    unreported = [name for name in ("generation", "profile") if not fields.get(name)]
    if unreported:
        problems.append(
            "OpenVMM's CPU profile check passed without reporting its "
            + " and ".join(unreported)
        )
    elif is_host_profile_id(profile):
        problems.append(
            f"OpenVMM reported host profile {profile}, which qualification "
            "never accepts"
        )
    elif not is_catalog_profile_id(profile):
        problems.append(
            f"OpenVMM reported CPU profile {profile}, which is not a catalog "
            "profile ID (vendor.generation.vN)"
        )
    elif profile.split(".")[1] != generation:
        problems.append(
            f"OpenVMM reported CPU profile {profile} of generation "
            f"{profile.split('.')[1]} for generation {generation}"
        )
    return generation, profile, detail, problems


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
    selected_profile = fields.get("cpu_profile") or "none"
    # A missing, host, or malformed profile fails even when H2 doesn't run
    # with H3. Which generations have profiles is OpenVMM's to decide.
    if not is_catalog_profile_id(selected_profile):
        problems.append(
            "OpenVMM verified no catalog CPU profile: "
            f"cpu_profile={selected_profile}"
            + (
                " (a host profile, to which --cpu-profile auto falls back on a "
                "CPU that no built-in profile serves, and which qualification "
                "never accepts)"
                if is_host_profile_id(selected_profile)
                else ""
            )
        )
    elif expected_profile != "none" and not _same_profile_lineage(
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


@dataclass(frozen=True)
class ChronyFrequency:
    """chronyd's estimate of how fast the undisciplined clock runs."""

    reference: str
    ppm: float
    skew_ppm: float


def chrony_frequency() -> ChronyFrequency | str:
    """Return a synchronized chronyd's frequency estimate, or why there is none."""
    chronyc = shutil.which("chronyc")
    if chronyc is None:
        return "chronyc is not installed"
    try:
        completed = subprocess.run(
            [chronyc, "-c", "-n", "tracking"],
            capture_output=True,
            text=True,
            timeout=CHRONYC_TIMEOUT_SECONDS,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return f"chronyc tracking failed: {error}"
    report = completed.stdout.strip()
    if completed.returncode != 0:
        output = (completed.stderr.strip() or report).splitlines()
        return (
            f"chronyc tracking exited {completed.returncode}: "
            f"{output[-1] if output else 'no output'}"
        )
    fields = report.split(",")
    if len(fields) == CHRONY_TRACKING_FIELDS and fields[13] == CHRONY_UNSYNCHRONIZED:
        return "chronyd is not synchronized"
    try:
        ppm, skew_ppm = float(fields[7]), float(fields[9])
    except (IndexError, ValueError):
        ppm = skew_ppm = math.nan
    if (
        len(fields) != CHRONY_TRACKING_FIELDS
        or not math.isfinite(ppm)
        or not math.isfinite(skew_ppm)
        or skew_ppm < 0
    ):
        return f"chronyc tracking printed an unrecognized report: {report[:200]!r}"
    return ChronyFrequency(fields[1], ppm, skew_ppm)


def reread_chrony_frequency(
    conclusive: Callable[[ChronyFrequency], bool],
) -> tuple[ChronyFrequency | None, float]:
    """Re-read chronyd's frequency estimate until one is ``conclusive``.

    Reads it every CHRONY_REREAD_INTERVAL_SECONDS for at most
    CHRONY_REREAD_SECONDS, and returns the first conclusive estimate, or None,
    with the seconds that the re-reads took.
    """
    start = time.monotonic()
    while (
        time.monotonic() - start + CHRONY_REREAD_INTERVAL_SECONDS
        <= CHRONY_REREAD_SECONDS
    ):
        time.sleep(CHRONY_REREAD_INTERVAL_SECONDS)
        estimate = chrony_frequency()
        if isinstance(estimate, ChronyFrequency) and conclusive(estimate):
            return estimate, time.monotonic() - start
    return None, time.monotonic() - start


class _Timex(ctypes.Structure):
    """Linux's struct timex, as glibc declares it."""

    _fields_ = [
        ("modes", ctypes.c_uint),
        ("offset", ctypes.c_long),
        ("freq", ctypes.c_long),
        ("maxerror", ctypes.c_long),
        ("esterror", ctypes.c_long),
        ("status", ctypes.c_int),
        ("constant", ctypes.c_long),
        ("precision", ctypes.c_long),
        ("tolerance", ctypes.c_long),
        ("time", ctypes.c_long * 2),  # struct timeval
        ("tick", ctypes.c_long),
        ("ppsfreq", ctypes.c_long),
        ("jitter", ctypes.c_long),
        ("shift", ctypes.c_int),
        ("stabil", ctypes.c_long),
        ("jitcnt", ctypes.c_long),
        ("calcnt", ctypes.c_long),
        ("errcnt", ctypes.c_long),
        ("stbcnt", ctypes.c_long),
        ("tai", ctypes.c_int),
        ("padding", ctypes.c_int * 11),
    ]


def _adjtimex() -> tuple[int, _Timex]:
    """Return adjtimex's clock state and struct timex without changing the
    clock, or raise ScriptError naming why adjtimex can't."""
    timex = _Timex()
    try:
        libc = ctypes.CDLL(None, use_errno=True)
        state = int(libc.adjtimex(ctypes.byref(timex)))
    except (AttributeError, OSError) as error:
        raise ScriptError(f"adjtimex is unavailable: {error}") from error
    if state < 0:
        raise ScriptError(f"adjtimex failed: {os.strerror(ctypes.get_errno())}")
    return state, timex


@dataclass(frozen=True)
class KernelFrequency:
    """The frequency correction of the kernel's PLL in ppm, positive when it
    speeds the clock up: adjtimex's frequency plus its tick's."""

    ppm: float


def kernel_frequency() -> KernelFrequency | str:
    """Return a synchronized kernel PLL's frequency correction, or why there is
    none.

    A time service such as systemd-timesyncd or ntpd steers the clock through
    the kernel's PLL, which keeps its frequency estimate in adjtimex's
    frequency and slews offsets apart from it. chronyd keeps the PLL off and
    slews through that frequency, which then carries its slews.
    """
    try:
        state, timex = _adjtimex()
    except ScriptError as error:
        return str(error)
    status = int(timex.status)
    if state >= ADJTIMEX_TIME_ERROR or status & ADJTIMEX_STA_UNSYNC:
        return (
            f"the kernel clock is not synchronized (adjtimex state={state} "
            f"status=0x{status:04x})"
        )
    if not status & ADJTIMEX_STA_PLL:
        return f"the kernel PLL is off (adjtimex status=0x{status:04x})"
    tick = int(timex.tick)
    return KernelFrequency(
        int(timex.freq) / ADJTIMEX_SCALED_PPM
        + (tick - ADJTIMEX_NOMINAL_TICK) * 1e6 / ADJTIMEX_NOMINAL_TICK
    )


@dataclass(frozen=True)
class BootCorrection:
    """The time service's frequency correction averaged since boot: how fast
    CLOCK_MONOTONIC_RAW ran against CLOCK_MONOTONIC, in ppm, as chronyd's
    estimate says how fast it runs now, over ``seconds``."""

    ppm: float
    seconds: float


def boot_correction() -> BootCorrection | None:
    """Return the time service's frequency correction averaged since boot, or
    None where the host has no CLOCK_MONOTONIC_RAW.

    Both clocks exclude suspend, and after a small offset at boot only the
    time service's gradual adjustments separate them, so their difference
    over the uptime averages the reference's wander out without saved state.
    Slews just after boot or a live migration bias it.
    """
    if sys.platform == "win32":
        return None
    raw = time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW)
    monotonic = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
    if raw <= 0:
        return None
    return BootCorrection((raw - monotonic) / raw * 1e6, raw / 1e9)


def _deviation_ppm(rate: float, declared_hz: int) -> float:
    return (rate - declared_hz) / declared_hz * 1e6


def _corrected_rate(raw_rate: float, ppm: float) -> float:
    """Return the rate against true time of a rate against
    CLOCK_MONOTONIC_RAW, which a time service estimates runs ``ppm`` fast."""
    # chronyd sets the kernel's frequency to -ppm, so its estimate of true
    # time runs at CLOCK_MONOTONIC_RAW's rate times (1 - ppm).
    return raw_rate / (1 - ppm / 1e6)


def _chrony_deviations(
    raw_rate: float, declared_hz: int, chrony: ChronyFrequency
) -> tuple[float, float]:
    """Return the nearest and the farthest deviation from ``declared_hz`` of
    the rates within chronyd's skew of its estimate."""
    # chronyd's skew bounds the error of its frequency estimate, so the rate
    # lies between the rates that the estimate's bounds give.
    low, high = (
        _deviation_ppm(_corrected_rate(raw_rate, chrony.ppm + bound), declared_hz)
        for bound in (-chrony.skew_ppm, chrony.skew_ppm)
    )
    nearest = 0.0 if low <= 0 <= high else min(abs(low), abs(high))
    return nearest, max(abs(low), abs(high))


def check_rate(context: DoctorContext) -> CheckResult:
    clocksource = ""
    if not host_is_windows():
        try:
            clocksource = host_clocksource()
        except OSError as error:
            # The host clocksource is recorded as evidence only; an
            # unreadable file must not fail the check. Record it explicitly
            # and still run the TSC rate/stability measurements below.
            clocksource = f"unknown ({error})"
        context.facts["host_clocksource"] = clocksource
    schedule = context.schedule
    # The probe sleeps between samples for the whole window, 120 s on the
    # qualification schedule, so the timeout covers the window as well.
    records = run_probe(
        context,
        "rate",
        "--samples",
        str(schedule.rate_samples),
        "--interval-ms",
        str(schedule.rate_interval_ms),
        duration=(schedule.rate_samples - 1) * schedule.rate_interval_ms / 1000,
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
    window_rate = _window_rate(
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
    declared = context.facts.get("native_tsc_hz") or context.facts.get("tsc_hz")
    declared_hz = None if declared is None else int(declared)
    # Over the window, CLOCK_MONOTONIC also carries the slew with which
    # chronyd removes its current offset, which reaches hundreds of ppm for
    # seconds after its reference jumps (#768). chronyd's frequency estimate,
    # from its whole sample history, says instead how fast
    # CLOCK_MONOTONIC_RAW runs against the reference.
    chrony = None if host_is_windows() else chrony_frequency()
    reread_problem = reread_evidence = ""
    if isinstance(chrony, ChronyFrequency) and declared_hz is not None:

        def conclusive(estimate: ChronyFrequency) -> bool:
            nearest, farthest = _chrony_deviations(stable_rate, declared_hz, estimate)
            return farthest <= RATE_TOLERANCE_PPM or nearest > RATE_TOLERANCE_PPM

        if not conclusive(chrony):
            # The reference's wander moves chronyd's estimate and its skew
            # within seconds (#1008). Only the estimate changes, so the
            # samples stand while H4 re-reads it.
            estimate, seconds = reread_chrony_frequency(conclusive)
            if estimate is None:
                reread_problem = (
                    f", and no re-read of chronyd's estimate in {seconds:.0f} s "
                    "was conclusive"
                )
            else:
                reread_evidence = (
                    f"; chronyd's first estimate, {chrony.ppm:+.3f} ppm (skew "
                    f"{chrony.skew_ppm:.3f} ppm), straddled the limit, so H4 "
                    f"re-read it for {seconds:.0f} s (evidence)"
                )
                chrony = estimate
    # Without chronyd's estimate, a kernel PLL that another time service
    # drives keeps its own, apart from the offsets that it slews.
    kernel = kernel_frequency() if isinstance(chrony, str) else None
    if isinstance(chrony, ChronyFrequency):
        measured = _corrected_rate(stable_rate, chrony.ppm)
        reference = (
            f"against {stability_clock['name']} corrected by chronyd's frequency "
            f"estimate of {chrony.ppm:+.3f} ppm (skew {chrony.skew_ppm:.3f} ppm, "
            f"reference {chrony.reference})"
        )
        context.facts["rate_reference"] = (
            f"chronyd {chrony.ppm:+.3f} ppm (skew {chrony.skew_ppm:.3f} ppm)"
        )
    elif isinstance(kernel, KernelFrequency):
        # The PLL runs the kernel's clock at CLOCK_MONOTONIC_RAW's rate times
        # (1 + ppm), as chronyd's estimate of -ppm would.
        measured = _corrected_rate(stable_rate, -kernel.ppm)
        reference = (
            f"against {stability_clock['name']} corrected by the kernel PLL's "
            f"frequency of {kernel.ppm:+.3f} ppm"
        )
        context.facts["rate_reference"] = f"kernel PLL {kernel.ppm:+.3f} ppm"
    else:
        measured = window_rate
        reference = f"against {rate_clock['name']}"
        context.facts["rate_reference"] = rate_clock["name"]
    context.facts["measured_tsc_hz"] = f"{measured:.0f}"
    context.facts["rate_agreement_ppm"] = f"{agreement_ppm:.3f}"
    context.facts["rate_uncertainty_ppm"] = f"{uncertainty_ppm:.3f}"
    detail = (
        f"tsc_hz={measured:.0f} {reference} from {len(samples)} "
        f"samples {schedule.rate_interval_ms / 1000:g} s apart; against "
        f"{stability_clock['name']} (resolution "
        f"{float(stability_clock['resolution_ns']):g} ns) the interval rates agree "
        f"within {agreement_ppm:.3f} ppm, largest interval uncertainty "
        f"{uncertainty_ppm:.3f} ppm"
    )
    problems: list[str] = []
    warning = ""
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
    if declared_hz is None:
        # validate-runner runs before the OpenVMM binary is available; the
        # VM jobs run H3 with H4 to compare against the backend's rate.
        detail += "; not compared with the backend's rate (H3 did not run)"
    else:
        deviation_ppm = abs(_deviation_ppm(measured, declared_hz))
        context.facts["rate_deviation_ppm"] = f"{deviation_ppm:.3f}"
        detail += f"; {deviation_ppm:.3f} ppm from the backend's {declared} Hz"
        if isinstance(chrony, ChronyFrequency):
            # The rate passes only when the whole range within chronyd's skew
            # lies within the limit.
            nearest, farthest = _chrony_deviations(stable_rate, declared_hz, chrony)
            within = (
                f", {nearest:.3f} to {farthest:.3f} ppm within chronyd's skew of "
                f"{chrony.skew_ppm:.3f} ppm"
            )
        else:
            nearest = farthest = deviation_ppm
            within = ""
        if nearest > RATE_TOLERANCE_PPM:
            problems.append(
                f"the backend rate is {deviation_ppm:.3f} ppm from the measured "
                f"rate{within} (limit {RATE_TOLERANCE_PPM:g} ppm)"
            )
        elif farthest > RATE_TOLERANCE_PPM:
            problems.append(
                f"the measurement is inconclusive: the backend rate is "
                f"{deviation_ppm:.3f} ppm from the measured rate{within}, which "
                f"straddles the limit of {RATE_TOLERANCE_PPM:g} ppm{reread_problem}"
            )
        elif deviation_ppm > RATE_WARNING_PPM:
            warning = (
                f"the backend rate is {deviation_ppm:.3f} ppm from the measured "
                f"rate, more than {RATE_WARNING_PPM:g} ppm: the host's clock "
                f"correction uses {deviation_ppm / RATE_TOLERANCE_PPM:.0%} of the "
                f"guest's {RATE_TOLERANCE_PPM:g} ppm budget, and one run can't "
                "tell the TSC's error from the reference clock's wander"
            )
    detail += reread_evidence
    if isinstance(chrony, ChronyFrequency):
        detail += (
            f"; {window_rate:.0f} Hz against {rate_clock['name']}, which chronyd "
            "also slews (evidence)"
        )
    elif chrony is not None:
        detail += f"; no chronyd frequency estimate: {chrony} (evidence)"
        if isinstance(kernel, KernelFrequency):
            detail += (
                f"; {window_rate:.0f} Hz against {rate_clock['name']}, which the "
                "kernel PLL also slews (evidence)"
            )
        elif kernel is not None:
            detail += f"; no kernel PLL frequency: {kernel} (evidence)"
    boot = None if host_is_windows() else boot_correction()
    if boot is not None:
        hours = boot.seconds / 3600
        boot_rate = _corrected_rate(stable_rate, boot.ppm)
        detail += (
            f"; {boot_rate:.0f} Hz against {stability_clock['name']} corrected by "
            f"the average correction since boot, {boot.ppm:+.3f} ppm over "
            f"{hours:.1f} h"
        )
        if declared_hz is not None:
            boot_deviation = abs(_deviation_ppm(boot_rate, declared_hz))
            detail += f", {boot_deviation:.3f} ppm from the backend's rate"
        detail += " (evidence)"
        context.facts["boot_correction"] = f"{boot.ppm:+.3f} ppm over {hours:.1f} h"
    if clocksource:
        detail += f"; host clocksource {clocksource} (evidence)"
    if warning:
        detail = f"warning: {warning}; {detail}"
    if problems:
        return CheckResult("H4", False, "; ".join(problems) + f"; {detail}", warning)
    return CheckResult("H4", True, detail, warning)


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
        # time, which CHECK_CPU_BUDGET_US budgets; both are evidence only.
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
    try:
        state, timex = _adjtimex()
    except ScriptError as error:
        raise ScriptError(f"{UTC_UNVERIFIED}: {error}") from error
    return state, int(timex.status)


def _windows_time_source() -> tuple[bool, str]:
    try:
        completed = subprocess.run(
            ["w32tm", "/query", "/status"],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.SubprocessError, ValueError) as error:
        raise ScriptError(f"{UTC_UNVERIFIED}: w32tm /query /status: {error}") from error
    if completed.returncode != 0:
        raise ScriptError(
            f"{UTC_UNVERIFIED}: w32tm /query /status failed: "
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
            f"{UTC_UNVERIFIED}: w32tm /query /status reported "
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
    if state > ADJTIMEX_TIME_ERROR:
        return CheckResult(
            "H7", False, f"{UTC_UNVERIFIED}: unknown clock state; {detail}"
        )
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
    """Run checks in ID order; an exception fails only its own check."""
    results: list[CheckResult] = []
    for check in CHECK_IDS:
        if check not in checks:
            continue
        try:
            result = CHECKS[check](context)
        except (
            ArithmeticError,
            IndexError,
            KeyError,
            OSError,
            RuntimeError,
            subprocess.SubprocessError,
            TypeError,
            ValueError,
        ) as error:
            first = str(error).splitlines()[0] if str(error) else type(error).__name__
            result = CheckResult(check, False, first)
        results.append(result)
        print(result.line(), flush=True)
        if result.warning and os.environ.get("GITHUB_ACTIONS") == "true":
            print(
                f"::warning title=Time ABI host qualification ({context.backend})::"
                + _annotation(f"{check} {CHECK_TITLES[check]}: {result.warning}"),
                flush=True,
            )
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
    for result in results:
        if result.warning:
            lines.extend(
                ("", f"**{result.check} warning:** {_escape_markdown(result.warning)}")
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
        ("rate_reference", "Rate reference"),
        ("rate_deviation_ppm", "Rate deviation from the backend (ppm)"),
        ("rate_agreement_ppm", "Interval rate agreement (ppm)"),
        ("rate_uncertainty_ppm", "Largest interval uncertainty (ppm)"),
        ("boot_correction", "Average correction since boot (evidence)"),
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
        "Qualify this host for the NVX time ABI (doc/ci.md, 'Host qualification')."
    )
    parser.add_argument("--backend", choices=OPENVMM_TEST_BACKENDS, required=True)
    parser.add_argument(
        "--checks",
        nargs="+",
        choices=CHECK_IDS,
        metavar="ID",
        help="checks to run, in ID order (default: H1 to H7)",
    )
    parser.add_argument(
        "--openvmm", type=Path, help="OpenVMM binary for H2, H3, and H6"
    )
    parser.add_argument("--kernel", type=Path, help="guest kernel for H3 and H6")
    parser.add_argument("--initrd", type=Path, help="guest initramfs for H3 and H6")
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
        help="seconds allowed for each probe or guest, on top of H4's sampling "
        "window (default: 120)",
    )
    parser.add_argument(
        "--openvmm-arg",
        action="append",
        default=[],
        help=argparse.SUPPRESS,
    )
    parser.set_defaults(handler=run)
