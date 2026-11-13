use super::*;

/// Synthetic binding for conversion arithmetic, never an approved product profile.
fn binding() -> TimeBinding {
    TimeBinding {
        capacity: 1,
        counter_hz: 24_000_001,
        profile_id: "fixture".into(),
        profile_sha256: [0; 32],
    }
}

#[test]
fn effective_conversion_rejects_wrong_clocksource_or_frequency() {
    let binding = binding();
    assert!(validate_conversion(&binding, b"arch_sys_counter\n", binding.counter_hz).is_ok());
    assert!(matches!(
        validate_conversion(&binding, b"jiffies\n", binding.counter_hz),
        Err(Error::Clocksource(_))
    ));
    assert!(matches!(
        validate_conversion(&binding, b"arch_sys_counter\n", 24_000_000),
        Err(Error::Frequency {
            declared: 24_000_001,
            effective: 24_000_000,
        })
    ));
}

#[test]
fn arm_latch_uses_shared_v1_and_rejects_other_generations() {
    let mut bytes = super::super::tests::sample();
    bytes[16..24].copy_from_slice(&1000_u64.to_le_bytes());
    let mut latched = platform::LatchedTime {
        bytes,
        before: 999,
        after: 1001,
        realtime_ns: 1_000_020_000,
        realtime_counter: 1480,
        realtime_half_cycles: 12,
    };
    let paired = pair_latched(&latched, 24_000_000, 0, 0).unwrap();
    assert_eq!(paired.theta, 0);
    assert_eq!(paired.epsilon, 10540);
    assert_eq!(paired.frequency, -42);
    assert_eq!(paired.sample.correction_bound, None);
    assert_eq!(paired.sample.max_error_us, None);
    latched.bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
    assert!(matches!(
        pair_latched(&latched, 24_000_000, 0, 0),
        Err(Error::Generation {
            observed: 1,
            expected: 0,
        })
    ));
    latched.bytes[0] = 2;
    assert!(matches!(
        pair_latched(&latched, 24_000_000, 0, 0),
        Err(Error::Wire(super::super::Error::Version(2)))
    ));
}
