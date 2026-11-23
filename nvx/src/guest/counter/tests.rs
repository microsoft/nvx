use super::*;

impl ClockContext {
    pub(crate) fn x86(counter_hz: u64, lapic_hz: u64) -> Result<Self, ClockError> {
        if counter_hz < 1_000_000 || !matches!(lapic_hz, 200_000_000 | 1_000_000_000) {
            return Err(ClockError::Invalid("invalid validated x86 TSC/LAPIC rates"));
        }
        Ok(Self {
            binding: Binding::X86 {
                counter_hz: NonZeroU64::new(counter_hz).unwrap(),
                lapic_hz: NonZeroU64::new(lapic_hz).unwrap(),
            },
        })
    }

    pub(crate) fn x86_rates(&self) -> Result<(NonZeroU64, NonZeroU64), ClockError> {
        match &self.binding {
            Binding::X86 {
                counter_hz,
                lapic_hz,
            } => Ok((*counter_hz, *lapic_hz)),
            Binding::ArmVirtual(_) => Err(ClockError::Invalid(
                "ARM generic-timer metadata is not an x86 TSC/LAPIC rate pair",
            )),
        }
    }
}

impl ClockState {
    pub(crate) fn context(&self) -> Result<&ClockContext, ClockError> {
        match self {
            Self::Uninitialized => Err(ClockError::Uninitialized),
            Self::Ready(context) => Ok(context),
        }
    }
}

impl PublishedRateFields {
    pub(crate) fn decode(
        self,
        context: PublishedRateContext<'_>,
    ) -> Result<ClockState, ClockError> {
        if self.tsc_hz == 0 && self.lapic_hz == 0 {
            return Ok(ClockState::Uninitialized);
        }
        let PublishedRateContext::Validated(binding) = context else {
            return Err(ClockError::Invalid(
                "an uninitialized clock context accepts only zero legacy rate fields",
            ));
        };
        if self != Self::encode(&ClockState::Ready(binding.clone()), context)? {
            return Err(ClockError::Invalid(
                "legacy clock fields contradict the declared architecture/time binding",
            ));
        }
        Ok(ClockState::Ready(binding.clone()))
    }
}

impl PublishedRateContext<'_> {
    pub(crate) fn declared_isa(self) -> DeclaredGuestIsa {
        match self {
            Self::Uninitialized(architecture) => architecture,
            Self::Validated(context) => context.declared_isa(),
        }
    }
}

fn arm_identity() -> [u8; 96] {
    let mut bytes = [0; 96];
    bytes[..4].copy_from_slice(b"NVT\0");
    bytes[4] = 1;
    bytes[8..10].copy_from_slice(&1_u16.to_le_bytes());
    bytes[12..16].copy_from_slice(&1_u32.to_le_bytes());
    bytes[16..24].copy_from_slice(&19_200_001_u64.to_le_bytes());
    bytes[24..28].copy_from_slice(&20_u32.to_le_bytes());
    bytes[28..32].copy_from_slice(&4_u32.to_le_bytes());
    bytes[32..36].copy_from_slice(b"test");
    bytes[64..96].fill(0x5a);
    bytes
}

pub(crate) fn arm_context() -> ClockContext {
    arm_context_at_hz(19_200_001)
}

pub(crate) fn arm_context_at_hz(hz: u64) -> ClockContext {
    let mut identity = arm_identity();
    identity[16..24].copy_from_slice(&hz.to_le_bytes());
    ClockContext::arm(ArmBindingContext::from_identity(&identity, hz).unwrap())
}

#[test]
fn published_fields_need_the_correct_explicit_binding_not_a_zero_lapic_heuristic() {
    let arm_binding = arm_context();
    let x86_binding = ClockContext::x86(2_400_000_123, 200_000_000).unwrap();
    let arm = PublishedRateContext::Validated(&arm_binding);
    let x86 = PublishedRateContext::Validated(&x86_binding);
    let legacy = PublishedRateFields {
        tsc_hz: 19_200_001,
        lapic_hz: 0,
    };
    let state = legacy.decode(arm).unwrap();
    assert_eq!(PublishedRateFields::encode(&state, arm).unwrap(), legacy);
    assert!(legacy.decode(x86).is_err());
    assert!(
        PublishedRateFields {
            tsc_hz: 19_200_000,
            lapic_hz: 0
        }
        .decode(arm)
        .is_err()
    );
    assert!(PublishedRateFields::encode(&state, x86).is_err());
    assert_eq!(arm.declared_isa(), DeclaredGuestIsa::Aarch64);
    assert_eq!(x86.declared_isa(), DeclaredGuestIsa::X86_64);
    assert_eq!(arm_binding.counter_scale().kernel_hz(), 19_200_001);
    assert_eq!(arm_binding.counter_hz().get(), 19_200_001);
    assert_eq!(x86_binding.counter_scale().kernel_hz(), 2_400_000_000);
    assert_eq!(
        x86_binding
            .x86_rates()
            .map(|(counter, timer)| (counter.get(), timer.get())),
        Ok((2_400_000_123, 200_000_000))
    );
    assert!(arm_binding.x86_rates().is_err());
}

