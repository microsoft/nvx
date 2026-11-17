//! Exit statuses and failures shared by the guest programs, and the entry point of their `main`.
//!
//! The programs keep the command lines, exit statuses, and output of the C programs that they
//! replace, except that numeric arguments are plain decimal, with a `-` sign where `nvx-time`'s
//! test hooks take negative numbers, or `0x`-prefixed hexadecimal where the C program also read
//! hexadecimal, floating-point arguments plain decimal with an optional exponent, error messages
//! describe `errno` values with `nix`'s text, and the programs read the kernel's text, such as
//! its socket tables, sysctls, rates, CPU lists, and timer list, their own records, and their
//! test fixtures only in the form in which they are written, rejecting the signed, prefixed, and
//! other lenient forms that C's `strtoul`, `strtod`, and `scanf` also read. Nor do they keep
//! what only the C programs' fixed buffers and string handling did to oversized or malformed
//! input, such as a length limit on a valid CPU list, an unknown option that a later valid one
//! masked, or a NUL byte that ended a diagnostic.
//!
//! # Conventions
//!
//! The programs and the libraries of the guest module keep these conventions:
//!
//! - Program and library modules deny unsafe code. Only the private `sys` module may use it,
//!   behind documented safe operations. It makes every system call through `nix`, with the
//!   enabled, default-free feature set, or through the standard library, which reads and writes
//!   whole files and the MSR devices, except the few spawn, managed-agent, and time-tool
//!   operations that neither provides, which `sys` documents, and the programs take random bytes
//!   from the `getrandom` crate. `sys` also names the kernel's and the processor's interfaces
//!   once: the procfs, sysfs, and device paths, OpenVMM's I/O ports, CPUID leaves and MSRs, and
//!   the kernel's flags, so that the programs refer to them by name.
//! - Descriptors and mappings have owners that release them when dropped. Where a C program
//!   reported a cleanup failure, a consuming `close` or `unmap` returns it instead. Raw
//!   descriptor integers never leave `sys`, and no resource is dropped by hand: where a C
//!   program released one before a later step, its scope ends there, for example in a helper
//!   function that returns what the later step needs. State that a program changes for a while,
//!   such as a lock, a signal mask, a thread's CPU affinity or scheduling policy, a mount, a
//!   created file, or a kernel setting, likewise has an owner that restores it when dropped.
//! - A descriptor stays with the library that opened it: one that `sys` opens through `nix` is
//!   read and written only through `sys`, and one that the standard library opens only through
//!   it, except that a test may hand one to `FileDescriptor::from_owned`, which takes it over
//!   whole. No descriptor thus has two owners or two buffers; a standard-library adapter such as
//!   `BufReader` may add the one buffer of a `sys` descriptor through the `std::io::Read` that
//!   `FileDescriptor` implements, and a caller that takes `std::io::Write`, such as
//!   [`nvxs`](super::nvxs), writes one through the `std::io::Write` that it implements, which
//!   buffers nothing. `sys::Stdout` and `sys::DirectStdout` write standard output directly, past
//!   the standard library's line buffer, so the programs never print with the standard library's
//!   macros: `print!` would reorder its buffered output with theirs, and `eprint!` would bypass
//!   the error streams that the programs' tests capture. Clippy denies both in the guest module.
//! - Every program fails with the shared [`CommandError`], whose kind selects the program's exit
//!   status from the shared [`ExitStatus`]. Every module, type, field, variant, constant, static,
//!   and function is documented, which CI enforces with `clippy::missing_docs_in_private_items`.
//! - The programs use `nix`, `getrandom`, or the standard library rather than reimplement a
//!   facility, and keep a parser of their own only for forms that no library parses.

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
    TransferFailure = 4,
    /// The managed agent, or its execution helper before it executed the workload, failed.
    AgentFailure = 125,
    /// A benchmark could not write its result.
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
    Transfer,
    /// A benchmark could not write its result.
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
            Err(CommandError::Transfer) => Self::TransferFailure,
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
