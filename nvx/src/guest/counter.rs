//! Architecture-qualified guest clock metadata and the current published rate-field adapter.
//!
//! The published field names below belong to that codec, not to the common counter model.

use std::fmt;
use std::num::NonZeroU64;

/// Failed admission of clock metadata, before it can become a ready clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClockError {
    /// No checked clock has been supplied.
    #[cfg(test)]
    Uninitialized,
    /// The metadata contradicts this binding requirement.
    Invalid(&'static str),
}

impl fmt::Display for ClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(test)]
            Self::Uninitialized => formatter.write_str("counter metadata is not initialized"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ClockError {}

/// A checked guest-visible identity of the selected ARM CPU/time binding.
///
/// The VMM admits the selected effective CPU profile before boot. This decoder does not replace
/// that admission: it checks the exact identity record, timer identity and independent EL0 Hz.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(any(test, target_arch = "aarch64"))]
pub(crate) struct ArmBindingContext {
    /// Maximum virtual-processor capacity of the admitted binding.
    capacity: u32,
    /// Full-Hz rate, checked against the independently read CNTFRQ.
    counter_hz: NonZeroU64,
    /// Virtual timer interrupt identifier.
    timer_ppi: u32,
    /// Concrete selected CPU profile, never an automatic-selection token.
    profile_id: String,
    /// Nonzero digest of the selected effective profile.
    profile_sha256: [u8; 32],
}

#[cfg(any(test, target_arch = "aarch64"))]
impl ArmBindingContext {
    /// Checks the complete versioned record against the actual EL0 counter rate.
    pub(crate) fn from_identity(
        bytes: &[u8],
        observed_counter_hz: u64,
    ) -> Result<Self, ClockError> {
        let bytes: &[u8; 96] = bytes
            .try_into()
            .map_err(|_| ClockError::Invalid("ARM time identity must contain exactly 96 bytes"))?;
        if &bytes[..4] != b"NVT\0" || bytes[4] != 1 || bytes[5..8] != [0; 3] {
            return Err(ClockError::Invalid("unsupported ARM time identity header"));
        }
        let half = |offset| {
            u16::from_le_bytes(
                bytes[offset..offset + 2]
                    .try_into()
                    .expect("checked record"),
            )
        };
        let word = |offset| {
            u32::from_le_bytes(
                bytes[offset..offset + 4]
                    .try_into()
                    .expect("checked record"),
            )
        };
        if (half(8), half(10)) != (1, 0) {
            return Err(ClockError::Invalid("unsupported ARM time binding version"));
        }
        let capacity = word(12);
        if !matches!(capacity, 1 | 2 | 4 | 8) {
            return Err(ClockError::Invalid(
                "ARM time capacity must be 1, 2, 4 or 8",
            ));
        }
        let declared = u64::from_le_bytes(bytes[16..24].try_into().expect("checked record"));
        if declared != observed_counter_hz || declared > u64::from(u32::MAX) {
            return Err(ClockError::Invalid(
                "ARM time binding and observed CNTFRQ do not agree",
            ));
        }
        let counter_hz = NonZeroU64::new(declared)
            .ok_or(ClockError::Invalid("ARM counter frequency must be nonzero"))?;
        let timer_ppi = word(24);
        if timer_ppi != 20 {
            return Err(ClockError::Invalid(
                "ARM generic virtual timer must use INTID 20",
            ));
        }
        let length = usize::try_from(word(28)).expect("u32 fits the supported host sizes");
        if !(1..=32).contains(&length) {
            return Err(ClockError::Invalid(
                "ARM selected CPU profile ID length is invalid",
            ));
        }
        let id = &bytes[32..32 + length];
        if !id.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        }) || bytes[32 + length..64].iter().any(|&byte| byte != 0)
            || matches!(id, b"auto" | b"host")
        {
            return Err(ClockError::Invalid(
                "ARM selected CPU profile ID or padding is invalid",
            ));
        }
        let profile_sha256 = bytes[64..96].try_into().expect("checked record");
        if profile_sha256 == [0; 32] {
            return Err(ClockError::Invalid(
                "ARM selected CPU profile digest is absent",
            ));
        }
        Ok(Self {
            capacity,
            counter_hz,
            timer_ppi,
            profile_id: String::from_utf8(id.to_vec()).expect("validated ASCII"),
            profile_sha256,
        })
    }
}

