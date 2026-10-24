//! Eligibility: whether a clone may restore a generation of a template at a host's time, by the
//! restore window that the time contract of its capture sets, apart from its compatibility.

#[cfg(test)]
mod tests;

use std::fmt;
use std::io;
use std::time::SystemTime;

use super::manifest::{HostClock, TimeContract};
use super::store::Generation;
use crate::error::{Error, ErrorKind, Result};
use crate::host::platform;
use crate::host::process::BootId;

/// The longest interval by which a restore advances a clone's VM time, from the cut of the
/// captured VM's time to the restoring host's time, in nanoseconds: 30 days, as version
/// [`TIME_ABI_VERSION`](crate::contract::TIME_ABI_VERSION) of OpenVMM's time ABI selects
/// downtimes.
pub const MAX_DOWNTIME_NS: u64 = 30 * 24 * 60 * 60 * 1_000_000_000;

/// The time of a host as a restore samples it: the identities of the host and of its boot, its
/// monotonic clock with that clock's reading, and its UTC.
///
/// [`HostTime::now`] samples this host's. The fields are public, so that eligibility can also be
/// checked at other times, such as with a margin before the window of a generation closes, and in
/// tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostTime {
    /// The identity of the host, as a time contract records it: its machine ID on Linux, and its
    /// machine GUID on Windows.
    pub host_id: [u8; 16],
    /// The host's boot.
    pub boot_id: BootId,
    /// The host's monotonic clock.
    pub clock: HostClock,
    /// The reading of [`clock`](Self::clock), in nanoseconds.
    pub monotonic_ns: u64,
    /// The host's UTC, in nanoseconds since the Unix epoch.
    pub utc_ns: u64,
}

impl HostTime {
    /// Samples the time of this host as OpenVMM samples it at a capture and at a restore: the
    /// identities of the host and of its boot, and its UTC between two readings of its monotonic
    /// clock, Linux's `CLOCK_BOOTTIME` or Windows' interrupt time, whose midpoint it keeps.
    ///
    /// Fails with [`ErrorKind::Unavailable`] on hosts other than Linux and Windows, whose clocks
    /// no capture records, and with [`ErrorKind::Io`] if an identity or a clock of this host
    /// cannot be read.
    pub fn now() -> Result<Self> {
        let clock = if cfg!(target_os = "linux") {
            HostClock::LinuxBoottime
        } else if cfg!(windows) {
            HostClock::WindowsInterruptTime
        } else {
            return Err(Error::new(
                ErrorKind::Unavailable,
                "this host has no clock that a capture records, so it restores no template",
            ));
        };
        let failed = |what: &'static str| {
            move |error: io::Error| Error::io(format!("cannot read {what}")).with_source(error)
        };
        let host_id = platform::host_id().map_err(failed("the identity of this host"))?;
        let boot_id = BootId::current().map_err(failed("the identity of this host's boot"))?;
        let monotonic = || platform::monotonic_ns().map_err(failed("this host's monotonic clock"));
        let before = monotonic()?;
        let utc_ns = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|since| u64::try_from(since.as_nanos()).ok())
            .ok_or_else(|| Error::io("this host's UTC is outside the range of a time contract"))?;
        let after = monotonic()?;
        let Some(elapsed) = after.checked_sub(before) else {
            return Err(Error::io("this host's monotonic clock went backwards"));
        };
        Ok(Self {
            host_id,
            boot_id,
            clock,
            monotonic_ns: before + elapsed / 2,
            utc_ns,
        })
    }
}

/// The source of the downtime of a capture, which a restore chooses from the host's time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DowntimeSource {
    /// The host's monotonic clock, on the host, in the boot, and by the clock of the capture.
    HostMonotonic,
    /// UTC, after a reboot, on another host, or by another clock.
    Utc,
}

impl fmt::Display for DowntimeSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::HostMonotonic => "the host's monotonic clock",
            Self::Utc => "UTC",
        })
    }
}

