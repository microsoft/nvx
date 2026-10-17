//! Deadlines with Python's float seconds, as `nvx.py` computes them with `time.monotonic()`.

use std::thread;
use std::time::{Duration, Instant};

use super::{Result, check_interrupt};

/// The longest that a wait sleeps between checks for Ctrl-C and for the condition it awaits.
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// A point in time `timeout` seconds after it was taken, or none when that is beyond what an
/// [`Instant`] holds, which then never passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deadline(Option<Instant>);

impl Deadline {
    /// Returns the deadline `seconds` from now: `time.monotonic() + seconds`.
    pub(crate) fn after(seconds: f64) -> Self {
        Self(
            Duration::try_from_secs_f64(seconds)
                .ok()
                .and_then(|duration| Instant::now().checked_add(duration)),
        )
    }

    /// Returns the time left, which is zero once the deadline has passed, or `None` if the
    /// deadline never passes.
    pub(crate) fn remaining(self) -> Option<Duration> {
        self.0
            .map(|instant| instant.saturating_duration_since(Instant::now()))
    }

    /// Returns whether the deadline has passed: `time.monotonic() >= deadline`.
    pub(crate) fn passed(self) -> bool {
        self.0.is_some_and(|instant| Instant::now() >= instant)
    }

    /// Returns the deadline as an [`Instant`], which for a deadline that never passes is a
    /// century away.
    pub(crate) fn instant(self) -> Instant {
        self.0.unwrap_or_else(|| {
            let now = Instant::now();
            now.checked_add(Duration::from_secs(100 * 365 * 24 * 3600))
                .unwrap_or(now)
        })
    }

    /// Waits for the next retry of a step that the deadline bounds, as `_pause` does: returns
    /// `false` at once if the deadline has passed, and otherwise sleeps for `interval` or the
    /// time left, whichever is shorter, and returns whether time is left. Fails if Ctrl-C
    /// arrives.
    pub(crate) fn pause(self, interval: Duration) -> Result<bool> {
        let remaining = self.remaining();
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            return Ok(false);
        }
        sleep(remaining.map_or(interval, |remaining| remaining.min(interval)))?;
        Ok(!self.passed())
    }
}

/// Sleeps for `duration`, as `time.sleep` does, failing if Ctrl-C arrives meanwhile.
pub(crate) fn sleep(duration: Duration) -> Result {
    let end = Instant::now().checked_add(duration);
    loop {
        check_interrupt()?;
        let left = end.map_or(RETRY_INTERVAL, |end| {
            end.saturating_duration_since(Instant::now())
        });
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(RETRY_INTERVAL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_pass_after_their_seconds() {
        assert!(!Deadline::after(60.0).passed());
        assert!(Deadline::after(1e-300).passed());
        let unbounded = Deadline::after(1e300);
        assert_eq!(unbounded.remaining(), None);
        assert!(!unbounded.passed());
        assert!(unbounded.instant() > Instant::now());
        let short = Deadline::after(0.01);
        thread::sleep(Duration::from_millis(20));
        assert!(!short.pause(RETRY_INTERVAL).unwrap());
        assert_eq!(short.remaining(), Some(Duration::ZERO));
    }
}
