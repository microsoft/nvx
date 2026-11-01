//! The x86-64 instructions that the time tools execute, through the `core::arch` intrinsics.
//!
//! The intrinsics compile to the single instructions that their names give: `CPUID`, `RDTSC`,
//! `RDTSCP`, and `LFENCE`. `LFENCE` also orders the compiler's memory accesses, so a fenced read
//! of the time-stamp counter stays where the caller put it. Each intrinsic is safe to execute in
//! user mode on every x86-64 processor, except `RDTSCP`, which callers use only when
//! [`has_rdtscp`] reports it. Some of the intrinsics became safe functions after the crate's
//! minimum Rust version, so their calls allow an unused `unsafe` block.

use core::arch::x86_64::{__cpuid_count, __rdtscp, _mm_lfence, _rdtsc};

pub(crate) use core::arch::x86_64::CpuidResult;

/// CPUID leaves of the architecture and of Hyper-V, and the bits of their registers, that the
/// time tools read.
pub(crate) mod cpuid {
    use super::CpuidResult;

    /// A CPUID result whose four registers are zero.
    pub(crate) const ZERO: CpuidResult = CpuidResult {
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
    };

    /// One of the four registers of a CPUID result.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Register {
        /// EAX.
        Eax,
        /// EBX.
        Ebx,
        /// ECX.
        Ecx,
        /// EDX.
        Edx,
    }

    impl Register {
        /// The four registers, in the order in which CPUID results print them.
        pub(crate) const ALL: [Self; 4] = [Self::Eax, Self::Ebx, Self::Ecx, Self::Edx];

        /// Returns the register's value in `result`.
        pub(crate) const fn of(self, result: CpuidResult) -> u32 {
            match self {
                Self::Eax => result.eax,
                Self::Ebx => result.ebx,
                Self::Ecx => result.ecx,
                Self::Edx => result.edx,
            }
        }

        /// Returns the register's name, such as `EAX`.
        pub(crate) const fn name(self) -> &'static str {
            match self {
                Self::Eax => "EAX",
                Self::Ebx => "EBX",
                Self::Ecx => "ECX",
                Self::Edx => "EDX",
            }
        }
    }

    /// Basic leaf whose EAX is the highest basic leaf.
    pub(crate) const LEAF_MAXIMUM: u32 = 0;
    /// Basic leaf of the processor's signature and feature flags.
    pub(crate) const LEAF_FEATURES: u32 = 1;
    /// Basic leaf of thermal and power management, whose EAX bit 2 is ARAT.
    pub(crate) const LEAF_POWER_MANAGEMENT: u32 = 6;
    /// Basic leaf of the structured extended features, read with subleaf 0.
    pub(crate) const LEAF_STRUCTURED_FEATURES: u32 = 7;
    /// Basic leaf of architectural performance monitoring.
    pub(crate) const LEAF_PERFORMANCE_MONITORING: u32 = 0x0a;
    /// Basic leaf of the time-stamp counter's ratio to the core crystal clock.
    pub(crate) const LEAF_TSC_CRYSTAL: u32 = 0x15;
    /// Basic leaf of the processor's base, maximum, and bus frequencies.
    pub(crate) const LEAF_FREQUENCY: u32 = 0x16;
    /// Extended leaf whose EAX is the highest extended leaf.
    pub(crate) const LEAF_EXTENDED_MAXIMUM: u32 = 0x8000_0000;
    /// Extended leaf of the extended feature flags.
    pub(crate) const LEAF_EXTENDED_FEATURES: u32 = 0x8000_0001;
    /// Extended leaf of advanced power management, whose EDX bit 8 is the invariant TSC.
    pub(crate) const LEAF_ADVANCED_POWER: u32 = 0x8000_0007;
    /// First leaf of the hypervisor range, whose EAX is the highest hypervisor leaf and whose
    /// EBX, ECX, and EDX hold the hypervisor's vendor signature.
    pub(crate) const LEAF_HYPERVISOR: u32 = 0x4000_0000;
    /// Hyper-V leaf of the partition's privileges and features.
    pub(crate) const LEAF_HYPERV_FEATURES: u32 = 0x4000_0003;

    /// Leaf 1 ECX bit set when the processor runs under a hypervisor.
    pub(crate) const FEATURES_ECX_HYPERVISOR: u32 = 31;
    /// Leaf 1 ECX bit of the local APIC's TSC-deadline timer mode.
    pub(crate) const FEATURES_ECX_TSC_DEADLINE: u32 = 24;
    /// Leaf 1 ECX bit of the performance and debug capability MSR.
    pub(crate) const FEATURES_ECX_PDCM: u32 = 15;
    /// Leaf 1 EDX bit of the time-stamp counter.
    pub(crate) const FEATURES_EDX_TSC: u32 = 4;
    /// Leaf 6 EAX bit of the always-running APIC timer.
    pub(crate) const POWER_MANAGEMENT_EAX_ARAT: u32 = 2;
    /// Leaf 6 EAX bit of hardware-controlled performance states.
    pub(crate) const POWER_MANAGEMENT_EAX_HWP: u32 = 7;
    /// Leaf 6 ECX bit of the `APERF` and `MPERF` effective-frequency MSRs.
    pub(crate) const POWER_MANAGEMENT_ECX_APERFMPERF: u32 = 0;
    /// Bits of leaf 0xA's EAX that hold the version of architectural performance monitoring.
    pub(crate) const PERFORMANCE_MONITORING_VERSION_MASK: u32 = 0xff;
    /// Bits of each of leaf 0x16's registers that hold a frequency in MHz.
    pub(crate) const FREQUENCY_MHZ_MASK: u32 = 0xffff;
    /// Leaf 7 subleaf 0 EBX bit of the `IA32_TSC_ADJUST` MSR.
    pub(crate) const STRUCTURED_FEATURES_EBX_TSC_ADJUST: u32 = 1;
    /// Leaf 0x80000001 EDX bit of the `RDTSCP` instruction.
    pub(crate) const EXTENDED_FEATURES_EDX_RDTSCP: u32 = 27;
    /// Leaf 0x80000007 EDX bit of the invariant time-stamp counter.
    pub(crate) const ADVANCED_POWER_EDX_INVARIANT_TSC: u32 = 8;

    /// Hyper-V features EAX bit of the hypercall MSRs.
    pub(crate) const HYPERV_EAX_HYPERCALL: u32 = 5;
    /// Hyper-V features EAX bit of the virtual-processor index MSR.
    pub(crate) const HYPERV_EAX_VP_INDEX: u32 = 6;
    /// Hyper-V features EAX bit of the TSC and APIC frequency MSRs.
    pub(crate) const HYPERV_EAX_ACCESS_FREQUENCY: u32 = 11;
    /// Hyper-V features EAX bit of the TSC invariant-control MSR.
    pub(crate) const HYPERV_EAX_ACCESS_TSC_INVARIANT: u32 = 15;
    /// Hyper-V features EDX bit that makes the frequency MSRs available.
    pub(crate) const HYPERV_EDX_FREQUENCY_MSRS: u32 = 8;

    /// Returns whether bit `bit` of `register` is set.
    pub(crate) const fn bit(register: u32, bit: u32) -> bool {
        (register >> bit) & 1 != 0
    }

    /// Formats the registers of `result` as eight hexadecimal digits each, EAX first, between
    /// `separator`s.
    pub(crate) fn hex_registers(result: CpuidResult, separator: &str) -> String {
        Register::ALL
            .map(|register| format!("{:08x}", register.of(result)))
            .join(separator)
    }

    /// Returns the twelve-character signature that EBX, ECX, and EDX of `result` spell, as the
    /// hypervisor leaves hold a vendor's, with each byte that is not printable ASCII shown as a
    /// period.
    pub(crate) fn signature(result: CpuidResult) -> String {
        [result.ebx, result.ecx, result.edx]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .map(|byte| {
                if byte == b' ' || byte.is_ascii_graphic() {
                    char::from(byte)
                } else {
                    '.'
                }
            })
            .collect()
    }

    /// The processor's display family, display model, and stepping, which leaf 1's EAX encodes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Signature {
        /// The family, with the extended family added for family 0xF.
        pub(crate) family: u32,
        /// The model, with the extended model for families 6 and 0xF.
        pub(crate) model: u32,
        /// The stepping.
        pub(crate) stepping: u32,
    }

    impl Signature {
        /// Decodes leaf 1's EAX as Intel's and AMD's manuals define the display family and model.
        pub(crate) const fn decode(eax: u32) -> Self {
            /// Bits of each of the signature's four-bit fields.
            const NIBBLE: u32 = 0xf;
            /// Bits of the extended family.
            const BYTE: u32 = 0xff;
            /// The family whose display family adds the extended family.
            const EXTENDED_FAMILY: u32 = 0xf;
            /// The other family whose display model includes the extended model.
            const EXTENDED_MODEL_FAMILY: u32 = 0x6;
            let family = (eax >> 8) & NIBBLE;
            let mut model = (eax >> 4) & NIBBLE;
            if family == EXTENDED_MODEL_FAMILY || family == EXTENDED_FAMILY {
                model |= ((eax >> 16) & NIBBLE) << 4;
            }
            let family = if family == EXTENDED_FAMILY {
                family + ((eax >> 20) & BYTE)
            } else {
                family
            };
            Self {
                family,
                model,
                stepping: eax & NIBBLE,
            }
        }
    }
}

