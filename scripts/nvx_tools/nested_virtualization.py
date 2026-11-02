"""Whether a host gives a microVM nested virtualization, as the scenarios of
nested virtualization find it independently of OpenVMM and of `nvx-rs`.

A microVM exposes Intel's VMX alone (doc/design/2.2-cpu-and-topology.md), on
KVM whose `kvm_intel` offers nested VMX and on WHP that offers it, and never on
MSHV. The scenarios decide from this probe which path a host must take: a
microVM with nested virtualization runs VMs of its own where the host gives it,
and OpenVMM and `nvx-rs` refuse it before the guest runs elsewhere, so that no
host skips the scenario.
"""

from __future__ import annotations

import ctypes
import platform
import re
import sys
from pathlib import Path
from typing import NamedTuple

# The `nested` parameter of `kvm_intel`, Y or 1 where KVM offers its guests
# VMX.
KVM_INTEL_NESTED = Path("/sys/module/kvm_intel/parameters/nested")
INTEL = "GenuineIntel"
# WHvGetCapability's code of the processor feature banks, and the bit of the
# second bank that says that WHP offers nested virtualization.
WHP_CAPABILITY_PROCESSOR_FEATURES_BANKS = 0x1006
WHP_NESTED_VIRT_SUPPORT = 1 << 6
# The processor that Windows reports, such as
# "Intel64 Family 6 Model 154 Stepping 3, GenuineIntel", ends with the CPUID
# vendor.
WINDOWS_PROCESSOR_VENDOR = re.compile(r", (\S+)$")
# What OpenVMM says when it refuses a microVM nested virtualization: on MSHV,
# for a CPU profile of AMD's CPUs, and where KVM or WHP does not offer it.
OPENVMM_MSHV_REFUSAL = "microVM nested virtualization requires KVM or WHP, not mshv"
OPENVMM_PROFILE_REFUSAL = "nested virtualization requires an Intel CPU profile"
OPENVMM_KVM_REFUSAL = (
    "nested virtualization was requested but the host does not support it"
)
OPENVMM_WHP_REFUSAL = "WHP does not support nested virtualization on this host"


class NestedRefusal(NamedTuple):
    """Why a host gives a microVM of a backend no nested virtualization, and
    the messages, any of which OpenVMM's refusal holds."""

    reason: str
    openvmm: tuple[str, ...]


def host_cpu_vendor() -> str | None:
    """Return the CPUID vendor of the host's CPU, as the host OS reports it,
    or None where it cannot be read."""
    if sys.platform == "win32":
        match = WINDOWS_PROCESSOR_VENDOR.search(platform.processor().strip())
        return match.group(1) if match is not None else None
    try:
        cpuinfo = Path("/proc/cpuinfo").read_text(encoding="utf-8")
    except OSError:
        return None
    for line in cpuinfo.splitlines():
        key, _, value = line.partition(":")
        if key.strip() == "vendor_id":
            return value.strip()
    return None


def whp_offers_nested_virtualization() -> bool:
    """Return whether WHP offers its partitions nested virtualization, as the
    second processor feature bank of WHvGetCapability says."""
    if sys.platform != "win32":
        return False
    try:
        library = ctypes.WinDLL("WinHvPlatform.dll")
    except OSError:
        return False

    class FeatureBanks(ctypes.Structure):
        _fields_ = [
            ("banks_count", ctypes.c_uint32),
            ("reserved", ctypes.c_uint32),
            ("banks", ctypes.c_uint64 * 2),
        ]

    banks = FeatureBanks()
    written = ctypes.c_uint32(0)
    result = int(
        library.WHvGetCapability(
            WHP_CAPABILITY_PROCESSOR_FEATURES_BANKS,
            ctypes.byref(banks),
            ctypes.sizeof(banks),
            ctypes.byref(written),
        )
    )
    return result == 0 and bool(banks.banks[1] & WHP_NESTED_VIRT_SUPPORT)


def host_refusal(
    backend: str,
    *,
    vendor: str | None = None,
    kvm_nested: str | None = None,
    whp_nested: bool | None = None,
) -> NestedRefusal | None:
    """Return why this host gives a microVM of ``backend`` no nested
    virtualization, or None where it gives it.

    The CPU's vendor, ``kvm_intel``'s ``nested`` parameter, and WHP's feature,
    which the host reports unless given, decide: MSHV never gives it, a CPU
    other than Intel's never does, and KVM and WHP give it where they offer
    it."""
    if backend == "mshv":
        return NestedRefusal(
            "MSHV offers nested virtualization only with the Hyper-V guest "
            "interface, which the time ABI excludes",
            (OPENVMM_MSHV_REFUSAL,),
        )
    if vendor is None:
        vendor = host_cpu_vendor()
    if vendor != INTEL:
        offered = OPENVMM_KVM_REFUSAL if backend == "kvm" else OPENVMM_WHP_REFUSAL
        return NestedRefusal(
            f"the host's CPU is {vendor or 'unknown'}, and a microVM exposes "
            "Intel's VMX alone",
            (OPENVMM_PROFILE_REFUSAL, offered),
        )
    if backend == "kvm":
        if kvm_nested is None:
            try:
                kvm_nested = KVM_INTEL_NESTED.read_text(encoding="utf-8").strip()
            except OSError:
                kvm_nested = ""
        if kvm_nested in ("Y", "1"):
            return None
        return NestedRefusal(
            f"{KVM_INTEL_NESTED} is {kvm_nested or 'unreadable'}",
            (OPENVMM_KVM_REFUSAL,),
        )
    if backend == "whp":
        if whp_nested is None:
            whp_nested = whp_offers_nested_virtualization()
        if whp_nested:
            return None
        return NestedRefusal(
            "WHP offers no nested virtualization", (OPENVMM_WHP_REFUSAL,)
        )
    raise ValueError(f"unsupported backend {backend!r}")
