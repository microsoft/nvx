//! The shared time-sample wire codec. Its published TSC labels are not ISA declarations.

use std::fmt::{self, Display};

use super::model::{self, CounterReads, CounterSample, HostClock, HostClockStatus};

/// Bytes occupied by the version-one host time sample.
pub(crate) const SAMPLE_SIZE: usize = 48;
/// Supported time-sample wire version.
const SAMPLE_VERSION: u8 = 1;
/// Flag declaring that host test hooks affected the observation.
const SAMPLE_TEST_HOOKS: u8 = 0x08;
/// Wire sentinel for an unavailable unsigned uncertainty bound.
const SAMPLE_UNKNOWN: u32 = u32::MAX;

/// Checked cursor over a little-endian record, retaining the caller's incomplete-record error.
pub(super) struct Fields<'a, E> {
    /// Record bytes not yet consumed by a successful field read.
    rest: &'a [u8],
    /// Error returned when the next complete field is absent.
    incomplete: E,
}

impl<'a, E: Copy> Fields<'a, E> {
    /// Starts a cursor without admitting any record header or semantic values.
    pub(super) const fn new(record: &'a [u8], incomplete: E) -> Self {
        Self {
            rest: record,
            incomplete,
        }
    }

    /// Copies the next complete field, advancing only on success.
    pub(super) fn bytes<const N: usize>(&mut self) -> Result<[u8; N], E> {
        self.chunk().copied()
    }

    /// Borrows the next complete field, advancing only on success.
    pub(super) fn chunk<const N: usize>(&mut self) -> Result<&'a [u8; N], E> {
        let (field, rest) = self.rest.split_first_chunk().ok_or(self.incomplete)?;
        self.rest = rest;
        Ok(field)
    }

    /// Reads one little-endian unsigned word.
    pub(super) fn u32(&mut self) -> Result<u32, E> {
        self.bytes().map(u32::from_le_bytes)
    }

    /// Reads one little-endian signed word.
    #[cfg(target_arch = "x86_64")]
    pub(super) fn i32(&mut self) -> Result<i32, E> {
        self.bytes().map(i32::from_le_bytes)
    }

    /// Reads one little-endian unsigned double word.
    pub(super) fn u64(&mut self) -> Result<u64, E> {
        self.bytes().map(u64::from_le_bytes)
    }

    /// Reads one little-endian signed double word.
    pub(super) fn i64(&mut self) -> Result<i64, E> {
        self.bytes().map(i64::from_le_bytes)
    }
}

/// Why a host time sample is invalid, retaining the existing published diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeSampleError {
    /// At least one complete version-one field is absent.
    Incomplete,
    /// The header declares this unsupported wire version.
    Version(u8),
    /// The header uses unsupported flag bits or nonzero reserved data.
    Flags {
        /// Raw flags, including any unsupported bits.
        flags: u8,
        /// Reserved header byte that must be zero.
        reserved: u8,
    },
    /// The host clock status uses this reserved value.
    Status(u8),
    /// UTC is zero or outside the signed nanosecond range.
    Utc(u64),
    /// Counter pairing failed; these legacy diagnostic labels do not declare an ISA.
    Tsc {
        /// Rejected guest counter value in the sample.
        tsc: u64,
        /// Counter value before the guest request.
        before: u64,
        /// Counter value after the guest request.
        after: u64,
    },
}

impl Display for TimeSampleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Incomplete => formatter.write_str("time sample is incomplete"),
            Self::Version(version) => write!(formatter, "time sample version is {version}"),
            Self::Flags { flags, reserved } => write!(
                formatter,
                "time sample flags 0x{flags:02x} or reserved 0x{reserved:02x}"
            ),
            Self::Status(status) => write!(formatter, "time sample host status is {status}"),
            Self::Utc(utc) => write!(formatter, "time sample UTC {utc} is invalid"),
            Self::Tsc { tsc, before, after } => write!(
                formatter,
                "time sample TSC {tsc} is outside the guest's reads {before} to {after}"
            ),
        }
    }
}

impl std::error::Error for TimeSampleError {}

