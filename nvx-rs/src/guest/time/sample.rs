//! The one version-1 host-UTC wire decoder, independent of the guest counter architecture.

use std::fmt::Write as _;

use super::state::HostClockStatus;
use crate::guest::sys::{ClockAdjustment, FrequencyHold};

#[cfg(target_arch = "aarch64")]
pub(crate) mod arm64;

/// Lifecycle phase of the existing published time state and violation records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Initial guest boot.
    Boot,
    /// Snapshot capture preparation, unsupported by Phase 1 ARM.
    Capture,
    /// Post-snapshot repair, unsupported by Phase 1 ARM.
    Restore,
    /// Continuous daemon operation.
    Runtime,
}

impl Phase {
    /// Existing persisted phase order, unchanged for x86 state-file compatibility.
    pub(crate) const CHECKS: [Self; 3] = [Self::Boot, Self::Capture, Self::Restore];

    /// Returns the existing lowercase token.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Capture => "capture",
            Self::Restore => "restore",
            Self::Runtime => "runtime",
        }
    }

    /// Returns the phase's position in the persisted check records.
    pub(crate) fn check_index(self) -> Option<usize> {
        Self::CHECKS.iter().position(|&phase| phase == self)
    }
}

/// The discipline of host UTC published by the unchanged time sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostClock {
    /// Published host-clock status.
    pub(crate) status: HostClockStatus,
    /// Host UTC or guest-counter disruption generation.
    pub(crate) disruption: u32,
    /// Host UTC's signed scaled-ppm correction relative to the declared counter rate.
    pub(crate) correction: i64,
    /// Published correction-error bound, or honestly unknown.
    pub(crate) correction_bound: Option<u32>,
    /// Published host UTC error bound in microseconds, or honestly unknown.
    pub(crate) max_error_us: Option<u32>,
}

impl HostClock {
    /// Returns the existing adjtimex error-estimate contribution: a synchronized host's known
    /// bound, otherwise none. This does not turn an unknown published bound into a known one.
    pub(crate) fn error_us(&self) -> i64 {
        match (self.status, self.max_error_us) {
            (HostClockStatus::Synchronized, Some(error)) => i64::from(error),
            _ => 0,
        }
    }
}

/// A paired sample in the existing published state's architecture-independent representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BestSample {
    /// Guest-to-host UTC offset in nanoseconds.
    pub(crate) theta: i64,
    /// Pairing uncertainty in nanoseconds.
    pub(crate) epsilon: i64,
    /// Host clock correction and honestly unknown bounds.
    pub(crate) host: HostClock,
    /// Linux's resulting frequency correction, in scaled ppm.
    pub(crate) frequency: i64,
    /// Whether Linux's existing 500-ppm correction limit clamped the result.
    pub(crate) limited: bool,
    /// Existing host test-hook indicator, never a qualification result.
    pub(crate) test_hooks: bool,
}

/// Whether the selected sample satisfied the requested uncertainty bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleResult {
    /// The sample was within the bound.
    Accepted,
    /// A valid sample was selected but remained wider than the bound.
    Wide,
}

/// Whether another attempt can repair a failed measurement.
pub(crate) enum AttemptError<E> {
    /// A malformed record or pairing permits another bounded attempt.
    Retry(E),
    /// An authority, binding or generation failure ends the transaction.
    Fatal(E),
}

/// Selects the first narrow sample, or the least uncertain valid sample after all attempts.
pub(crate) fn select_sample<E>(
    attempts: usize,
    bound_ns: i64,
    mut invalid: E,
    mut take: impl FnMut() -> Result<BestSample, AttemptError<E>>,
) -> Result<(SampleResult, BestSample), E> {
    let mut best: Option<BestSample> = None;
    for _ in 0..attempts {
        let candidate = match take() {
            Ok(candidate) => candidate,
            Err(AttemptError::Retry(error)) => {
                invalid = error;
                continue;
            }
            Err(AttemptError::Fatal(error)) => return Err(error),
        };
        if best.is_none_or(|best| candidate.epsilon < best.epsilon) {
            best = Some(candidate);
        }
        if candidate.epsilon <= bound_ns {
            return Ok((SampleResult::Accepted, candidate));
        }
    }
    best.map(|best| (SampleResult::Wide, best)).ok_or(invalid)
}