/// A checked ARM identity, plus an opposite-ISA context for codec refusal tests.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(any(test, target_arch = "aarch64"))]
enum Binding {
    /// Valid x86 metadata used only by cross-ISA codec tests.
    #[cfg(test)]
    X86 {
        /// Nonzero recorded TSC rate.
        counter_hz: NonZeroU64,
        /// Nonzero recorded local-APIC timer rate.
        lapic_hz: NonZeroU64,
    },
    /// Admitted generic virtual counter and timer/profile identity.
    ArmVirtual(ArmBindingContext),
}

/// A caller's explicit guest-ISA declaration, not a hardware or clock admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(test, target_arch = "aarch64"))]
pub(crate) enum DeclaredGuestIsa {
    /// Explicit x86-64 declaration, without implying valid rate metadata.
    #[cfg(test)]
    X86_64,
    /// Explicit ARM64 declaration, without implying a checked profile binding.
    Aarch64,
}

/// Checked ARM binding context; no field or rate heuristic selects the ISA.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(any(test, target_arch = "aarch64"))]
pub(crate) struct ClockContext {
    /// Architecture-specific rate contract, never inferred from legacy field values.
    binding: Binding,
}

/// Architecture-qualified counter arithmetic, not clock/profile or snapshot admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CounterScale {
    /// Counter rate used by the host's UTC observation.
    declared_hz: NonZeroU64,
    /// Actual kernel conversion rate, including x86's legacy floor-to-kHz result.
    kernel_hz: u64,
    /// Pairing conversion rate; only x86 applies the legacy synthetic 1000-Hz floor.
    pairing_hz: u64,
}

impl CounterScale {
    /// The initial x86 sample needs an independently read TSC rate, not an earlier LAPIC read.
    #[cfg(any(test, target_arch = "x86_64"))]
    pub(crate) fn x86_counter(counter_hz: u64) -> Result<Self, ClockError> {
        if counter_hz < 1_000_000 {
            return Err(ClockError::Invalid("invalid validated x86 TSC rate"));
        }
        Ok(Self::x86_recorded(
            NonZeroU64::new(counter_hz).expect("positive checked rate"),
        ))
    }

    /// Preserves the x86 cache's any-positive persisted-rate contract without new admission.
    #[cfg(any(test, target_arch = "x86_64"))]
    pub(crate) fn x86_recorded(rate: NonZeroU64) -> Self {
        let kernel_hz = rate.get() / 1000 * 1000;
        Self {
            declared_hz: rate,
            kernel_hz,
            pairing_hz: kernel_hz.max(1000),
        }
    }

    /// Returns the checked full-Hz sample rate.
    pub(crate) fn declared_hz(self) -> NonZeroU64 {
        self.declared_hz
    }

    /// Returns the actual kernel conversion rate, including x86's kHz quantization.
    pub(crate) fn kernel_hz(self) -> u64 {
        self.kernel_hz
    }

    /// Preserves the x86 pairing floor without applying it to its frequency correction.
    pub(crate) fn pairing_hz(self) -> u64 {
        self.pairing_hz
    }
}

#[cfg(any(test, target_arch = "aarch64"))]
impl ClockContext {
    /// Returns the architecture recorded by this admitted binding.
    pub(crate) fn declared_isa(&self) -> DeclaredGuestIsa {
        match &self.binding {
            #[cfg(test)]
            Binding::X86 { .. } => DeclaredGuestIsa::X86_64,
            Binding::ArmVirtual(_) => DeclaredGuestIsa::Aarch64,
        }
    }

    /// Wraps an ARM identity that has already passed complete binding validation.
    pub(crate) fn arm(binding: ArmBindingContext) -> Self {
        Self {
            binding: Binding::ArmVirtual(binding),
        }
    }