/// One version-one host sample; the wire's `tsc` member carries guest counter ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimeSample {
    /// Validated sample flags, including the test-hook marker.
    pub(crate) flags: u8,
    /// VM generation in which the host produced this sample.
    pub(crate) generation: u32,
    /// Host UTC in nanoseconds since the Unix epoch.
    pub(crate) utc_ns: u64,
    /// Guest counter ticks under the existing wire field name.
    pub(crate) tsc: u64,
    /// Host UTC/counter pairing uncertainty in nanoseconds.
    pub(crate) pairing_ns: u32,
    /// Host clock quality and rate-correction metadata.
    pub(crate) host: HostClock,
}

impl TimeSample {
    /// Decodes the version-one fields and rejects unsupported headers or unusable UTC.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, TimeSampleError> {
        let mut fields = Fields::new(bytes, TimeSampleError::Incomplete);
        let [version, flags, status, reserved] = fields.bytes()?;
        let generation = fields.u32()?;
        let utc_ns = fields.u64()?;
        let tsc = fields.u64()?;
        let pairing_ns = fields.u32()?;
        let disruption = fields.u32()?;
        let correction = fields.i64()?;
        let bound = |value: u32| (value != SAMPLE_UNKNOWN).then_some(value);
        let correction_bound = bound(fields.u32()?);
        let max_error_us = bound(fields.u32()?);
        if version != SAMPLE_VERSION {
            return Err(TimeSampleError::Version(version));
        }
        if flags & !SAMPLE_TEST_HOOKS != 0 || reserved != 0 {
            return Err(TimeSampleError::Flags { flags, reserved });
        }
        let status = HostClockStatus::from_byte(status).ok_or(TimeSampleError::Status(status))?;
        if utc_ns == 0 || i64::try_from(utc_ns).is_err() {
            return Err(TimeSampleError::Utc(utc_ns));
        }
        Ok(Self {
            flags,
            generation,
            utc_ns,
            tsc,
            pairing_ns,
            host: HostClock {
                status,
                disruption,
                correction,
                correction_bound,
                max_error_us,
            },
        })
    }

    /// Returns whether the host marked this observation as affected by test hooks.
    pub(crate) const fn test_hooks(&self) -> bool {
        self.flags & SAMPLE_TEST_HOOKS != 0
    }
}

/// Actual counter reads translated to the existing wire adapter's field labels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GuestReads {
    /// Counter value before the guest requested the sample.
    pub(crate) before: u64,
    /// Counter value after the guest received the sample.
    pub(crate) after: u64,
    /// Guest realtime in nanoseconds since the Unix epoch.
    pub(crate) realtime_ns: i64,
    /// Counter paired with the realtime read, under the legacy field name.
    pub(crate) realtime_tsc: u64,
    /// Half-width of the realtime bracket in counter ticks, under the legacy field name.
    pub(crate) realtime_half_cycles: u64,
}

/// Returns host-minus-guest nanoseconds and uncertainty using independently admitted rates.
pub(crate) fn pair_bound_sample(
    sample: &TimeSample,
    reads: &GuestReads,
    rates: model::CounterRates,
) -> Result<(i64, i64), TimeSampleError> {
    let (sample, reads) = counter_observations(sample, reads);
    rates.pair(sample, reads).map_err(pairing_error)
}

/// Translates legacy wire labels to architecture-neutral observations without admitting rates.
pub(super) fn counter_observations(
    sample: &TimeSample,
    reads: &GuestReads,
) -> (CounterSample, CounterReads) {
    (
        CounterSample {
            utc_ns: sample.utc_ns,
            ticks: sample.tsc,
            pairing_ns: sample.pairing_ns,
        },
        CounterReads {
            before: reads.before,
            after: reads.after,
            realtime_ns: reads.realtime_ns,
            realtime_ticks: reads.realtime_tsc,
            realtime_half_ticks: reads.realtime_half_cycles,
        },
    )
}

/// Preserves the published pairing diagnostic while translating a common counter error.
pub(super) fn pairing_error(error: model::CounterOutsideReads) -> TimeSampleError {
    TimeSampleError::Tsc {
        tsc: error.ticks,
        before: error.before,
        after: error.after,
    }
}

#[cfg(test)]
pub(super) mod tests;