/// Exact encoded record length, unchanged across the x86 and ARM time bindings.
pub(crate) const SIZE: usize = 48;
/// Existing maximum attempt count for an initial or periodic sample group.
pub(crate) const SAMPLE_ATTEMPTS: usize = 3;
/// Existing maximum accepted pairing uncertainty, in nanoseconds.
pub(crate) const SAMPLE_BOUND_NS: i64 = 100_000;
/// Existing initial discipline interval.
pub(crate) const FAST_PERIOD: std::time::Duration = std::time::Duration::from_secs(16);
/// Existing steady-state discipline interval.
pub(crate) const SLOW_PERIOD: std::time::Duration = std::time::Duration::from_secs(64);
/// Existing number of initial fast polls.
pub(crate) const FAST_POLLS: i32 = 4;
/// Existing maximum synchronized sample age, in nanoseconds.
pub(crate) const SYNCHRONIZED_AGE_NS: i64 = 128_000_000_000;
/// Nanoseconds in a second.
const NSEC: i128 = 1_000_000_000;
/// Existing cross-processor skew allowance, not a measured qualification result.
const SKEW_NS: i128 = 1000;
/// Existing maximum slew contribution to local extrapolation.
const SLEW_PPM: i128 = 2000;
/// One full rate in Linux's signed scaled-ppm unit.
const SCALED_ONE: i128 = 1_000_000 * (1 << 16);
/// Linux's existing 500-ppm frequency-correction limit.
const FREQUENCY_LIMIT: i128 = 500 * (1 << 16);
/// Offset magnitude at or above which the existing discipline steps the clock.
const STEP_THRESHOLD_NS: u64 = 128_000_000;
/// Linux's scaled-ppm representation of one ppm.
const SCALED_PPM: i64 = 1 << 16;
/// Parts per billion in one ppm.
const PPB_PER_PPM: i64 = 1000;

/// A malformed version-1 field, before architecture-specific pairing or admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// The record ends before byte 48.
    Incomplete,
    /// A version other than one.
    Version(u8),
    /// Undefined flags or a nonzero reserved byte.
    Flags {
        /// Observed flags.
        flags: u8,
        /// Observed reserved byte.
        reserved: u8,
    },
    /// An undefined host-clock status.
    Status(u8),
    /// Zero UTC or UTC outside signed nanoseconds.
    Utc(u64),
}

/// Validated common wire fields; structural validity is not CPU/time qualification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sample {
    /// Existing sample/test flags.
    pub(crate) flags: u8,
    /// Existing host-clock status byte, zero through three.
    pub(crate) host_status: u8,
    /// VM process generation counter.
    pub(crate) generation: u32,
    /// Host UTC nanoseconds.
    pub(crate) utc_ns: u64,
    /// The guest architecture's counter paired with UTC, without conversion.
    pub(crate) counter: u64,
    /// Published pairing uncertainty, not a correction-error bound.
    pub(crate) pairing_ns: u32,
    /// Host-clock disruption generation.
    pub(crate) disruption: u32,
    /// Host-clock correction in signed scaled ppm.
    pub(crate) correction: i64,
    /// Correction-error bound, or honestly unknown.
    pub(crate) correction_bound: Option<u32>,
    /// Host UTC maximum error in microseconds, or unknown.
    pub(crate) max_error_us: Option<u32>,
}

impl Sample {
    /// Decodes the record at the start of the buffer, preserving the existing x86 behavior
    /// for trailing bytes and the existing unknown-bound sentinel.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let bytes = bytes.get(..SIZE).ok_or(Error::Incomplete)?;
        let [version, flags, status, reserved] = bytes[..4].try_into().expect("checked record");
        let word = |offset| {
            u32::from_le_bytes(
                bytes[offset..offset + 4]
                    .try_into()
                    .expect("checked record"),
            )
        };
        let double = |offset| {
            u64::from_le_bytes(
                bytes[offset..offset + 8]
                    .try_into()
                    .expect("checked record"),
            )
        };
        if version != 1 {
            return Err(Error::Version(version));
        }
        if flags & !0x08 != 0 || reserved != 0 {
            return Err(Error::Flags { flags, reserved });
        }
        if status > 3 {
            return Err(Error::Status(status));
        }
        let utc_ns = double(8);
        if utc_ns == 0 || i64::try_from(utc_ns).is_err() {
            return Err(Error::Utc(utc_ns));
        }
        let bound = |value| (value != u32::MAX).then_some(value);
        Ok(Self {
            flags,
            host_status: status,
            generation: word(4),
            utc_ns,
            counter: double(16),
            pairing_ns: word(24),
            disruption: word(28),
            correction: i64::from_le_bytes(bytes[32..40].try_into().expect("checked record")),
            correction_bound: bound(word(40)),
            max_error_us: bound(word(44)),
        })
    }
}

