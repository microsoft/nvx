//! ARM hardware adapter for the shared codec, pairing and Linux clock discipline.

use std::fmt;

use super::{Rates, Reads, Sample};
use crate::guest::sys::platform::binding::{BindingError, TimeBinding};
use crate::guest::sys::{self, ClockAdjustment, Errno, aarch64, kernel, platform};

/// A failed time transaction or an inconsistent effective Linux counter interface.
#[derive(Debug)]
pub(crate) enum Error {
    /// An operating-system or restricted-device operation failed.
    Operation(&'static str, Errno),
    /// The identity record was structurally malformed.
    Binding(BindingError),
    /// Linux is not using the architected system counter as its clocksource.
    Clocksource(Vec<u8>),
    /// The effective architected frequency differs from the declared one.
    Frequency {
        /// Frequency declared by the admitted host binding.
        declared: u64,
        /// Effective CNTFRQ value visible through Linux's normal EL0 interface.
        effective: u64,
    },
    /// A time sample failed the unchanged wire-v1 decoder.
    Wire(super::Error),
    /// A different generation must not discipline the current guest's clock.
    Generation {
        /// Generation carried by the sample.
        observed: u32,
        /// Generation of the caller's current cold boot or repair.
        expected: u32,
    },
    /// The paired counter lies outside the synchronous local latch interval.
    Counter(super::CounterOutsideReads),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operation(operation, error) => write!(formatter, "{operation}: {error}"),
            Self::Binding(error) => error.fmt(formatter),
            Self::Clocksource(value) => write!(
                formatter,
                "clocksource {:?}, expected arch_sys_counter",
                String::from_utf8_lossy(value)
            ),
            Self::Frequency {
                declared,
                effective,
            } => write!(
                formatter,
                "CNTFRQ is {effective} Hz, time binding declares {declared} Hz"
            ),
            Self::Wire(error) => write!(formatter, "time sample: {error:?}"),
            Self::Generation { observed, expected } => {
                write!(
                    formatter,
                    "time sample generation {observed}, expected {expected}"
                )
            }
            Self::Counter(reads) => write!(
                formatter,
                "sample counter {} outside latch reads {}..{}",
                reads.counter, reads.before, reads.after
            ),
        }
    }
}

impl std::error::Error for Error {}

/// A real paired sample and the existing discipline's resulting correction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Paired {
    /// Original wire fields, retaining honestly unknown host-error bounds.
    pub(crate) sample: Sample,
    /// Actual local reads surrounding the synchronous host latch.
    pub(crate) reads: Reads,
    /// Guest-to-host UTC offset in nanoseconds.
    pub(crate) theta: i64,
    /// Pairing uncertainty in nanoseconds.
    pub(crate) epsilon: i64,
    /// Linux frequency correction in scaled ppm.
    pub(crate) frequency: i64,
    /// Whether Linux's existing 500-ppm correction limit clamped the result.
    pub(crate) limited: bool,
}

impl Paired {
    /// Converts the measurement into the existing published-state representation.
    pub(crate) fn measurement(self) -> super::BestSample {
        super::BestSample {
            theta: self.theta,
            epsilon: self.epsilon,
            host: super::HostClock {
                status: super::HostClockStatus::from_byte(self.sample.host_status)
                    .expect("wire decoder validated host status"),
                disruption: self.sample.disruption,
                correction: self.sample.correction,
                correction_bound: self.sample.correction_bound,
                max_error_us: self.sample.max_error_us,
            },
            frequency: self.frequency,
            limited: self.limited,
            test_hooks: self.sample.flags & 0x08 != 0,
        }
    }
}

/// Clock metadata only: no descriptor, mapping, permission or record lock is retained.
pub(crate) struct Clock {
    /// Structural binding metadata, not independent proof of CPU-profile admission.
    pub(crate) binding: TimeBinding,
}

