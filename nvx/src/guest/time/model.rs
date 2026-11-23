//! Platform-independent time observations and units, shared by reports and the runtime.

use crate::guest::counter::CounterScale;
use std::fmt;

/// Fixed-point units in one part per million, as Linux adjtimex uses.
pub(super) const SCALED_PPM: i64 = 1 << 16;
/// Linux's maximum absolute frequency correction, in scaled ppm.
pub(super) const FREQUENCY_LIMIT: i64 = 500 * SCALED_PPM;
/// Parts per billion in one part per million.
pub(super) const PPB_PER_PPM: i64 = 1000;

/// Lifecycle phase named by conformance records and violation events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Initial cold-boot admission.
    Boot,
    /// Preparation for a supported snapshot.
    Capture,
    /// Repair after a supported restore.
    Restore,
    /// Ongoing monitoring after initial admission.
    Runtime,
}

impl Phase {
    /// Phases with a slot in the published conformance-check record.
    pub(crate) const CHECKS: [Self; 3] = [Self::Boot, Self::Capture, Self::Restore];

    /// Returns the stable phase token used in published diagnostics.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Capture => "capture",
            Self::Restore => "restore",
            Self::Runtime => "runtime",
        }
    }

    /// Returns a conformance-record slot, absent for continuous runtime monitoring.
    pub(crate) fn check_index(self) -> Option<usize> {
        Self::CHECKS.iter().position(|&phase| phase == self)
    }
}

/// Host UTC discipline quality, independent of guest clock-binding admission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HostClockStatus {
    /// No usable host discipline status was supplied.
    #[default]
    Unknown,
    /// The host reports externally synchronized UTC.
    Synchronized,
    /// The host clock runs without external synchronization.
    FreeRunning,
    /// The host explicitly reports unreliable UTC.
    Unreliable,
}

impl HostClockStatus {
    /// Parses only the existing published status tokens.
    pub(super) fn parse(text: &str) -> Option<Self> {
        match text {
            "unknown" => Some(Self::Unknown),
            "synchronized" => Some(Self::Synchronized),
            "free-running" => Some(Self::FreeRunning),
            "unreliable" => Some(Self::Unreliable),
            _ => None,
        }
    }

    /// Decodes the version-one wire status, refusing reserved values.
    pub(crate) const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Unknown),
            1 => Some(Self::Synchronized),
            2 => Some(Self::FreeRunning),
            3 => Some(Self::Unreliable),
            _ => None,
        }
    }
}

impl fmt::Display for HostClockStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unknown => "unknown",
            Self::Synchronized => "synchronized",
            Self::FreeRunning => "free-running",
            Self::Unreliable => "unreliable",
        })
    }
}

/// The discipline of host UTC that a time sample publishes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostClock {
    /// Host UTC quality reported with this sample.
    pub(crate) status: HostClockStatus,
    /// Host clock discontinuity counter, distinct from the VM generation.
    pub(crate) disruption: u32,
    /// Host UTC's rate against the declared guest counter, minus one, in scaled ppm.
    pub(crate) correction: i64,
    /// Bound on the frequency correction in unsigned scaled ppm, when known.
    pub(crate) correction_bound: Option<u32>,
    /// Maximum synchronized UTC error in microseconds, when known.
    pub(crate) max_error_us: Option<u32>,
}

impl HostClock {
    /// Returns an error estimate only for synchronized UTC with a supplied bound.
    pub(crate) fn error_us(&self) -> i64 {
        match (self.status, self.max_error_us) {
            (HostClockStatus::Synchronized, Some(error)) => i64::from(error),
            _ => 0,
        }
    }
}

/// Narrowest accepted observation and the correction requested by its host clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BestSample {
    /// Estimated host UTC minus guest realtime, in nanoseconds.
    pub(crate) theta: i64,
    /// Conservative uncertainty of that offset, in nanoseconds.
    pub(crate) epsilon: i64,
    /// Host quality and rate metadata belonging to this observation.
    pub(crate) host: HostClock,
    /// Requested Linux frequency correction in scaled ppm.
    pub(crate) frequency: i64,
    /// Whether the requested correction reached Linux's supported frequency limit.
    pub(crate) limited: bool,
    /// Whether the host marked the observation as affected by test hooks.
    pub(crate) test_hooks: bool,
}

/// Actual admitted counter scale and the kernel's current scaled-ppm correction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CounterRates {
    /// Independently admitted host-sample and guest-kernel conversion rates.
    pub(crate) scale: CounterScale,
    /// Kernel frequency correction active during the paired observation, in scaled ppm.
    pub(crate) frequency: i64,
}

impl CounterRates {
    /// Pairs a host observation with the guest's reads using the admitted rates.
    pub(crate) fn pair(
        self,
        sample: CounterSample,
        reads: CounterReads,
    ) -> Result<(i64, i64), CounterOutsideReads> {
        pair_counter_sample(sample, reads, self.scale, self.frequency)
    }

    /// Converts the host's scaled-ppm rate correction to Linux's rate and limit flag.
    pub(crate) fn requested_frequency(self, correction: i64) -> (i64, bool) {
        frequency_for_counter(
            correction,
            self.scale.declared_hz().get(),
            self.scale.kernel_hz(),
        )
    }
}

/// Divides by a positive denominator, rounding halfway values away from zero.
pub(super) fn divide_rounded(numerator: i128, denominator: i128) -> i128 {
    let half = denominator / 2;
    if numerator >= 0 {
        (numerator + half) / denominator
    } else {
        (numerator - half) / denominator
    }
}