/// Guest clock reads surrounding a synchronous selector write and the following realtime read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reads {
    /// Counter immediately before the latch.
    pub(crate) before: u64,
    /// Counter immediately after the latch.
    pub(crate) after: u64,
    /// Linux realtime in nanoseconds.
    pub(crate) realtime_ns: i64,
    /// Midpoint of the counter reads around realtime.
    pub(crate) realtime_counter: u64,
    /// Upper bound on half the counter interval around realtime.
    pub(crate) realtime_half_cycles: u64,
}

/// Declared and kernel conversion rates, with the current Linux correction.
#[derive(Clone, Copy)]
pub(crate) struct Rates {
    /// The actual declared counter frequency.
    pub(crate) counter_hz: u64,
    /// The rate Linux uses to convert its clocksource, not necessarily x86's truncated kHz.
    pub(crate) kernel_hz: u64,
    /// Current signed scaled-ppm frequency correction.
    pub(crate) frequency: i64,
}

/// One diagnostic record before pairing, so a failed pairing does not hide the raw sample.
pub(crate) struct DiagnosticSample {
    /// Validated wire fields, with honestly unknown host-error bounds.
    pub(crate) sample: Sample,
    /// Actual local reads surrounding the latch.
    pub(crate) reads: Reads,
    /// Actual counter and Linux conversion rates.
    pub(crate) rates: Rates,
    /// Actual ARM binding identity, not a profile allowlist.
    #[cfg(target_arch = "aarch64")]
    pub(crate) profile: (String, [u8; 32]),
}

impl DiagnosticSample {
    /// Formats the same raw-and-paired diagnostic on either ISA, retaining counter-specific names.
    pub(crate) fn format(&self) -> (String, Result<(), CounterOutsideReads>) {
        let sample = &self.sample;
        let host = HostClockStatus::from_byte(sample.host_status).expect("validated wire status");
        let bound =
            |value: Option<u32>| value.map_or_else(|| "unknown".into(), |value| value.to_string());
        #[cfg(target_arch = "x86_64")]
        let counter_name = "tsc";
        #[cfg(target_arch = "aarch64")]
        let counter_name = "counter";
        let mut line = format!(
            "flags=0x{:02x} generation={} status={} utc_ns={} {counter_name}={} pairing_ns={} disruption={} correction={} correction_bound={} max_error_us={} before={} after={} realtime_ns={}",
            sample.flags,
            sample.generation,
            host,
            sample.utc_ns,
            sample.counter,
            sample.pairing_ns,
            sample.disruption,
            sample.correction,
            bound(sample.correction_bound),
            bound(sample.max_error_us),
            self.reads.before,
            self.reads.after,
            self.reads.realtime_ns,
        );
        let paired = pair(
            sample.utc_ns,
            sample.counter,
            sample.pairing_ns,
            &self.reads,
            self.rates,
        );
        if let Ok((theta, epsilon)) = &paired {
            let (frequency, limited) = guest_frequency(
                sample.correction,
                self.rates.counter_hz,
                self.rates.kernel_hz,
            );
            let _ = write!(
                line,
                " theta_ns={theta} epsilon_ns={epsilon} frequency={frequency} frequency_limited={}",
                u8::from(limited)
            );
        }
        #[cfg(target_arch = "aarch64")]
        {
            let mut digest = String::new();
            for byte in self.profile.1 {
                let _ = write!(digest, "{byte:02x}");
            }
            let _ = write!(
                line,
                " counter_hz={} profile_id={} profile_sha256={digest}",
                self.rates.counter_hz, self.profile.0
            );
        }
        line.push('\n');
        (line, paired.map(drop))
    }
}

/// An impossible host latch after the published pairing uncertainty and skew allowance.
#[derive(Debug)]
pub(crate) struct CounterOutsideReads {
    /// The sample's counter.
    pub(crate) counter: u64,
    /// Local lower read.
    pub(crate) before: u64,
    /// Local upper read.
    pub(crate) after: u64,
}

/// Divides by a positive denominator, rounding to nearest and ties away from zero.
pub(crate) fn divide_rounded(numerator: i128, denominator: i128) -> i128 {
    let half = denominator / 2;
    if numerator >= 0 {
        (numerator + half) / denominator
    } else {
        (numerator - half) / denominator
    }
}

/// Divides a nonnegative numerator by a positive denominator, rounding upward.
fn divide_up(numerator: i128, denominator: i128) -> i128 {
    (numerator + denominator - 1) / denominator
}