/// Model-specific registers of the architecture, of Hyper-V, and of KVM that the time tools
/// read or write.
pub(crate) mod msr {
    /// `IA32_TIME_STAMP_COUNTER`.
    pub(crate) const IA32_TSC: u32 = 0x10;
    /// `IA32_TSC_ADJUST`.
    pub(crate) const IA32_TSC_ADJUST: u32 = 0x3b;
    /// `IA32_TSC_DEADLINE`.
    pub(crate) const IA32_TSC_DEADLINE: u32 = 0x6e0;
    /// Hyper-V's `HV_X64_MSR_GUEST_OS_ID`.
    pub(crate) const HV_GUEST_OS_ID: u32 = 0x4000_0000;
    /// Hyper-V's `HV_X64_MSR_HYPERCALL`.
    pub(crate) const HV_HYPERCALL: u32 = 0x4000_0001;
    /// Hyper-V's `HV_X64_MSR_VP_INDEX`.
    pub(crate) const HV_VP_INDEX: u32 = 0x4000_0002;
    /// Hyper-V's `HV_X64_MSR_TIME_REF_COUNT`.
    pub(crate) const HV_TIME_REF_COUNT: u32 = 0x4000_0020;
    /// Hyper-V's `HV_X64_MSR_TSC_FREQUENCY`.
    pub(crate) const HV_TSC_FREQUENCY: u32 = 0x4000_0022;
    /// Hyper-V's `HV_X64_MSR_APIC_FREQUENCY`.
    pub(crate) const HV_APIC_FREQUENCY: u32 = 0x4000_0023;
    /// Hyper-V's `HV_X64_MSR_TSC_INVARIANT_CONTROL`.
    pub(crate) const HV_TSC_INVARIANT_CONTROL: u32 = 0x4000_0118;
    /// KVM's `MSR_KVM_SYSTEM_TIME_NEW`.
    pub(crate) const KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
}