#[test]
fn x86_roundtrip_and_uninitialized_state_keep_their_published_fields() {
    let binding = ClockContext::x86(2_400_000_123, 1_000_000_000).unwrap();
    let context = PublishedRateContext::Validated(&binding);
    let legacy = PublishedRateFields {
        tsc_hz: 2_400_000_123,
        lapic_hz: 1_000_000_000,
    };
    let ready = legacy.decode(context).unwrap();
    assert_eq!(
        PublishedRateFields::encode(&ready, context).unwrap(),
        legacy
    );
    let pending = PublishedRateFields {
        tsc_hz: 0,
        lapic_hz: 0,
    }
    .decode(context)
    .unwrap();
    assert_eq!(pending, ClockState::Uninitialized);
    assert_eq!(pending.context(), Err(ClockError::Uninitialized));
    assert!(
        PublishedRateFields {
            tsc_hz: 0,
            lapic_hz: 1
        }
        .decode(context)
        .is_err()
    );
    assert!(ClockContext::x86(0, 0).is_err());
    assert!(ClockContext::x86(999_999, 200_000_000).is_err());
    assert!(ClockContext::x86(1_000_000, 200_000_000).is_ok());
}

#[test]
fn zero_only_context_declares_both_isas_without_admitting_a_clock_or_ready_state() {
    let zeros = PublishedRateFields {
        tsc_hz: 0,
        lapic_hz: 0,
    };
    let x86 = ClockContext::x86(2_400_000_000, 200_000_000).unwrap();
    let arm = arm_context();
    for architecture in [DeclaredGuestIsa::X86_64, DeclaredGuestIsa::Aarch64] {
        let context = PublishedRateContext::Uninitialized(architecture);
        assert_eq!(context.declared_isa(), architecture);
        let pending = zeros.decode(context).unwrap();
        assert_eq!(pending, ClockState::Uninitialized);
        assert_eq!(pending.context(), Err(ClockError::Uninitialized));
        assert_eq!(
            PublishedRateFields::encode(&pending, context).unwrap(),
            zeros
        );
        for invalid in [
            PublishedRateFields {
                tsc_hz: 1,
                lapic_hz: 0,
            },
            PublishedRateFields {
                tsc_hz: 0,
                lapic_hz: 1,
            },
            PublishedRateFields {
                tsc_hz: 2_400_000_000,
                lapic_hz: 200_000_000,
            },
            PublishedRateFields {
                tsc_hz: 19_200_001,
                lapic_hz: 0,
            },
        ] {
            assert!(
                invalid.decode(context).is_err(),
                "{architecture:?}: {invalid:?}"
            );
        }
        for ready in [&x86, &arm] {
            assert!(
                PublishedRateFields::encode(&ClockState::Ready(ready.clone()), context).is_err()
            );
        }
    }
}

#[test]
fn arm_context_checks_observed_full_hz_timer_capacity_and_selected_identity_record() {
    let identity = arm_identity();
    assert!(ArmBindingContext::from_identity(&identity, 19_200_000).is_err());
    for (offset, value) in [(12, 3_u32), (24, 19), (28, 33)] {
        let mut bad = identity;
        bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(ArmBindingContext::from_identity(&bad, 19_200_001).is_err());
    }
    let mut bad = identity;
    bad[36] = b'x';
    assert!(ArmBindingContext::from_identity(&bad, 19_200_001).is_err());
    assert!(ArmBindingContext::from_identity(&identity[..95], 19_200_001).is_err());
    let mut bad = identity;
    bad[16..24].fill(0);
    assert!(ArmBindingContext::from_identity(&bad, 0).is_err());
    let mut bad = identity;
    bad[64..96].fill(0);
    assert!(ArmBindingContext::from_identity(&bad, 19_200_001).is_err());
    for id in [b"auto", b"host", b"bad!", b"TEST"] {
        let mut bad = identity;
        bad[32..36].copy_from_slice(id);
        assert!(ArmBindingContext::from_identity(&bad, 19_200_001).is_err());
    }
}
