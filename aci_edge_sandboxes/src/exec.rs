use std::fmt;
use std::io::{self, PipeReader, PipeWriter, Read};
use std::sync::Arc;
#[cfg(any(feature = "openvmm", feature = "testing"))]
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;

use crate::backend::ExecControl;
use crate::error::{Error, Result};

#[cfg(any(feature = "openvmm", feature = "testing"))]
#[derive(Default)]
enum CompletionState {
    #[default]
    Running,
    Finished(Result<ExecOutcome>),
    Collected,
}

#[cfg(any(feature = "openvmm", feature = "testing"))]
#[derive(Default)]
pub(crate) struct Completion {
    state: Mutex<CompletionState>,
    finished: Condvar,
}

#[cfg(any(feature = "openvmm", feature = "testing"))]
impl Completion {
    pub(crate) fn finish(&self, outcome: Result<ExecOutcome>) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) =
            CompletionState::Finished(outcome);
        self.finished.notify_all();
    }

    pub(crate) fn wait(&self) -> Result<ExecOutcome> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            match std::mem::replace(&mut *state, CompletionState::Collected) {
                CompletionState::Running => {
                    *state = CompletionState::Running;
                    state = self
                        .finished
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                CompletionState::Finished(outcome) => return outcome,
                CompletionState::Collected => {
                    return Err(Error::backend_error(
                        "the execution outcome was already collected",
                    ));
                }
            }
        }
    }
}

/// Terminal outcome of an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExecOutcome {
    /// The workload exited with this status code.
    Exited(i32),
    /// The workload was terminated by this signal number.
    Signaled(i32),
    /// The workload overran `process.timeout` and is no longer running.
    TimedOut,
    /// The workload was cancelled through its [`Canceller`] and is no longer running.
    Cancelled,
    /// The workload could not run to completion.
    Failed(ExecFailure),
}

impl ExecOutcome {
    /// Returns whether the workload exited with status zero.
    pub fn success(self) -> bool {
        self == Self::Exited(0)
    }

    /// Returns the exit status of an [`ExecOutcome::Exited`] workload.
    pub fn exit_code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(code),
            _ => None,
        }
    }
}

impl fmt::Display for ExecOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exited(code) => write!(formatter, "exited with status {code}"),
            Self::Signaled(signal) => write!(formatter, "terminated by signal {signal}"),
            Self::TimedOut => formatter.write_str("timed out"),
            Self::Cancelled => formatter.write_str("cancelled"),
            Self::Failed(failure) => write!(formatter, "failed: {failure}"),
        }
    }
}

/// Reason an execution failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExecFailure {
    /// The sandbox could not launch the workload.
    LaunchFailed,
    /// The workload's working directory (`process.cwd`, or the backend's default) does not exist,
    /// is not a directory, or is not accessible to the workload, so the workload did not run.
    WorkingDirectory,
    /// The workload exceeded the backend's output limit and was terminated.
    OutputLimitExceeded,
    /// The sandbox lost track of the workload's exit status.
    Workload,
}

impl fmt::Display for ExecFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::LaunchFailed => "the workload could not be launched",
            Self::WorkingDirectory => {
                "the workload's working directory does not exist, is not a directory, or is not \
                 accessible to the workload"
            }
            Self::OutputLimitExceeded => "the workload exceeded the output limit",
            Self::Workload => "the workload's exit status could not be determined",
        })
    }
}

/// Collected result of [`Execution::wait_with_output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    /// Terminal outcome.
    pub outcome: ExecOutcome,
    /// Everything the workload wrote to standard output.
    pub stdout: Vec<u8>,
    /// Everything the workload wrote to standard error.
    pub stderr: Vec<u8>,
}

/// Cancels a live execution. Clones share the same execution.
#[derive(Clone)]
pub struct Canceller {
    control: Arc<dyn ExecControl>,
}

impl Canceller {
    pub(crate) fn new(control: Arc<dyn ExecControl>) -> Self {
        Self { control }
    }