/// Saturates a clock result to signed nanoseconds.
pub(crate) fn saturate(value: i128) -> i64 {
    value.clamp(i64::MIN.into(), i64::MAX.into()) as i64
}

/// The existing offset/uncertainty calculation, parameterized only by how Linux converts the
/// architecture's clocksource. Unknown correction bounds remain unknown and do not replace
/// measured pairing uncertainty.
pub(crate) fn pair(
    utc_ns: u64,
    counter: u64,
    pairing_ns: u32,
    reads: &Reads,
    rates: Rates,
) -> Result<(i64, i64), CounterOutsideReads> {
    let counter_hz = i128::from(rates.counter_hz.max(1));
    let kernel_hz = i128::from(rates.kernel_hz.max(1));
    let pairing = i128::from(pairing_ns);
    let tolerance = divide_up((pairing + SKEW_NS) * counter_hz, NSEC);
    let paired = i128::from(counter);
    if paired < i128::from(reads.before) - tolerance || paired > i128::from(reads.after) + tolerance
    {
        return Err(CounterOutsideReads {
            counter,
            before: reads.before,
            after: reads.after,
        });
    }
    let frequency = i128::from(rates.frequency).clamp(-FREQUENCY_LIMIT, FREQUENCY_LIMIT);
    let elapsed = divide_rounded(
        (i128::from(reads.realtime_counter) - paired) * NSEC,
        kernel_hz,
    );
    let extrapolated = elapsed + divide_rounded(elapsed * frequency, SCALED_ONE);
    let realtime = i128::from(reads.realtime_ns) - extrapolated;
    let theta = i128::from(utc_ns) - realtime;
    let local = divide_up(i128::from(reads.realtime_half_cycles) * NSEC, kernel_hz);
    let slew = divide_up(extrapolated.abs() * SLEW_PPM, 1_000_000);
    Ok((saturate(theta), saturate(pairing + SKEW_NS + local + slew)))
}

/// Derives the correction from the published host rate and the kernel clocksource conversion,
/// preserving Linux's existing clamp and without rounding ARM frequency to x86 kHz.
pub(crate) fn guest_frequency(correction: i64, counter_hz: u64, kernel_hz: u64) -> (i64, bool) {
    let rate = i128::from(counter_hz.max(1));
    let correction = i128::from(correction).clamp(-SCALED_ONE, SCALED_ONE);
    let numerator = (SCALED_ONE + correction) * i128::from(kernel_hz) - SCALED_ONE * rate;
    let frequency = divide_rounded(numerator, rate);
    let held = frequency.clamp(-FREQUENCY_LIMIT, FREQUENCY_LIMIT);
    (held as i64, held != frequency)
}

/// Converts Linux scaled ppm to rounded parts per billion.
pub(crate) fn frequency_to_ppb(frequency: i64) -> i64 {
    saturate(divide_rounded(
        i128::from(frequency) * i128::from(PPB_PER_PPM),
        i128::from(SCALED_PPM),
    ))
}

/// Converts a known scaled-ppm error bound to ppb, rounding upward.
pub(crate) fn bound_to_ppb(bound: u32) -> u64 {
    (u64::from(bound) * PPB_PER_PPM as u64).div_ceil(SCALED_PPM as u64)
}

/// Splits a signed step into floor seconds and nonnegative nanoseconds.
pub(crate) fn split_offset(offset_ns: i64) -> (i64, i64) {
    (
        offset_ns.div_euclid(NSEC as i64),
        offset_ns.rem_euclid(NSEC as i64),
    )
}

/// Rounds nonnegative nanoseconds to whole microseconds, upward and saturating.
fn ceil_us(nanoseconds: i64) -> i64 {
    nanoseconds.saturating_add(999) / 1000
}

/// Derives the existing Linux frequency hold and error estimates without replacing unknown
/// host bounds: callers supply their existing interpretation of the host error.
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

/// Applies the existing step threshold and fast/slow PLL constants, independently of the
/// architecture of the counter used to derive the offset.
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
        time_constant: if fast { 4 } else { 6 },
        hold: frequency_hold(magnitude, epsilon, frequency, host_error_us),
    }
}

#[cfg(test)]
mod tests;
/// Resolves the shared effective mode for every command, using init's exact token splitting.
pub(crate) fn report_only(command_line: &[u8], explicit: bool) -> bool {
    use crate::protocol::cmdline::{self, Token};

    explicit
        || cmdline::init_tokens(command_line).any(|token| {
            token
                == Token {
                    name: b"nvx_time_abi",
                    value: Some(b"report-only"),
                }
        })
}