impl Clock {
    /// Opens the existing identity window, verifies Linux's effective counter conversion, and
    /// relinquishes all device authority before returning.
    pub(crate) fn open() -> Result<Self, Error> {
        let record = platform::time_binding_record()
            .map_err(|error| Error::Operation("read time binding", error))?;
        let binding = TimeBinding::parse(&record).map_err(Error::Binding)?;
        let clocksource = sys::read_file(kernel::CURRENT_CLOCKSOURCE)
            .map_err(|error| Error::Operation("read current clocksource", error))?;
        let frequency = aarch64::counter_frequency_hz()
            .map_err(|error| Error::Operation("read CNTFRQ", error))?;
        validate_conversion(&binding, &clocksource, frequency)?;
        Ok(Self { binding })
    }

    /// Takes one synchronous sample through the existing a7 window and shared v1 math.
    pub(crate) fn sample(&self, generation: u32) -> Result<Paired, Error> {
        let (latched, correction) = self.read()?;
        pair_latched(&latched, self.binding.counter_hz, correction, generation)
    }

    /// Retains raw diagnostic fields even when their pairing will fail.
    pub(crate) fn diagnostic(&self) -> Result<super::DiagnosticSample, Error> {
        let (latched, correction) = self.read()?;
        Ok(super::DiagnosticSample {
            sample: Sample::parse(&latched.bytes).map_err(Error::Wire)?,
            reads: reads(&latched),
            rates: Rates {
                counter_hz: self.binding.counter_hz,
                kernel_hz: self.binding.counter_hz,
                frequency: correction,
            },
            profile: (self.binding.profile_id.clone(), self.binding.profile_sha256),
        })
    }

    /// Reads the real effective conversion and latch without retaining device authority.
    fn read(&self) -> Result<(platform::LatchedTime, i64), Error> {
        let frequency = aarch64::counter_frequency_hz()
            .map_err(|error| Error::Operation("read CNTFRQ", error))?;
        if frequency != self.binding.counter_hz {
            return Err(Error::Frequency {
                declared: self.binding.counter_hz,
                effective: frequency,
            });
        }
        let discipline = sys::adjust_clock(ClockAdjustment::Query)
            .map_err(|error| Error::Operation("query adjtimex", error))?;
        let latched =
            platform::latch_time().map_err(|error| Error::Operation("latch time sample", error))?;
        Ok((latched, discipline.scaled_ppm))
    }
}

/// Verifies the standard kernel conversion interface; this is not raw CPU-profile readback.
fn validate_conversion(
    binding: &TimeBinding,
    clocksource: &[u8],
    effective_hz: u64,
) -> Result<(), Error> {
    if clocksource != b"arch_sys_counter\n" && clocksource != b"arch_sys_counter" {
        return Err(Error::Clocksource(clocksource.to_vec()));
    }
    if effective_hz != binding.counter_hz {
        return Err(Error::Frequency {
            declared: binding.counter_hz,
            effective: effective_hz,
        });
    }
    Ok(())
}

/// Adapts the real latch to the same codec and math as x86, preserving full ARM Hz rather than
/// applying the x86 kernel's kHz truncation.
fn pair_latched(
    latched: &platform::LatchedTime,
    frequency_hz: u64,
    current_correction: i64,
    generation: u32,
) -> Result<Paired, Error> {
    let sample = Sample::parse(&latched.bytes).map_err(Error::Wire)?;
    if sample.generation != generation {
        return Err(Error::Generation {
            observed: sample.generation,
            expected: generation,
        });
    }
    let reads = reads(latched);
    let (theta, epsilon) = super::pair(
        sample.utc_ns,
        sample.counter,
        sample.pairing_ns,
        &reads,
        Rates {
            counter_hz: frequency_hz,
            kernel_hz: frequency_hz,
            frequency: current_correction,
        },
    )
    .map_err(Error::Counter)?;
    let (frequency, limited) =
        super::guest_frequency(sample.correction, frequency_hz, frequency_hz);
    Ok(Paired {
        sample,
        reads,
        theta,
        epsilon,
        frequency,
        limited,
    })
}

/// Adapts the hardware latch's reads without changing counter units.
fn reads(latched: &platform::LatchedTime) -> Reads {
    Reads {
        before: latched.before,
        after: latched.after,
        realtime_ns: latched.realtime_ns,
        realtime_counter: latched.realtime_counter,
        realtime_half_cycles: latched.realtime_half_cycles,
    }
}

#[cfg(test)]
mod tests;
