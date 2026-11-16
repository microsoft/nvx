//! Stopping a managed sandbox and removing a stopped sandbox's state.

use std::fs;
use std::time::{Duration, Instant};

use super::directory::prepare_state_directory;
use super::outcome::read_openvmm_outcome;
use super::{CONFIG_NAME, Found, Running, connect, differing_parts, found_vm, running};
use crate::ErrorCode;
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL};
use crate::host::cli::sandbox::lock::LifecycleLock;
use crate::host::cli::sandbox::state::{Config, read_config};
use crate::host::cli::sandbox::{Artifacts, error_with_cause};
use crate::host::cli::snapshots::Store;
use crate::host::cli::store::StateFile;
use crate::host::cli::{Exit, Result, check_interrupt, output};
use crate::host::process::Process;
use crate::host::runtime::IdentityPart;
use crate::host::snapshot::OPENVMM_DIR;
use crate::host::vm::{self, SavedVm, VmDir, VmOutputs};

/// How the stop of a sandbox's OpenVMM ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopOutcome {
    /// OpenVMM exited without a forced termination: by the end of its graceful stop, or before
    /// a termination began.
    Graceful,
    /// A forced termination through OpenVMM's verified process ended it within its bound.
    Forced,
    /// OpenVMM still runs: it did not exit within the bound of its stop or termination.
    TimedOut,
}

/// Stops a running sandbox, and checks the outcome report that its OpenVMM wrote, unless another
/// runtime identity than the artifacts' started the VM.
///
/// A STOP acknowledgement is not exit proof. After its grace period the stop forces only the
/// verified process; an indeterminate exit retains the runtime record.
///
/// A VM of another runtime identity may speak other contracts, so the stop asks it nothing: it
/// ends the VM as [`terminate_mismatched`] does.
pub(crate) fn stop(state_path: &str, artifacts: &Artifacts<'_>, timeout: f64) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    let Running { saved, process } = match found_vm(&mut lock, artifacts)? {
        Found::Mismatched(saved, process, differences) => {
            return terminate_mismatched(&mut lock, &saved, &process, &differences, timeout);
        }
        found => running(&state_dir, found)?,
    };
    let grace = Deadline::after(timeout);
    let requested = connect(&saved, artifacts, timeout).and_then(|mut session| {
        session.stop(grace.remaining().map_or(0.0, |left| left.as_secs_f64()))
    });
    if matches!(requested, Err(Exit::Interrupted)) {
        return requested;
    }
    wait_for_exit(&process, grace)?;
    let (outcome, stopped) = stop_verified(&saved, &process, timeout)?;
    lock.clear_runtime()?;
    if let Err(error) = requested {
        output::print_stderr(&format!(
            "warning: managed guest shutdown failed: {error:?}"
        ));
    }
    if outcome == StopOutcome::Forced {
        output::print_stderr("warning: OpenVMM was forcibly terminated after managed stop");
    }
    let report = stopped.report.map_err(error_with_cause)?;
    if report.is_none() && outcome == StopOutcome::Forced {
        output::print_stderr("warning: the forced stop produced no outcome report");
        return Ok(());
    }
    read_openvmm_outcome(&state_dir.join(StateFile::Outcome.name())).map(drop)
}

fn stop_verified(
    saved: &SavedVm,
    process: &Process,
    timeout: f64,
) -> Result<(StopOutcome, vm::Stopped)> {
    let outputs = VmOutputs {
        log: saved.vm_dir.path().join(StateFile::Log.name()),
        outcome: saved.vm_dir.path().join(StateFile::Outcome.name()),
        endpoint: saved.endpoint.clone(),
        console: None,
    };
    let (mut stopped, mut failure) = (None, None);
    let outcome = terminate_within(
        Deadline::after(timeout),
        |bound| {
            let now = Instant::now();
            match vm::stop(process, &outputs, |_| false, now, now + bound) {
                Ok(result) => {
                    let outcome = if result.forced {
                        StopOutcome::Forced
                    } else {
                        StopOutcome::Graceful
                    };
                    stopped = Some(result);
                    Ok(outcome)
                }
                Err(error) if error.kind() == crate::ErrorKind::StillRunning => {
                    failure = Some(error);
                    Ok(StopOutcome::TimedOut)
                }
                Err(error) => Err(error_with_cause(error)),
            }
        },
        |deadline| wait_for_exit(process, deadline),
    )?;
    if outcome == StopOutcome::TimedOut {
        return Err(error_with_cause(
            failure.expect("an unconfirmed stop retained its failure"),
        ));
    }
    Ok((
        outcome,
        stopped.expect("a confirmed stop returned its report result"),
    ))
}

