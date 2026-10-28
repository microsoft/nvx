//! A running workload, its output, cancellation, and outcome.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::session_code;
use crate::error::{Error, ErrorCode, Result};
use crate::host::session::{ControlSession, ExecEvent, SessionError};
use crate::protocol::{
    CWD_FAILED, ExitCategory, LAUNCH_FAILED, MAPPINGS_INCOMPLETE, MAX_OUTPUT_BYTES,
};

/// Longest delay between a cancellation request and its delivery to the guest.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Receives a workload's output.
pub trait OutputSink {
    /// Writes one chunk. After an error, the chunks of this stream are discarded, and the
    /// workload continues.
    fn write(&mut self, chunk: &[u8]) -> io::Result<()>;
}

impl<F: FnMut(&[u8]) -> io::Result<()>> OutputSink for F {
    fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        self(chunk)
    }
}

/// How a workload ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadOutcome {
    /// The workload exited with this code.
    Exited(i32),
    /// A signal with this number ended the workload.
    Signaled(i32),
    /// The workload's timeout elapsed, and the guest agent killed it.
    TimedOut,
    /// The workload exceeded [`MAX_OUTPUT_BYTES`], and the guest agent killed it.
    OutputLimitExceeded,
    /// The guest agent could not supervise the workload.
    Failed,
    /// A cancellation request ended the workload.
    Cancelled,
    /// The guest agent could not launch the workload.
    LaunchFailed,
    /// The workload could not enter its working directory, so nothing of it ran. The guest agent
    /// wrote a diagnostic that names the directory to its standard error.
    WorkingDirectory,
}

/// A workload that [`crate::host::instance::Instance::exec`] started.
pub struct Execution {
    /// Exclusive control session that carries this workload's traffic.
    pub(super) session: ControlSession,
    /// Identifier correlating requests, output, cancellation, and the final outcome.
    pub(super) request_id: u64,
    /// Workload response deadline, absent for a workload without a timeout.
    pub(super) deadline: Option<Instant>,
    /// Maximum time allowed for control traffic after cancellation or failure.
    pub(super) control_timeout: Duration,
}

impl Execution {
    /// Relays the workload's output to `stdout` and `stderr` and returns its outcome.
    ///
    /// Events are read in short slices, so that setting `cancel` reaches the guest within 20 ms.
    /// The guest then terminates the workload and reports the cancelled outcome as usual. Once
    /// cancellation is sent, a missing response fails within the control timeout of
    /// [`crate::host::instance::Instance::exec`], or the earlier workload-response deadline, even when the workload has
    /// no timeout. The guest agent limits the combined output to [`MAX_OUTPUT_BYTES`], and a
    /// guest that sends more fails the execution. If the process that pumps an execution dies,
    /// the guest kills the workload as soon as it notices that the control session is gone.
    pub fn pump(
        mut self,
        stdout: &mut dyn OutputSink,
        stderr: &mut dyn OutputSink,
        cancel: &AtomicBool,
    ) -> Result<WorkloadOutcome> {
        let mut deadline = self.deadline;
        let mut forwarded = 0usize;
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut cancel_sent = false;
        loop {
            if !cancel_sent && cancel.load(Ordering::Acquire) {
                let cancellation_deadline = Instant::now() + self.control_timeout;
                self.session
                    .cancel_exec(self.request_id, cancellation_deadline)
                    .map_err(|error| {
                        Error::new(
                            session_code(&error),
                            format!("cannot deliver the cancellation request: {error}"),
                        )
                        .with_source(error)
                    })?;
                deadline = Some(deadline.map_or(cancellation_deadline, |deadline| {
                    deadline.min(cancellation_deadline)
                }));
                cancel_sent = true;
            }
            let slice = Instant::now() + CANCEL_POLL_INTERVAL;
            let poll_deadline = if cancel_sent {
                deadline
            } else {
                Some(deadline.map_or(slice, |deadline| deadline.min(slice)))
            };
            let event = match self.session.next_exec_event(self.request_id, poll_deadline) {
                Err(SessionError::TimedOut)
                    if poll_deadline != deadline
                        && deadline.is_none_or(|deadline| Instant::now() < deadline) =>
                {
                    continue;
                }
                event => event,
            };
            let event = event.map_err(|error| match error {
                SessionError::TimedOut => {
                    Error::timed_out("timed out waiting for the workload outcome")
                }
                ended @ (SessionError::Closed | SessionError::Reset) => Error::new(
                    session_code(&ended),
                    "the control session ended before the workload finished; the sandbox VM may \
                     have stopped",
                ),
                other => Error::new(
                    session_code(&other),
                    format!("the control session failed: {other}"),
                )
                .with_source(other),
            })?;
            let (chunk, sink, open): (Vec<u8>, &mut dyn OutputSink, &mut bool) = match event {
                ExecEvent::Stdout(chunk) => (chunk, &mut *stdout, &mut stdout_open),
                ExecEvent::Stderr(chunk) => (chunk, &mut *stderr, &mut stderr_open),
                ExecEvent::Exit { category, status } => {
                    return Ok(match category {
                        ExitCategory::Exit => WorkloadOutcome::Exited(status),
                        ExitCategory::Signal => WorkloadOutcome::Signaled(if status > 128 {
                            status - 128
                        } else {
                            status
                        }),
                        ExitCategory::Timeout => WorkloadOutcome::TimedOut,
                        ExitCategory::OutputLimit => WorkloadOutcome::OutputLimitExceeded,
                        ExitCategory::Failed => WorkloadOutcome::Failed,
                        ExitCategory::Cancelled => WorkloadOutcome::Cancelled,
                    });
                }
                ExecEvent::Rejected { status, category } => {
                    return match category.as_str() {
                        LAUNCH_FAILED => Ok(WorkloadOutcome::LaunchFailed),
                        // The guest already wrote a diagnostic that names the directory to stderr.
                        CWD_FAILED => Ok(WorkloadOutcome::WorkingDirectory),
                        // Only a start that was interrupted leaves a guest without its mappings.
                        MAPPINGS_INCOMPLETE => Err(Error::new(
                            ErrorCode::ExecRefused,
                            "the guest has not mounted every mapped host path, because its start \
                             did not complete; stop the sandbox and start it again",
                        )),
                        _ => Err(Error::new(
                            ErrorCode::ExecRefused,
                            format!(
                                "the guest agent rejected the workload: {category} (status \
                                 {status})"
                            ),
                        )),
                    };
                }
            };
            forwarded = forwarded.saturating_add(chunk.len());
            if forwarded > MAX_OUTPUT_BYTES {
                return Err(Error::protocol(
                    "the guest exceeded the output limit of the control protocol",
                ));
            }
            if *open && sink.write(&chunk).is_err() {
                *open = false;
            }
        }
    }
}
