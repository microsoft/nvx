//! The shared `nvx-time` program and typed time-ABI calls for x86-64 and ARM64 Linux guests.
//!
//! [`TimeAbi::cold_boot`] synchronizes UTC before guest work. [`TimeAbi::start_daemon`] starts
//! one supervised child for deferred architectural checks, kernel-log watching and continuous
//! clock discipline. The command-line program uses these same calls on either ISA.
//! [`TimeAbi::generation_id`] reads the unchanged a6 process identity.
//!
//! Transport and counter conversion remain architectural: x86 uses PIO, the TSC and Linux's
//! kHz conversion; ARM uses the restricted platform device, CNTVCT and the a8 binding's full-Hz
//! frequency, checked against CNTFRQ. CPU-profile admission belongs to the VMM, not this program.
//! ARM currently supports cold boot only; capture, restore and x86 diagnostic test hooks remain
//! x86-only. The sample wire format, event codes and persisted state format are unchanged.
//!
//! Enforcing violations stop the VM with the status in [`crate::protocol::status`]. A caller
//! that owns shutdown uses [`TimeAbi::strict`] and receives a [`Violation`] instead. The child
//! daemon enforces independently. Each hardware transaction relinquishes its authority before
//! another process can be started, and no call waits for a child it did not start.
#![cfg_attr(
    target_arch = "x86_64",
    doc = r#"
On x86, capture and restore use the same API. An in-memory capture hands fresh entropy over
as an owned `Seed`, rather than writing an entropy file:

```no_run
use nvx::guest::reseed;
use nvx::guest::time::{CaptureOutcome, CaptureRequest, RestoreFinish, ScratchPolicy, TimeAbi};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let abi = TimeAbi::new();
let daemon = abi.start_daemon(abi.cold_boot()?)?;
let request = CaptureRequest::in_memory(ScratchPolicy::Fresh, abi.generation_id()?);
if let CaptureOutcome::Restored(mut restore) = abi.capture(request)? {
    if let Some(seed) = restore.take_seed() {
        reseed::reseed(&seed)?;
    }
    let mut finish = RestoreFinish::new();
    if restore.acknowledgement_required() {
        finish = finish.acknowledge();
    }
    abi.restore_finish(finish)?;
}
# let _ = daemon;
# Ok(())
# }
```
"#
)]

mod api;
#[cfg(target_arch = "x86_64")]
mod checks;
#[cfg(target_arch = "aarch64")]
mod checks_arm64;
#[cfg(target_arch = "aarch64")]
use checks_arm64 as checks;
mod commands;
#[cfg(target_arch = "x86_64")]
mod cpu_mask;
mod daemon;
#[cfg(target_arch = "x86_64")]
mod decoders;
mod deferred;
mod events;
mod sample;
mod state;
mod system;
#[cfg(target_arch = "x86_64")]
mod test_mode;

use std::ffi::CString;
use std::io::{self, Write};
use std::process::ExitCode;

#[cfg(target_arch = "x86_64")]
pub(crate) use api::PreCaptureError;
#[cfg(target_arch = "x86_64")]
pub use api::{
    CaptureOutcome, CaptureRequest, CpuSet, MemoryRange, Restore, RestoreFinish, RestoreNetwork,
    ScratchPolicy,
};
pub use api::{ColdBoot, TimeAbi, TimeError, Violation};

use super::status::{CommandError, run_main};
use super::sys;
pub(crate) use deferred::DEFERRED_START_NS;
#[cfg(target_arch = "x86_64")]
pub(crate) use deferred::IDLE_BOUND_NS;
pub(crate) use sample::Phase;

/// Nanoseconds in one second.
#[cfg(target_arch = "x86_64")]
pub(crate) const NSEC_PER_SEC: i64 = 1_000_000_000;
/// Nanoseconds in one millisecond.
#[cfg(target_arch = "x86_64")]
pub(crate) const NSEC_PER_MSEC: i64 = 1_000_000;
/// Nanoseconds in one microsecond.
pub(crate) const NSEC_PER_USEC: i64 = 1_000;
/// Maximum processor count represented by the time ABI's fixed CPU masks.
#[cfg(target_arch = "x86_64")]
pub(crate) const MAX_CPUS: usize = 64;
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
/// Cold-boot commands supported by the ARM machine contract.
#[cfg(target_arch = "aarch64")]
const USAGE: &[u8] = b"usage: nvx-time [--report-only] boot|status|sample|generation-id\n";

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
    let Some((command, rest)) = arguments.split_first() else {
        sys::report(err, &[USAGE]);
        return Err(CommandError::Usage);
    };
    #[cfg(target_arch = "x86_64")]
    if *command == b"test" {
        return test_mode::run(rest, out);
    }
    #[cfg(target_arch = "aarch64")]
    let _ = (command, rest);
    commands::run(&arguments, report_only, out, err)
}

#[cfg(test)]
mod tests;