/// The downtime of a capture at a host's time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Downtime {
    /// The downtime from the anchor of the capture's time contract, in nanoseconds.
    pub nanos: u64,
    /// The interval by which a restore advances the clone's VM time: the downtime and the
    /// interval from the cut of the captured VM's time to the anchor, in nanoseconds, or
    /// `u64::MAX` if it is longer.
    pub vm_time_nanos: u64,
    /// The source of the downtime.
    pub source: DowntimeSource,
}

/// Whether a clone may restore a capture at a host's time, by the restore window of its time
/// contract, as [`TimeContract::eligibility`] decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Eligibility {
    /// A clone may restore it: its downtime is not negative, and the interval by which the
    /// restore advances the clone's VM time is at most [`MAX_DOWNTIME_NS`].
    Eligible(Downtime),
    /// Not yet: the host's time is before the anchor of the capture, so that its downtime is
    /// negative, as on a host whose UTC is behind the capturing host's.
    Early {
        /// The source of the downtime.
        source: DowntimeSource,
        /// How long before the anchor the host's time is, in nanoseconds.
        before_ns: u64,
    },
    /// No longer: the interval by which a restore would advance the clone's VM time is longer
    /// than [`MAX_DOWNTIME_NS`].
    Expired(Downtime),
}

impl Eligibility {
    /// Returns whether a clone may restore the capture.
    pub fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible(_))
    }
}

impl fmt::Display for Eligibility {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eligible(downtime) => write!(
                formatter,
                "eligible, after a downtime of {} ns by {}",
                downtime.nanos, downtime.source
            ),
            Self::Early { source, before_ns } => write!(
                formatter,
                "not eligible yet: by {source}, the host's time is {before_ns} ns before the \
                 anchor of the capture"
            ),
            Self::Expired(downtime) => write!(
                formatter,
                "no longer eligible: after a downtime of {} ns by {}, a restore would advance the \
                 clone's VM time by {} ns, more than the {MAX_DOWNTIME_NS} ns that it accepts",
                downtime.nanos, downtime.source, downtime.vm_time_nanos
            ),
        }
    }
}

impl TimeContract {
    /// Returns whether a clone may restore the capture of this time contract at the host's time
    /// `now`, as a restore decides it.
    ///
    /// A restore measures the downtime of the capture from its anchor: by the host's monotonic
    /// clock if `now` is of the host, the boot, and the clock that captured it, a clock that
    /// counts the time that the host was suspended, and that no change of the host's wall clock
    /// affects; and by UTC otherwise, after a reboot, on another host, or by another clock. It
    /// refuses a negative downtime, and a capture whose VM time it would advance, by the downtime
    /// and the interval from the cut of the VM's time to the anchor, by more than
    /// [`MAX_DOWNTIME_NS`].
    pub fn eligibility(&self, now: &HostTime) -> Eligibility {
        let same_boot = self.host_id == now.host_id
            && self.boot_id == now.boot_id
            && self.host_clock == now.clock;
        let (source, measured) = if same_boot {
            (
                DowntimeSource::HostMonotonic,
                i128::from(now.monotonic_ns) - i128::from(self.capture_monotonic_ns),
            )
        } else {
            (
                DowntimeSource::Utc,
                i128::from(now.utc_ns) - i128::from(self.capture_utc_ns),
            )
        };
        // Both readings are 64-bit, so a downtime that is not negative is one too.
        let Ok(nanos) = u64::try_from(measured) else {
            return Eligibility::Early {
                source,
                before_ns: u64::try_from(measured.unsigned_abs()).unwrap_or(u64::MAX),
            };
        };
        let vm_time_nanos = nanos.saturating_add(self.cut_to_anchor_ns.unwrap_or(0));
        let downtime = Downtime {
            nanos,
            vm_time_nanos,
            source,
        };
        if vm_time_nanos <= MAX_DOWNTIME_NS {
            Eligibility::Eligible(downtime)
        } else {
            Eligibility::Expired(downtime)
        }
    }
}

impl Generation {
    /// Returns whether a clone may restore the generation at the host's time `now`, by the
    /// restore window of its capture's time contract, as [`TimeContract::eligibility`] decides
    /// it.
    pub fn eligibility(&self, now: &HostTime) -> Eligibility {
        self.time().eligibility(now)
    }
}
