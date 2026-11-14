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
    let set_frequency = ClockAdjustment::SetFrequency(-7);
    let pin = ClockAdjustment::Pin {
        hold,
        status: libc::STA_PLL | libc::STA_NANO,
    };
    assert_eq!((step.modes(), step.status()), (0x2100, 0));
    // The status holds the frequency, `STA_FREQHOLD`, with the loop and nanoseconds.
    assert_eq!((frequency.modes(), frequency.status()), (0x001e, 0x2081));
    // Clearing the nanosecond offset changes neither status nor frequency.
    assert_eq!((clear_phase.modes(), clear_phase.status()), (0x2001, 0));
    assert_eq!((slew.modes(), slew.status()), (0x203f, 0x2081));
    // Setting the frequency alone changes nothing else.
    assert_eq!((set_frequency.modes(), set_frequency.status()), (0x0002, 0));
    // The pin is a step by zero, which resets the status and the error estimates, with a hold
    // of them, and of the frequency, whose status is the caller's.
    assert_eq!((pin.modes(), pin.status()), (0x211e, 0x2001));
    assert_eq!(
        (
            ClockAdjustment::Query.modes(),
            ClockAdjustment::Query.status()
        ),
        (0, 0)
    );
}

#[test]
fn a_discipline_holds_a_frequency_with_its_own_error_estimates() {
    let discipline = ClockDiscipline {
        scaled_ppm: 1_000,
        maximum_error_us: 70,
        estimated_error_us: 3,
        status: libc::STA_PLL,
    };
    assert_eq!(
        discipline.hold(-3),
        FrequencyHold {
            scaled_ppm: -3,
            maximum_error_us: 70,
            estimated_error_us: 3,
        }
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
