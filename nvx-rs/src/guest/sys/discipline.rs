//! The kernel's discipline of the wall clock, through `adjtimex`.
//!
//! `nix` does not wrap `adjtimex`, so this module calls it through libc, and takes the kernel's
//! mode and status bits from libc's constants. Callers describe an adjustment as a typed
//! [`ClockAdjustment`], so that only this module knows how the kernel encodes it.

use nix::libc;

use super::Errno;

/// The discipline status of the frequency and slew adjustments: the phase-locked loop, with
/// offsets in nanoseconds rather than microseconds, and with its frequency held, so that it slews
/// only the phase and keeps the frequency correction that the discipline sets from the host's.
const PLL_NANOSECONDS: i32 = libc::STA_PLL | libc::STA_NANO | libc::STA_FREQHOLD;

/// A change to the wall clock's discipline, or a query of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClockAdjustment {
    /// Reads the discipline without changing it.
    Query,
    /// Steps the wall clock by `seconds` plus `nanoseconds`.
    Step {
        /// Whole seconds of the step, rounded toward negative infinity.
        seconds: i64,
        /// Nonnegative nanoseconds that complete the step.
        nanoseconds: i64,
    },
    /// Sets the frequency correction and the error estimates, and selects the phase-locked loop
    /// in nanosecond mode with its frequency held.
    Frequency(FrequencyHold),
    /// Slews the wall clock by `offset_ns` through the phase-locked loop in nanosecond mode, with
    /// its frequency held, and sets its time constant, the frequency correction, and the error
    /// estimates.
    Slew {
        /// Phase offset in nanoseconds.
        offset_ns: i64,
        /// Time constant of the phase-locked loop.
        time_constant: i64,
        /// The frequency correction and the error estimates.
        hold: FrequencyHold,
    },
}

/// The frequency correction that the discipline holds, and the error estimates that it sets with
/// it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrequencyHold {
    /// Frequency correction in parts per million, scaled by 2^16.
    pub(crate) scaled_ppm: i64,
    /// Maximum error in microseconds.
    pub(crate) maximum_error_us: i64,
    /// Estimated error in microseconds.
    pub(crate) estimated_error_us: i64,
}

impl ClockAdjustment {
    /// Returns the `modes` bits with which `adjtimex` applies the adjustment.
    pub(crate) fn modes(self) -> u32 {
        /// The modes that set the held frequency and the error estimates.
        const HOLD: u32 =
            libc::ADJ_FREQUENCY | libc::ADJ_MAXERROR | libc::ADJ_ESTERROR | libc::ADJ_STATUS;
        match self {
            Self::Query => 0,
            Self::Step { .. } => libc::ADJ_SETOFFSET | libc::ADJ_NANO,
            Self::Frequency(_) => HOLD,
            Self::Slew { .. } => HOLD | libc::ADJ_OFFSET | libc::ADJ_TIMECONST | libc::ADJ_NANO,
        }
    }

    /// Returns the discipline status that the adjustment sets, or zero for one that sets none.
    pub(crate) fn status(self) -> i32 {
        match self {
            Self::Query | Self::Step { .. } => 0,
            Self::Frequency(_) | Self::Slew { .. } => PLL_NANOSECONDS,
        }
    }
}

/// The discipline that the kernel reports after an adjustment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClockDiscipline {
    /// Frequency correction in parts per million, scaled by 2^16.
    pub(crate) scaled_ppm: i64,
}

impl FrequencyHold {
    /// Writes the frequency correction and the error estimates to `value`.
    fn apply(self, value: &mut libc::timex) {
        value.freq = self.scaled_ppm;
        value.maxerror = self.maximum_error_us;
        value.esterror = self.estimated_error_us;
    }
}

/// Applies `adjustment` to the wall clock's discipline, which needs `CAP_SYS_TIME` for any
/// adjustment but a query, and returns the discipline that results.
pub(crate) fn adjust_clock(adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno> {
    // SAFETY: all zeros is the kernel's baseline for every field of `timex`.
    let mut value: libc::timex = unsafe { std::mem::zeroed() };
    value.modes = adjustment.modes();
    value.status = adjustment.status();
    match adjustment {
        ClockAdjustment::Query => {}
        ClockAdjustment::Step {
            seconds,
            nanoseconds,
        } => {
            // With `ADJ_NANO`, the kernel reads the microseconds field as nanoseconds.
            value.time = libc::timeval {
                tv_sec: seconds,
                tv_usec: nanoseconds,
            };
        }
        ClockAdjustment::Frequency(hold) => hold.apply(&mut value),
        ClockAdjustment::Slew {
            offset_ns,
            time_constant,
            hold,
        } => {
            value.offset = offset_ns;
            value.constant = time_constant;
            hold.apply(&mut value);
        }
    }
    // SAFETY: `value` is initialized, and writable for the kernel's reply, for the call.
    Errno::result(unsafe { libc::adjtimex(&raw mut value) })?;
    Ok(ClockDiscipline {
        scaled_ppm: value.freq,
    })
}

#[cfg(test)]
mod tests {
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
        let slew = ClockAdjustment::Slew {
            offset_ns: 5,
            time_constant: 4,
            hold,
        };
        assert_eq!((step.modes(), step.status()), (0x2100, 0));
        // The status holds the frequency, `STA_FREQHOLD`, with the loop and nanoseconds.
        assert_eq!((frequency.modes(), frequency.status()), (0x001e, 0x2081));
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
}