/// Bounds intermediate arithmetic to the signed published nanosecond/rate range.
pub(super) fn saturate(value: i128) -> i64 {
    value.clamp(i64::MIN.into(), i64::MAX.into()) as i64
}

/// Converts signed scaled ppm to rounded parts per billion.
pub(crate) fn frequency_to_ppb(frequency: i64) -> i64 {
    saturate(divide_rounded(
        i128::from(frequency) * i128::from(PPB_PER_PPM),
        i128::from(SCALED_PPM),
    ))
}

/// Converts an unsigned scaled-ppm uncertainty to parts per billion, rounding upward.
pub(crate) fn bound_to_ppb(bound: u32) -> u64 {
    (u64::from(bound) * PPB_PER_PPM as u64).div_ceil(SCALED_PPM as u64)
}

/// Common rate math; ISA-specific callers select the kernel's actual counter rate.
pub(crate) fn frequency_for_counter(
    correction: i64,
    declared_hz: u64,
    kernel_hz: u64,
) -> (i64, bool) {
    let scaled_one = i128::from(SCALED_PPM) * 1_000_000;
    let declared = i128::from(declared_hz.max(1));
    let correction = i128::from(correction).clamp(-scaled_one, scaled_one);
    let numerator = (scaled_one + correction) * i128::from(kernel_hz) - scaled_one * declared;
    let frequency = divide_rounded(numerator, declared);
    let limit = i128::from(FREQUENCY_LIMIT);
    let held = frequency.clamp(-limit, limit);
    (held as i64, held != frequency)
}

/// A host UTC observation paired with one guest counter value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CounterSample {
    /// Observed host UTC in nanoseconds since the Unix epoch.
    pub(crate) utc_ns: u64,
    /// Guest counter value paired with that UTC observation.
    pub(crate) ticks: u64,
    /// Host-side UTC/counter pairing uncertainty in nanoseconds.
    pub(crate) pairing_ns: u32,
}

/// Guest-side brackets and the realtime observation used to compare against host UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CounterReads {
    /// Counter read before requesting the host observation.
    pub(crate) before: u64,
    /// Counter read after receiving the host observation.
    pub(crate) after: u64,
    /// Guest realtime in nanoseconds since the Unix epoch.
    pub(crate) realtime_ns: i64,
    /// Counter value paired with the guest realtime read.
    pub(crate) realtime_ticks: u64,
    /// Half-width of the guest realtime/counter bracket, in counter ticks.
    pub(crate) realtime_half_ticks: u64,
}

/// A host-paired counter value outside the guest bracket and allowed pairing tolerance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CounterOutsideReads {
    /// Rejected counter value supplied by the host.
    pub(crate) ticks: u64,
    /// Start of the guest request bracket.
    pub(crate) before: u64,
    /// End of the guest request bracket.
    pub(crate) after: u64,
}

/// One shared pairing algorithm. Only its explicit scale decides quantized x86 versus full-Hz ARM.
pub(crate) fn pair_counter_sample(
    sample: CounterSample,
    reads: CounterReads,
    scale: CounterScale,
    frequency: i64,
) -> Result<(i64, i64), CounterOutsideReads> {
    pair_counter_at_rates(
        sample,
        reads,
        scale.declared_hz().get(),
        scale.pairing_hz(),
        frequency,
    )
}

/// Arithmetic-only adapter used by the existing x86 synthetic test command, including zero-rate
/// fixtures. It constructs no binding or Ready metadata and is never the ARM rate-admission path.
pub(super) fn pair_counter_at_rates(
    sample: CounterSample,
    reads: CounterReads,
    declared_hz: u64,
    kernel_hz: u64,
    frequency: i64,
) -> Result<(i64, i64), CounterOutsideReads> {
    /// Nanoseconds per second for counter conversion.
    const NSEC: i128 = 1_000_000_000;
    /// Additional host/guest pairing skew allowance in nanoseconds.
    const SKEW_BOUND_NS: i128 = 1000;
    /// Maximum realtime slew allowance while extrapolating the observation.
    const SLEW_RATE_PPM: i128 = 2000;
    let up = |numerator: i128, denominator: i128| (numerator + denominator - 1) / denominator;
    let declared_hz = i128::from(declared_hz.max(1));
    let kernel_hz = i128::from(kernel_hz.max(1));
    let pairing = i128::from(sample.pairing_ns);
    let tolerance = up((pairing + SKEW_BOUND_NS) * declared_hz, NSEC);
    let ticks = i128::from(sample.ticks);
    if ticks < i128::from(reads.before) - tolerance || ticks > i128::from(reads.after) + tolerance {
        return Err(CounterOutsideReads {
            ticks: sample.ticks,
            before: reads.before,
            after: reads.after,
        });
    }
    let frequency = i128::from(frequency.clamp(-FREQUENCY_LIMIT, FREQUENCY_LIMIT));
    let elapsed = divide_rounded((i128::from(reads.realtime_ticks) - ticks) * NSEC, kernel_hz);
    let extrapolated =
        elapsed + divide_rounded(elapsed * frequency, i128::from(SCALED_PPM) * 1_000_000);
    let realtime = i128::from(reads.realtime_ns) - extrapolated;
    let local = up(i128::from(reads.realtime_half_ticks) * NSEC, kernel_hz);
    let slew = up(extrapolated.abs() * SLEW_RATE_PPM, 1_000_000);
    Ok((
        saturate(i128::from(sample.utc_ns) - realtime),
        saturate(pairing + SKEW_BOUND_NS + local + slew),
    ))
}

#[cfg(test)]
mod tests;