fn terminate_mismatched(
    lock: &mut LifecycleLock,
    saved: &SavedVm,
    process: &Process,
    differences: &[IdentityPart],
    timeout: f64,
) -> Result {
    let (outcome, stopped) = stop_verified(saved, process, timeout)?;
    lock.clear_runtime()?;
    if let Err(error) = stopped.report {
        output::print_stderr(&format!(
            "warning: stopped VM outcome could not be read: {error}"
        ));
    }
    let ended = match outcome {
        StopOutcome::Forced => "was terminated without a guest shutdown",
        StopOutcome::Graceful => "had exited",
        StopOutcome::TimedOut => unreachable!("an unconfirmed stop returns an error"),
    };
    output::print_stderr(&format!(
        "warning: sandbox ran under another runtime identity than these artifacts, which differ \
         in {}: its OpenVMM process {} {ended}",
        differing_parts(differences),
        saved.process.pid
    ));
    Ok(())
}

/// Ends an OpenVMM by identity-verified forced termination, and waits until `deadline` for it to
/// exit: `terminate` calls [`vm::stop`] in a short slice, and `wait` polls for exit with Ctrl-C.
///
/// The termination waits for one retry interval at most, and `wait` for the rest, so that Ctrl-C
/// ends the wait for an OpenVMM that does not exit at once, as one in uninterruptible I/O. Once
/// that one has exited, a second termination finds it exited, signals nothing, and clears its
/// record, and the result is [`StopOutcome::Forced`] all the same. One that still runs at the
/// deadline is [`StopOutcome::TimedOut`], and Ctrl-C fails the termination: both keep the record.
pub(super) fn terminate_within(
    deadline: Deadline,
    mut terminate: impl FnMut(Duration) -> Result<StopOutcome>,
    wait: impl FnOnce(Deadline) -> Result<bool>,
) -> Result<StopOutcome> {
    check_interrupt()?;
    // A termination's bound is positive, though the deadline may have passed.
    let bound = deadline
        .remaining()
        .map_or(RETRY_INTERVAL, |left| left.min(RETRY_INTERVAL))
        .max(Duration::from_nanos(1));
    match terminate(bound)? {
        StopOutcome::TimedOut if wait(deadline)? => match terminate(bound)? {
            StopOutcome::TimedOut => Ok(StopOutcome::TimedOut),
            StopOutcome::Graceful | StopOutcome::Forced => Ok(StopOutcome::Forced),
        },
        outcome => Ok(outcome),
    }
}

/// Waits until the OpenVMM of `vmm` exits or `deadline` passes, and returns whether it exited,
/// failing if Ctrl-C arrives first.
pub(super) fn wait_for_exit(process: &Process, deadline: Deadline) -> Result<bool> {
    loop {
        check_interrupt()?;
        let slice = deadline
            .remaining()
            .map_or(RETRY_INTERVAL, |left| left.min(RETRY_INTERVAL));
        if process
            .wait(Instant::now() + slice)
            .map_err(Exit::from_os_error)?
            .is_some()
        {
            return Ok(true);
        }
        if deadline.passed() {
            return Ok(false);
        }
    }
}

/// Removes a stopped sandbox and its state directory, as `deprovision` does.
///
/// A restored sandbox's view of its snapshot goes first, under the snapshot's lock, since its
/// removal changes the links of the snapshot's files.
pub(crate) fn deprovision(state_path: &str, artifacts: &Artifacts<'_>, timeout: f64) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    match found_vm(&mut lock, artifacts)? {
        Found::Nothing | Found::Exited => {}
        Found::Live(..) | Found::Mismatched(..) => {
            return Err(Exit::error(
                ErrorCode::SandboxRunning,
                "sandbox must be stopped before deprovision",
            ));
        }
    }
    // The store accepts a state directory that a VM directory would not, such as an earlier
    // release's, so deprovision requires a VM directory only to remove a view from one.
    if fs::symlink_metadata(state_dir.join(OPENVMM_DIR)).is_ok()
        && let Some(source) = Config::new(read_config(&state_dir.join(CONFIG_NAME))?)?.source()?
    {
        let vm = VmDir::open(&state_dir).map_err(error_with_cause)?;
        Store::open(&source.store)?
            .lock(&source.snapshot, Deadline::after(timeout))?
            .remove_view(&vm)?;
    }
    // No OpenVMM of the sandbox runs, and a start that was interrupted is over, so the store
    // removes the sandbox: its files and the configuration, then refuses files that are not
    // NVX's, and otherwise removes its lock file, while it still holds the lock, and the
    // directory.
    lock.remove(timeout)
}