    /// Requests cancellation of the execution.
    ///
    /// Backends that cannot cancel return [`ErrorCode::Unsupported`](crate::ErrorCode::Unsupported);
    /// see [`ExecCapabilities::cancel`](crate::ExecCapabilities::cancel).
    pub fn cancel(&self) -> Result<()> {
        self.control.cancel()
    }
}

impl fmt::Debug for Canceller {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Canceller").finish_non_exhaustive()
    }
}

/// A live execution returned by [`AciEdgeSandbox::exec`](crate::AciEdgeSandbox::exec).
///
/// Output arrives on operating-system pipes, so callers can hand the raw handles to other
/// components. Read both streams concurrently, or use [`Execution::wait_with_output`]; a stream
/// that is never taken is discarded when the execution is waited on.
pub struct Execution {
    stdout: Option<PipeReader>,
    stderr: Option<PipeReader>,
    stdin: Option<PipeWriter>,
    control: Arc<dyn ExecControl>,
}

impl Execution {
    pub(crate) fn new(
        stdout: PipeReader,
        stderr: PipeReader,
        stdin: Option<PipeWriter>,
        control: Arc<dyn ExecControl>,
    ) -> Self {
        Self {
            stdout: Some(stdout),
            stderr: Some(stderr),
            stdin,
            control,
        }
    }

    /// Takes the read end of the workload's standard output.
    pub fn take_stdout(&mut self) -> Option<PipeReader> {
        self.stdout.take()
    }

    /// Takes the read end of the workload's standard error.
    pub fn take_stderr(&mut self) -> Option<PipeReader> {
        self.stderr.take()
    }

    /// Takes the write end of the workload's standard input.
    ///
    /// Present only for [`StdinMode::Piped`](crate::StdinMode::Piped) requests. Drop the writer to
    /// deliver end-of-file.
    pub fn take_stdin(&mut self) -> Option<PipeWriter> {
        self.stdin.take()
    }

    /// Returns a handle that cancels this execution from any thread.
    pub fn canceller(&self) -> Canceller {
        Canceller::new(Arc::clone(&self.control))
    }

    /// Waits for the terminal outcome, discarding any output stream that was not taken.
    pub fn wait(mut self) -> Result<ExecOutcome> {
        self.stdin = None;
        self.stdout = None;
        self.stderr = None;
        self.control.wait()
    }

    /// Collects every untaken output stream and waits for the terminal outcome.
    pub fn wait_with_output(mut self) -> Result<ExecOutput> {
        self.stdin = None;
        let stdout = spawn_collector(self.stdout.take())?;
        let stderr = spawn_collector(self.stderr.take())?;
        let outcome = self.control.wait();
        let stdout = join_collector(stdout)?;
        let stderr = join_collector(stderr)?;
        Ok(ExecOutput {
            outcome: outcome?,
            stdout,
            stderr,
        })
    }
}

impl fmt::Debug for Execution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Execution")
            .field("stdout", &self.stdout.is_some())
            .field("stderr", &self.stderr.is_some())
            .field("stdin", &self.stdin.is_some())
            .finish_non_exhaustive()
    }
}

type Collector = Option<thread::JoinHandle<io::Result<Vec<u8>>>>;

fn spawn_collector(reader: Option<PipeReader>) -> Result<Collector> {
    let Some(mut reader) = reader else {
        return Ok(None);
    };
    thread::Builder::new()
        .name("nvx-exec-collect".to_owned())
        .spawn(move || {
            let mut output = Vec::new();
            reader.read_to_end(&mut output)?;
            Ok(output)
        })
        .map(Some)
        .map_err(|error| {
            Error::backend_error("failed to start an output collector thread").with_source(error)
        })
}

fn join_collector(collector: Collector) -> Result<Vec<u8>> {
    let Some(collector) = collector else {
        return Ok(Vec::new());
    };
    match collector.join() {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            Err(Error::backend_error("failed to read execution output").with_source(error))
        }
        Err(_) => Err(Error::backend_error("output collector thread panicked")),
    }
}