/// Runs `CPUID` with `leaf` and `subleaf`.
#[allow(unused_unsafe)]
pub(crate) fn cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
    // SAFETY: CPUID executes in user mode on every x86-64 processor and accesses no memory.
    unsafe { __cpuid_count(leaf, subleaf) }
}

/// Returns whether the processor has `RDTSCP`, which CPUID leaf 0x80000001 EDX bit 27 reports.
pub(crate) fn has_rdtscp() -> bool {
    cpuid(cpuid::LEAF_EXTENDED_MAXIMUM, 0).eax >= cpuid::LEAF_EXTENDED_FEATURES
        && cpuid::bit(
            cpuid(cpuid::LEAF_EXTENDED_FEATURES, 0).edx,
            cpuid::EXTENDED_FEATURES_EDX_RDTSCP,
        )
}

/// Reads the time-stamp counter without a fence.
#[inline]
#[allow(unused_unsafe)]
pub(crate) fn rdtsc() -> u64 {
    // SAFETY: RDTSC executes in user mode unless the kernel sets CR4.TSD, which Linux does not.
    unsafe { _rdtsc() }
}

/// Reads the time-stamp counter with `RDTSCP`, which waits for the preceding instructions.
///
/// On a processor without the instruction, it raises the same invalid-opcode fault as the C
/// probe did, so callers check [`has_rdtscp`] first.
#[inline]
#[allow(unused_unsafe)]
pub(crate) fn rdtscp() -> u64 {
    let mut processor = 0;
    // SAFETY: the callers checked `has_rdtscp`, and `processor` is writable for the call.
    unsafe { __rdtscp(&mut processor) }
}

/// Reads the time-stamp counter after `LFENCE`, as Linux's `rdtsc_ordered` does.
#[inline]
#[allow(unused_unsafe)]
pub(crate) fn rdtsc_lfence() -> u64 {
    // SAFETY: LFENCE and RDTSC execute in user mode on every x86-64 processor.
    unsafe {
        _mm_lfence();
        _rdtsc()
    }
}

/// Reads the time-stamp counter between two `LFENCE`s, which bracket a timed region.
#[inline]
#[allow(unused_unsafe)]
pub(crate) fn rdtsc_fenced() -> u64 {
    // SAFETY: LFENCE and RDTSC execute in user mode on every x86-64 processor.
    unsafe {
        _mm_lfence();
        let counter = _rdtsc();
        _mm_lfence();
        counter
    }
}

/// Tells the processor that the caller spins: the `PAUSE` instruction on x86-64.
#[inline]
pub(crate) fn pause() {
    std::hint::spin_loop();
}

#[cfg(test)]
mod tests;
