//! `nvx-time`, the guest side of NVX time ABI v1.
//!
//! It runs the conformance checks, the violation watcher and the wall-clock discipline in one
//! daemon, and the snapshot agent's time steps: the guest preparation steps of the architecture
//! guide's section 5.3.2 and the guest repair steps of its section 5.5.7, which the comments cite
//! by number. Section 3.2.10 is the contract that it enforces. It prints nothing on success:
//! checks record their results in the state file, and only violation events reach the console.
//! It keeps the check IDs, the guest codes, and the formats of the output lines and the state
//! file of the C program that it replaced.
//!
//! `nvx-time test NAME ARGUMENTS...` runs one of the C program's test hooks, which exercise its
//! decoders, formats, and process structure without the hypervisor.

mod checks;
mod commands;
mod cpu_mask;
mod daemon;
mod decoders;
mod events;
mod state;
mod system;
mod test_mode;

use std::ffi::CString;
use std::io::{self, Write};
use std::process::ExitCode;

use super::status::{CommandError, ExitStatus};
use super::sys;

/// Nanoseconds in one second.
pub(crate) const NSEC_PER_SEC: i64 = 1_000_000_000;
/// Nanoseconds in one millisecond.
pub(crate) const NSEC_PER_MSEC: i64 = 1_000_000;
/// Nanoseconds in one microsecond.
pub(crate) const NSEC_PER_USEC: i64 = 1_000;
/// Maximum processor count represented by the time ABI's fixed CPU masks.
pub(crate) const MAX_CPUS: usize = 64;
/// Delay between restore readiness and deferred check work.
pub(crate) const DEFERRED_START_NS: i64 = 150 * NSEC_PER_MSEC;
/// Deadline after which idle-priority work is promoted.
pub(crate) const IDLE_BOUND_NS: i64 = 100 * NSEC_PER_MSEC;
/// Most arguments that a command line may hold besides `--report-only`, as many as the C
/// program's fixed argument array.
const MAX_ARGUMENTS: usize = 32;

/// The phase in which a check or event occurs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Initial guest boot.
    Boot,
    /// Snapshot capture preparation.
    Capture,
    /// Post-snapshot restore repair.
    Restore,
    /// Continuous daemon operation.
    Runtime,
}

impl Phase {
    /// Persisted check phases in state-file order.
    pub(crate) const CHECKS: [Self; 3] = [Self::Boot, Self::Capture, Self::Restore];

    /// Returns the stable lowercase phase token used in output records.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Capture => "capture",
            Self::Restore => "restore",
            Self::Runtime => "runtime",
        }
    }

    /// Returns this phase's position in the three persisted check records.
    pub(crate) fn check_index(self) -> Option<usize> {
        Self::CHECKS.iter().position(|&phase| phase == self)
    }
}

/// Byte-exact command help printed for malformed invocations.
const USAGE: &[u8] = br#"usage: nvx-time [--report-only] COMMAND
  boot
  check --phase capture|restore [--new-cpus LIST]
  pre-capture
  capture 0|1 GENERATION_ID ENTROPY_FILE [--finish]
  cancel-capture
  restore-finish [--ack] [--new-cpus LIST]
  status
  sample
  generation-id
  exhaustive
  test NAME ARGUMENTS...
"#;

/// Runs `nvx-time` with the process's arguments. It exits with 0 on success, 1 when a command
/// or a check fails, or 2 for an invalid command line.
pub fn main() -> ExitCode {
    sys::restore_sigpipe();
    ExitStatus::from(run(
        &sys::args(),
        &mut sys::Stdout::new(),
        &mut io::stderr(),
    ))
    .into()
}

/// Dispatches one argument vector, program name first, to a command.
fn run(args: &[CString], out: &mut dyn Write, err: &mut dyn Write) -> Result<(), CommandError> {
    let mut arguments = Vec::with_capacity(args.len().min(MAX_ARGUMENTS));
    let mut report_only = false;
    for argument in args.iter().skip(1) {
        if argument.as_bytes() == b"--report-only" {
            report_only = true;
        } else if arguments.len() < MAX_ARGUMENTS {
            arguments.push(argument.as_bytes());
        } else {
            sys::report(err, &[USAGE]);
            return Err(CommandError::Usage);
        }
    }
    let Some((&command, rest)) = arguments.split_first() else {
        sys::report(err, &[USAGE]);
        return Err(CommandError::Usage);
    };
    if command == b"test" {
        return test_mode::run(rest, out);
    }
    commands::run(&arguments, report_only, out, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&[u8]]) -> Vec<CString> {
        std::iter::once(CString::new("nvx-time").unwrap())
            .chain(values.iter().map(|value| CString::new(*value).unwrap()))
            .collect()
    }

    #[test]
    fn usage_is_byte_exact_and_bounds_the_arguments() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run(&args(&[]), &mut out, &mut err),
            Err(CommandError::Usage)
        );
        assert_eq!(err, USAGE);
        err.clear();
        let many = vec![b"x".as_slice(); MAX_ARGUMENTS + 1];
        assert_eq!(
            run(&args(&many), &mut out, &mut err),
            Err(CommandError::Usage)
        );
        assert_eq!(err, USAGE);
        assert!(out.is_empty());
    }

    #[test]
    fn checked_phases_have_state_file_positions() {
        let positions = Phase::CHECKS.map(Phase::check_index);
        assert_eq!(positions, [Some(0), Some(1), Some(2)]);
        assert_eq!(Phase::Runtime.check_index(), None);
    }
}
