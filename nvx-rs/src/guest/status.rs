//! Exit statuses and failures shared by the guest programs, and the entry point of their `main`.

use std::ffi::CString;
use std::process::ExitCode;

use super::sys;

/// Process exit statuses of the guest programs.
///
/// A program exits with [`Success`](Self::Success), or with the status that its
/// [`CommandError`] selects. Only `nvx-device-io` and `nvx-time-probe` exit with 3, only
/// `nvx-device-io` with the benchmark statuses 4 and 255, and only `nvx-managed-agent` with 125.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ExitStatus {
    /// The program completed.
    Success = 0,
    /// An operation failed.
    Failure = 1,
    /// The command line was invalid.
    Usage = 2,
    /// A benchmark could not open, connect, size, or time its device, or a diagnostic could not
    /// prepare or run its measurement.
    SetupFailure = 3,
    /// A benchmark transfer or send was incomplete.
    #[cfg(any(target_arch = "x86_64", test))]
    TransferFailure = 4,
    /// The managed agent, or its execution helper before it executed the workload, failed.
    AgentFailure = 125,
    /// A benchmark could not write its result.
    #[cfg(any(target_arch = "x86_64", test))]
    OutputFailure = 255,
}

/// Typed failure of a guest program, which selects its exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandError {
    /// The command line is invalid. A program with a usage message has printed it.
    Usage,
    /// An operation failed. A program that reports failures has printed why.
    Operation,
    /// A benchmark could not open, connect, size, or time its device, or a diagnostic could not
    /// prepare or run its measurement.
    Setup,
    /// A benchmark transfer or send was incomplete.
    #[cfg(any(target_arch = "x86_64", test))]
    Transfer,
    /// A benchmark could not write its result.
    #[cfg(any(target_arch = "x86_64", test))]
    Output,
    /// The managed agent or its execution helper failed for any reason, including an invalid
    /// command line. It has reported why where its C program did.
    Agent,
}

impl From<Result<(), CommandError>> for ExitStatus {
    fn from(result: Result<(), CommandError>) -> Self {
        match result {
            Ok(()) => Self::Success,
            Err(CommandError::Usage) => Self::Usage,
            Err(CommandError::Operation) => Self::Failure,
            Err(CommandError::Setup) => Self::SetupFailure,
            #[cfg(any(target_arch = "x86_64", test))]
            Err(CommandError::Transfer) => Self::TransferFailure,
            #[cfg(any(target_arch = "x86_64", test))]
            Err(CommandError::Output) => Self::OutputFailure,
            Err(CommandError::Agent) => Self::AgentFailure,
        }
    }
}

impl From<ExitStatus> for ExitCode {
    fn from(status: ExitStatus) -> Self {
        Self::from(status as u8)
    }
}

/// Runs a guest program as its `main` does: restores the `SIGPIPE` disposition that the process
/// inherited, passes `run` the process's arguments, program name first, and returns the exit code
/// that its outcome selects.
pub(crate) fn run_main(run: impl FnOnce(&[CString]) -> Result<(), CommandError>) -> ExitCode {
    sys::restore_sigpipe();
    ExitStatus::from(run(&sys::args())).into()
}

#[cfg(test)]
pub(crate) mod tests;
