//! The existing bounded idle-priority policy for deferred time conformance work.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::guest::sys::{self, TimeValLike};

/// Existing delay before asynchronous conformance work starts.
pub(crate) const DEFERRED_START_NS: i64 = 150_000_000;
/// Existing maximum idle-priority interval before promotion to normal priority.
pub(crate) const IDLE_BOUND_NS: i64 = 100_000_000;
/// Existing maximum wait for timer-list convergence.
pub(crate) const TIMER_LIST_WAIT: Duration = Duration::from_millis(200);
/// Existing timer-list convergence polling interval.
pub(crate) const TIMER_LIST_POLL: Duration = Duration::from_millis(10);
/// Existing maximum wait for published phase checks.
pub(crate) const STATUS_WAIT: Duration = Duration::from_secs(30);
/// Existing interval while waiting for published phase checks.
pub(crate) const STATUS_POLL: Duration = Duration::from_millis(10);

/// Join-on-drop owner of the existing idle-to-normal promotion timer.
pub(crate) struct Promoter {
    /// Disconnecting this sender cancels a pending promotion.
    cancel: Option<mpsc::Sender<()>>,
    /// The owned promoter thread.
    thread: Option<thread::JoinHandle<()>>,
}

impl Promoter {
    /// Starts the existing promotion deadline only after all platform authority is released.
    pub(crate) fn start(start_ns: i64) -> Option<Self> {
        sys::check_released().ok()?;
        let (cancel, cancelled) = mpsc::channel();
        let deadline = start_ns.wrapping_add(IDLE_BOUND_NS);
        let thread = thread::Builder::new()
            .name("nvx-time-promote".to_owned())
            .spawn(move || {
                let now =
                    sys::clock_time(sys::Clock::Monotonic).map_or(0, |time| time.num_nanoseconds());
                let remaining = deadline.wrapping_sub(now);
                let timeout = Duration::from_nanos(remaining.max(0) as u64);
                if cancelled.recv_timeout(timeout) == Err(RecvTimeoutError::Timeout) {
                    sys::promote_all_threads();
                }
            })
            .ok()?;
        Some(Self {
            cancel: Some(cancel),
            thread: Some(thread),
        })
    }

    /// Waits for the existing promotion deadline.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn finish(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Promoter {
    fn drop(&mut self) {
        drop(self.cancel.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
