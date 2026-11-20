//! `nvx-time`, the guest side of NVX's time ABI, and the typed calls through which any
//! guest init or agent runs the ABI's boot steps in its own process.
//!
//! [`TimeAbi`] runs the cold-boot time step and, separately, starts the time daemon, a process
//! that the caller supervises; it reads the VM generation ID, with typed results and errors.
//! Each call scopes its actual hardware access to its transactions: x86 I/O-port permissions on
//! the calling thread, or ARM's restricted platform descriptor. No call waits for a child that
//! it did not start:
//!
//! ```no_run
//! use nvx::guest::time::TimeAbi;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let abi = TimeAbi::new();
//! // At boot, before the process starts any thread.
//! let daemon = abi.start_daemon(abi.cold_boot()?)?;
//! // The caller supervises the process `daemon`.
//! # let _ = daemon;
//! # Ok(())
//! # }
//! ```
//!
//! On x86-64, the ABI's capture request, which repairs the time of a restore, and the restore's
//! finish are private: `guest::snapshot::capture` runs them, and hands the
//! restore's fresh entropy to its repair in memory, as an owned
//! [`Seed`](crate::guest::reseed::Seed), while `nvx-time capture` writes it to a file. `nvx-time`
//! runs each step through the same calls, and the rest of the ABI itself. ARM cold boot does
//! not admit capture or restore.
//!
//! A violation of the ABI stops the VM in enforcing mode, the mode of the command tools, with a
//! status that `nvx::protocol::status` publishes: 193 for a conformance check, 194 for the time
//! daemon's watcher, and 195 for the repair of a restore. A caller that owns the VM's exit
//! status and its shutdown takes the ABI in strict mode, with [`TimeAbi::strict`], in which a
//! call fails with the violation instead, whose [`status`](Violation::status) the caller ends the
//! VM with.
//!
//! `nvx-time` runs the conformance checks, the violation watcher and the wall-clock discipline
//! in one daemon, a separate process, and the snapshot agent's time steps: the guest preparation
//! steps of the architecture guide's section 5.3.2 and the guest repair steps of its section
//! 5.5.7, which the comments cite by number. Section 3.2.10 is the contract that it enforces. It
//! prints nothing on success: checks record their results in the state file, and only violation
//! events reach the console. It keeps the check IDs, the guest codes, and the formats of the
//! output lines and the state file of the C program that it replaced.
//!
//! `nvx-time test NAME ARGUMENTS...` runs one of the C program's test hooks, which exercise its
//! decoders, formats, and process structure without the hypervisor.

mod api;
#[cfg(target_arch = "x86_64")]
mod checks;
#[cfg(target_arch = "aarch64")]
mod checks_aarch64;
mod checks_shared;
mod commands;
mod cpu_mask;
mod daemon;
#[cfg(target_arch = "x86_64")]
mod decoders;
mod discipline;
mod events;
mod model;
mod state;
mod system;
#[cfg(target_arch = "x86_64")]
mod test_mode;
mod watcher;
mod wire;

use std::ffi::CString;
use std::io::{self, Write};
use std::process::ExitCode;

#[cfg(target_arch = "aarch64")]
use checks_aarch64 as checks;

#[cfg(target_arch = "x86_64")]
pub(crate) use api::{
    CaptureOutcome, CaptureRequest, CpuSet, MemoryRange, PreCaptureError, Restore, RestoreFinish,
    ScratchPolicy,
};
pub use api::{ColdBoot, TimeAbi, TimeError, Violation};
pub(crate) use model::Phase;

use super::status::{CommandError, run_main};
use super::sys;

/// Nanoseconds in one second.
const NSEC_PER_SEC: i64 = 1_000_000_000;
/// Nanoseconds in one millisecond.
const NSEC_PER_MSEC: i64 = 1_000_000;
/// Nanoseconds in one microsecond.
const NSEC_PER_USEC: i64 = 1_000;
/// Maximum processor count represented by the time ABI's fixed CPU masks.
pub(crate) const MAX_CPUS: usize = 64;
/// Delay between restore readiness and deferred check work.
const DEFERRED_START_NS: i64 = 150 * NSEC_PER_MSEC;
/// Deadline after which idle-priority work is promoted.
const IDLE_BOUND_NS: i64 = 100 * NSEC_PER_MSEC;
/// Most arguments that a command line may hold besides `--report-only`, as many as the C
/// program's fixed argument array.
const MAX_ARGUMENTS: usize = 32;

/// Byte-exact command help printed for malformed invocations.
#[cfg(target_arch = "x86_64")]
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

#[cfg(target_arch = "aarch64")]
/// ARM cold-boot commands; capture, restore and x86 test hooks are not admitted.
const USAGE: &[u8] = br#"usage: nvx-time [--report-only] COMMAND
  boot
  status
  sample
  generation-id
"#;

/// Runs `nvx-time` with the process's arguments. It exits with 0 on success, 1 when a command
/// or a check fails, or 2 for an invalid command line.
#[doc(hidden)]
pub fn main() -> ExitCode {
    run_main(|args| run(args, &mut sys::Stdout::new(), &mut io::stderr()))
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
    let Some((&command, _rest)) = arguments.split_first() else {
        sys::report(err, &[USAGE]);
        return Err(CommandError::Usage);
    };
    if command == b"test" {
        #[cfg(target_arch = "x86_64")]
        return test_mode::run(_rest, out);
        #[cfg(target_arch = "aarch64")]
        {
            sys::report(err, &[b"nvx-time: ARM x86 test hooks are unsupported\n"]);
            return Err(CommandError::Operation);
        }
    }
    commands::run(&arguments, report_only, out, err)
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests;
