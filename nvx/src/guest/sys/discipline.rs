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
    /// Clears the phase offset through the phase-locked loop in nanosecond mode, without changing
    /// its status or held frequency correction.
    ClearPhase,
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
    /// Sets the frequency correction alone, and keeps the status, the error estimates, and the
    /// phase offset.
    SetFrequency(i64),
    /// Makes the wall clock run at the frequency correction of `hold` alone, from the moment of
    /// the call: it steps the wall clock by zero, which clears the kernel's NTP state, the phase
    /// offset and the tick length's cached correction of it included, which a clearing of the
    /// phase offset alone leaves in force until the kernel's next second, and then sets the
    /// frequency correction and the error estimates of `hold`, and `status`, which the step reset
    /// to unsynchronized.
    Pin {
        /// The frequency correction and the error estimates.
        hold: FrequencyHold,
        /// The discipline's status, as the kernel reports it.
        status: i32,
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
            Self::ClearPhase => libc::ADJ_OFFSET | libc::ADJ_NANO,
            Self::Slew { .. } => HOLD | libc::ADJ_OFFSET | libc::ADJ_TIMECONST | libc::ADJ_NANO,
            Self::SetFrequency(_) => libc::ADJ_FREQUENCY,
            Self::Pin { .. } => HOLD | libc::ADJ_SETOFFSET | libc::ADJ_NANO,
        }
    }

    /// Returns the discipline status that the adjustment sets, or zero for one that sets none.
    pub(crate) fn status(self) -> i32 {
        match self {
            Self::Query | Self::Step { .. } | Self::ClearPhase | Self::SetFrequency(_) => 0,
            Self::Frequency(_) | Self::Slew { .. } => PLL_NANOSECONDS,
            Self::Pin { status, .. } => status,
        }
    }
}

/// The discipline that the kernel reports after an adjustment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClockDiscipline {
    /// Frequency correction in parts per million, scaled by 2^16.
    pub(crate) scaled_ppm: i64,
    /// Maximum error in microseconds.
    pub(crate) maximum_error_us: i64,
    /// Estimated error in microseconds.
    pub(crate) estimated_error_us: i64,
    /// The discipline's status bits, `STA_*`.
    pub(crate) status: i32,
}

impl ClockDiscipline {
    /// Returns the hold of the frequency correction `scaled_ppm` with this discipline's error
    /// estimates.
    pub(crate) fn hold(self, scaled_ppm: i64) -> FrequencyHold {
        FrequencyHold {
            scaled_ppm,
            maximum_error_us: self.maximum_error_us,
            estimated_error_us: self.estimated_error_us,
        }
    }
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
        ClockAdjustment::Query | ClockAdjustment::ClearPhase => {}
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
        ClockAdjustment::Frequency(hold) | ClockAdjustment::Pin { hold, .. } => {
            hold.apply(&mut value);
        }
        ClockAdjustment::SetFrequency(scaled_ppm) => value.freq = scaled_ppm,
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
        maximum_error_us: value.maxerror,
        estimated_error_us: value.esterror,
        status: value.status,
    })
}

#[cfg(test)]
mod tests;