    /// Returns the binding's checked full-Hz counter rate.
    pub(crate) fn counter_hz(&self) -> NonZeroU64 {
        match &self.binding {
            #[cfg(test)]
            Binding::X86 { counter_hz, .. } => *counter_hz,
            Binding::ArmVirtual(binding) => binding.counter_hz,
        }
    }

    /// Returns the admitted ARM VP capacity, refusing an x86 clock.
    #[cfg(target_arch = "aarch64")]
    pub(crate) fn arm_capacity(&self) -> Result<u32, ClockError> {
        match &self.binding {
            Binding::ArmVirtual(binding) => Ok(binding.capacity),
            #[cfg(test)]
            Binding::X86 { .. } => Err(ClockError::Invalid("ARM generic-timer binding required")),
        }
    }

    /// Derives arithmetic rates only from this already-checked clock.
    pub(crate) fn counter_scale(&self) -> CounterScale {
        match &self.binding {
            #[cfg(test)]
            Binding::X86 { counter_hz, .. } => CounterScale::x86_recorded(*counter_hz),
            Binding::ArmVirtual(_) => CounterScale {
                declared_hz: self.counter_hz(),
                kernel_hz: self.counter_hz().get(),
                pairing_hz: self.counter_hz().get(),
            },
        }
    }
}

/// Clock admission state, separate from the presence of published status bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg(any(test, target_arch = "aarch64"))]
pub(crate) enum ClockState {
    /// No checked binding is available; only zero legacy rate fields are admissible.
    #[default]
    Uninitialized,
    /// A context satisfying its explicit ISA's rate/binding contract.
    Ready(ClockContext),
}

/// A zero-only report may declare its ISA without fabricating a validated binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(test, target_arch = "aarch64"))]
pub(crate) enum PublishedRateContext<'a> {
    /// Explicit ISA declaration that permits only the legacy zero/zero encoding.
    Uninitialized(DeclaredGuestIsa),
    /// Explicit rate context; an ARM context always carries the complete checked binding.
    Validated(&'a ClockContext),
}

/// The two still-current published field names are confined to the report codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PublishedRateFields {
    /// Legacy rate slot: TSC Hz on x86, generic virtual counter Hz on ARM.
    pub(crate) tsc_hz: u64,
    /// Legacy local-APIC slot, exactly zero for an admitted ARM binding.
    pub(crate) lapic_hz: u64,
}

impl PublishedRateFields {
    /// Emits zero-only uninitialized metadata or the exact rates of the supplied binding.
    #[cfg(any(test, target_arch = "aarch64"))]
    pub(crate) fn encode(
        state: &ClockState,
        expected: PublishedRateContext<'_>,
    ) -> Result<Self, ClockError> {
        if matches!(state, ClockState::Uninitialized) {
            return Ok(Self {
                tsc_hz: 0,
                lapic_hz: 0,
            });
        }
        let PublishedRateContext::Validated(expected) = expected else {
            return Err(ClockError::Invalid(
                "an uninitialized clock context cannot encode Ready clock metadata",
            ));
        };
        let context = match state {
            ClockState::Uninitialized => unreachable!("uninitialized state encoded above"),
            ClockState::Ready(context) if context == expected => context,
            ClockState::Ready(_) => {
                return Err(ClockError::Invalid(
                    "clock metadata contradicts the supplied architecture/time binding",
                ));
            }
        };
        Ok(match &context.binding {
            #[cfg(test)]
            Binding::X86 {
                counter_hz,
                lapic_hz,
            } => Self {
                tsc_hz: counter_hz.get(),
                lapic_hz: lapic_hz.get(),
            },
            Binding::ArmVirtual(binding) => Self {
                tsc_hz: binding.counter_hz.get(),
                // This is a legacy codec slot, not an ARM local-APIC model.
                lapic_hz: 0,
            },
        })
    }
}

#[cfg(test)]
pub(super) mod tests;
