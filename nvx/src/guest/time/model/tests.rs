use super::*;
use crate::guest::counter::{ClockState, tests::arm_context};

#[test]
fn shared_pairing_uses_checked_full_arm_hz_without_x86_khz_rounding() {
    let binding = arm_context();
    let scale = binding.counter_scale();
    let sample = CounterSample {
        utc_ns: 1_000_000_000,
        ticks: 1_000_000,
        pairing_ns: 30,
    };
    let reads = CounterReads {
        before: sample.ticks - 1,
        after: sample.ticks + 1,
        realtime_ns: 2_000_000_000,
        realtime_ticks: sample.ticks + 19_200_001,
        realtime_half_ticks: 2,
    };
    let rates = CounterRates {
        scale,
        frequency: 0,
    };
    assert_eq!(rates.pair(sample, reads).unwrap().0, 0);
    assert_eq!(rates.requested_frequency(0), (0, false));
    let x86_scale = CounterScale::x86_counter(19_200_001).unwrap();
    assert_eq!(
        pair_counter_sample(sample, reads, x86_scale, 0).unwrap().0,
        52
    );
    assert!(CounterScale::x86_counter(0).is_err());
    assert!(ClockState::Uninitialized.context().is_err());
}

#[test]
fn generic_counter_pairing_refuses_samples_outside_the_actual_read_bracket() {
    let scale = arm_context().counter_scale();
    let sample = CounterSample {
        utc_ns: 1_000_000_000,
        ticks: 1_000_000,
        pairing_ns: 30,
    };
    let reads = CounterReads {
        before: 2_000_000,
        after: 2_000_010,
        ..CounterReads::default()
    };
    assert_eq!(
        pair_counter_sample(sample, reads, scale, 0),
        Err(CounterOutsideReads {
            ticks: sample.ticks,
            before: reads.before,
            after: reads.after
        })
    );
}

#[test]
fn the_host_error_counts_only_while_a_time_service_synchronizes_the_host() {
    let host = |status, max_error_us| HostClock {
        status,
        max_error_us,
        ..HostClock::default()
    };
    assert_eq!(
        host(HostClockStatus::Synchronized, Some(12_500)).error_us(),
        12_500
    );
    assert_eq!(host(HostClockStatus::Synchronized, None).error_us(), 0);
    assert_eq!(
        host(HostClockStatus::FreeRunning, Some(16_000_000)).error_us(),
        0
    );
    assert_eq!(host(HostClockStatus::Unknown, Some(5)).error_us(), 0);
}

#[test]
fn runtime_observations_do_not_create_a_persisted_check_phase() {
    assert_eq!(Phase::Runtime.check_index(), None);
    assert_eq!(Phase::Runtime.name(), "runtime");
    assert_eq!(
        Phase::CHECKS.map(Phase::name),
        ["boot", "capture", "restore"]
    );
}

#[test]
fn common_frequency_math_retains_full_arm_hz_and_existing_x86_quantization() {
    assert_eq!(frequency_for_counter(0, 19_200_001, 19_200_001), (0, false));
    assert_eq!(
        frequency_for_counter(13_107_200, 19_200_001, 19_200_001),
        (13_107_200, false)
    );
    assert_eq!(
        frequency_for_counter(0, 2_194_843_123, 2_194_843_000),
        (-3_673, false)
    );
    assert_eq!(
        frequency_for_counter(39_321_600, 19_200_001, 19_200_001),
        (32_768_000, true)
    );
    assert_eq!(frequency_for_counter(0, 0, 0), (-32_768_000, true));
}
