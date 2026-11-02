//! Checks of the kernel's encoding of each adjustment.

use super::*;

#[test]
fn adjustments_select_the_kernel_modes_and_status() {
    let step = ClockAdjustment::Step {
        seconds: -2,
        nanoseconds: 872_000_000,
    };
    let hold = FrequencyHold {
        scaled_ppm: 65_536,
        maximum_error_us: 2,
        estimated_error_us: 1,
    };
    let frequency = ClockAdjustment::Frequency(hold);
    let clear_phase = ClockAdjustment::ClearPhase;
    let slew = ClockAdjustment::Slew {
        offset_ns: 5,
        time_constant: 4,
        hold,
    };
    assert_eq!((step.modes(), step.status()), (0x2100, 0));
    // The status holds the frequency, `STA_FREQHOLD`, with the loop and nanoseconds.
    assert_eq!((frequency.modes(), frequency.status()), (0x001e, 0x2081));
    // Clearing the nanosecond offset changes neither status nor frequency.
    assert_eq!((clear_phase.modes(), clear_phase.status()), (0x2001, 0));
    assert_eq!((slew.modes(), slew.status()), (0x203f, 0x2081));
    assert_eq!(
        (
            ClockAdjustment::Query.modes(),
            ClockAdjustment::Query.status()
        ),
        (0, 0)
    );
}

#[test]
fn a_hold_writes_the_frequency_and_the_error_estimates() {
    // SAFETY: all zeros is the kernel's baseline for every field of `timex`.
    let mut value: libc::timex = unsafe { std::mem::zeroed() };
    FrequencyHold {
        scaled_ppm: -13_107_200,
        maximum_error_us: 70,
        estimated_error_us: 3,
    }
    .apply(&mut value);
    assert_eq!(
        (value.freq, value.maxerror, value.esterror),
        (-13_107_200, 70, 3)
    );
}

#[test]
fn a_query_reports_the_frequency_without_privileges() {
    assert!(adjust_clock(ClockAdjustment::Query).is_ok());
}
