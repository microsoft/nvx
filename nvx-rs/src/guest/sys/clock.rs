//! The kernel's clocks: reading them and sleeping on them.

#[cfg(any(target_arch = "x86_64", test))]
use std::time::Duration;

use nix::sys::time::TimeSpec;
use nix::sys::time::TimeValLike;
use nix::time::{ClockId, clock_gettime};
use nix::time::{ClockNanosleepFlags, clock_nanosleep};

use super::Errno;

/// Nanoseconds in a second, which scale the clock's whole seconds.
const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;

/// Nanoseconds in a millisecond, which `monotonic_ms` divides away.
const NANOSECONDS_PER_MILLISECOND: u64 = 1_000_000;

/// A clock that the guest programs read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Clock {
    /// The wall clock, which the time tools step and slew to the host's.
    Realtime,
    /// The time since boot, without suspend, which the wall clock's discipline slews.
    Monotonic,
    /// The time since boot, without suspend or discipline.
    #[cfg(any(target_arch = "x86_64", test))]
    MonotonicRaw,
    /// The monotonic clock as the kernel saved it at the last scheduler tick, which a read
    /// returns without reading the clocksource, so that it advances once per tick.
    MonotonicCoarse,
    /// The time since boot, with suspend.
    Boottime,
    /// The CPU time of every thread of the process, exited threads' included.
    ProcessCpuTime,
}

impl Clock {
    /// Returns the kernel's identifier of the clock.
    const fn id(self) -> ClockId {
        match self {
            Self::Realtime => ClockId::CLOCK_REALTIME,
            Self::Monotonic => ClockId::CLOCK_MONOTONIC,
            #[cfg(any(target_arch = "x86_64", test))]
            Self::MonotonicRaw => ClockId::CLOCK_MONOTONIC_RAW,
            Self::MonotonicCoarse => ClockId::CLOCK_MONOTONIC_COARSE,
            Self::Boottime => ClockId::CLOCK_BOOTTIME,
            Self::ProcessCpuTime => ClockId::CLOCK_PROCESS_CPUTIME_ID,
        }
    }
}

/// Reads `clock`, through the vDSO where the kernel provides one.
pub(crate) fn clock_time(clock: Clock) -> Result<TimeSpec, Errno> {
    clock_gettime(clock.id())
}

/// Returns the monotonic clock in nanoseconds, wrapping as the C programs' `u64` arithmetic did.
///
/// The C library reads this clock through the kernel's vDSO, without a system call. The C
/// `nvx-device-io` made the system call itself, so each read here costs less than there, and the
/// benchmark loops that read the clock can complete slightly more operations. The clock never
/// moves backwards, and its precision is that of the guest kernel's clocksource.
pub(crate) fn monotonic_ns() -> Result<u64, Errno> {
    let time = clock_gettime(ClockId::CLOCK_MONOTONIC)?;
    Ok((time.tv_sec() as u64)
        .wrapping_mul(NANOSECONDS_PER_SECOND)
        .wrapping_add(time.tv_nsec() as u64))
}

/// Returns the monotonic clock in whole milliseconds.
///
/// It equals the C programs' `tv_sec * 1000 + tv_nsec / 1000000` until the nanosecond count
/// wraps, 584 years after the clock's start.
pub(crate) fn monotonic_ms() -> Result<u64, Errno> {
    monotonic_ns().map(|nanoseconds| nanoseconds / NANOSECONDS_PER_MILLISECOND)
}

/// Sleeps until the monotonic clock reaches `deadline_ns`, resuming after interruptions.
///
/// A deadline that has passed returns at once.
pub(crate) fn sleep_until_monotonic(deadline_ns: i64) {
    let deadline = TimeSpec::nanoseconds(deadline_ns);
    while clock_nanosleep(
        ClockId::CLOCK_MONOTONIC,
        ClockNanosleepFlags::TIMER_ABSTIME,
        &deadline,
    ) == Err(Errno::EINTR)
    {}
}

/// Sleeps for `duration` on the monotonic clock with one call, which a signal ends early.
#[cfg(any(target_arch = "x86_64", test))]
pub(crate) fn sleep_once(duration: Duration) {
    let _ = clock_nanosleep(
        ClockId::CLOCK_MONOTONIC,
        ClockNanosleepFlags::empty(),
        &TimeSpec::from_duration(duration),
    );
}

/// Reads the monotonic clock with the `clock_gettime` system call itself and returns the
/// nanoseconds within the second.
///
/// The latency probe measures this system call. `nix` and the C library read the clock through
/// the vDSO, without one, so this makes it through libc's `syscall`.
#[cfg(target_arch = "x86_64")]
pub(crate) fn syscall_monotonic_nanoseconds() -> u64 {
    let mut time = nix::libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the system call writes one `timespec` to `time`, which outlives it.
    unsafe {
        nix::libc::syscall(
            nix::libc::SYS_clock_gettime,
            nix::libc::CLOCK_MONOTONIC,
            &raw mut time,
        )
    };
    time.tv_nsec as u64
}

#[cfg(test)]
mod tests;
