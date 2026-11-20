use super::*;

#[test]
fn correction_boundary_matches_the_abi() {
    assert!(matches!(
        plan_correction(128_000_000, 10, true, 0, 0),
        ClockAdjustment::Step { .. }
    ));
    assert!(matches!(
        plan_correction(127_999_999, 10, true, 0, 0),
        ClockAdjustment::Slew { .. }
    ));
    assert_eq!(
        plan_correction(-1_128_000_000, 0, false, 0, 0),
        ClockAdjustment::Step {
            seconds: -2,
            nanoseconds: 872_000_000
        }
    );
}

#[test]
fn a_slew_holds_the_frequency_with_the_host_error() {
    assert_eq!(
        plan_correction(-1_500, 2_100, false, 13_107_200, 12_500),
        ClockAdjustment::Slew {
            offset_ns: -1_500,
            time_constant: 6,
            hold: FrequencyHold {
                scaled_ppm: 13_107_200,
                maximum_error_us: 4 + 12_500,
                estimated_error_us: 3,
            },
        }
    );
    assert_eq!(
        frequency_hold(0, 524, -3_673, 0),
        FrequencyHold {
            scaled_ppm: -3_673,
            maximum_error_us: 1,
            estimated_error_us: 1
        }
    );
}

#[test]
fn offsets_split_toward_negative_infinity() {
    assert_eq!(split_offset(-1), (-1, 999_999_999));
    assert_eq!(split_offset(1_500_000_000), (1, 500_000_000));
    assert_eq!(split_offset(i64::MIN), (-9_223_372_037, 145_224_192));
}
