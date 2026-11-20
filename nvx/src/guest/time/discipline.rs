//! Shared wall-clock correction policy over the existing Linux discipline operations.

use super::{NSEC_PER_SEC, NSEC_PER_USEC};
use crate::guest::sys::{ClockAdjustment, FrequencyHold};

/// Offset magnitude at which correction steps rather than slews realtime.
const STEP_THRESHOLD_NS: u64 = 128_000_000;
/// Linux PLL time constant during initial convergence.
const FAST_TIME_CONSTANT: i64 = 4;
/// Linux PLL time constant after convergence.
const SLOW_TIME_CONSTANT: i64 = 6;

/// Splits a signed offset into floor-rounded seconds and nonnegative nanoseconds.
pub(crate) fn split_offset(offset_ns: i64) -> (i64, i64) {
    (
        offset_ns.div_euclid(NSEC_PER_SEC),
        offset_ns.rem_euclid(NSEC_PER_SEC),
    )
}

/// Rounds a nonnegative nanosecond uncertainty upward to microseconds.
fn ceil_us(nanoseconds: i64) -> i64 {
    nanoseconds.saturating_add(NSEC_PER_USEC - 1) / NSEC_PER_USEC
}

/// Builds Linux error estimates from offset, pairing uncertainty and synchronized host error.
pub(crate) fn frequency_hold(
    offset: u64,
    epsilon: i64,
    frequency: i64,
    host_error_us: i64,
) -> FrequencyHold {
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    FrequencyHold {
        scaled_ppm: frequency,
        maximum_error_us: ceil_us(offset.saturating_add(epsilon)).saturating_add(host_error_us),
        estimated_error_us: ceil_us(epsilon),
    }
}

/// Selects an exact step or the phase/frequency PLL update, without applying it.
pub(crate) fn plan_correction(
    theta: i64,
    epsilon: i64,
    fast: bool,
    frequency: i64,
    host_error_us: i64,
) -> ClockAdjustment {
    let magnitude = theta.unsigned_abs();
    if magnitude >= STEP_THRESHOLD_NS {
        let (seconds, nanoseconds) = split_offset(theta);
        return ClockAdjustment::Step {
            seconds,
            nanoseconds,
        };
    }
    ClockAdjustment::Slew {
        offset_ns: theta,
        time_constant: if fast {
            FAST_TIME_CONSTANT
        } else {
            SLOW_TIME_CONSTANT
        },
        hold: frequency_hold(magnitude, epsilon, frequency, host_error_us),
    }
}

#[cfg(test)]
mod tests;
