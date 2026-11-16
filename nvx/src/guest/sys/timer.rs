//! An owned monotonic timer descriptor.

use std::os::fd::{AsFd, BorrowedFd};
use std::time::Duration;

use nix::sys::time::TimeSpec;
use nix::sys::timer::Expiration;
use nix::sys::timerfd::{ClockId, TimerFd, TimerFlags, TimerSetTimeFlags};

use super::Errno;

/// A close-on-exec, nonblocking timer descriptor on the monotonic clock, which polls readable
/// once the timer expires. Dropping it closes the descriptor.
#[derive(Debug)]
pub(crate) struct Timer(TimerFd);

impl Timer {
    /// Creates a disarmed timer.
    pub(crate) fn new() -> Result<Self, Errno> {
        TimerFd::new(
            ClockId::CLOCK_MONOTONIC,
            TimerFlags::TFD_CLOEXEC | TimerFlags::TFD_NONBLOCK,
        )
        .map(Self)
    }

    /// Arms the timer to expire once, `delay` from now, replacing any earlier expiration.
    pub(crate) fn arm_after(&self, delay: Duration) -> Result<(), Errno> {
        self.0.set(
            Expiration::OneShot(TimeSpec::from_duration(delay)),
            TimerSetTimeFlags::empty(),
        )
    }

    /// Consumes the timer's expiration count, if it expired, without blocking.
    pub(crate) fn consume(&self) {
        let _ = self.0.wait();
    }

    /// Borrows the descriptor for `poll` in this module tree.
    pub(super) fn borrowed(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(test)]
mod tests;
