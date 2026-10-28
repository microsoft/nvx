//! Stopping a managed sandbox and removing a stopped sandbox's state.

use std::time::{Duration, Instant};

use super::directory::prepare_state_directory;
use super::outcome::read_openvmm_outcome;
use super::{Found, Running, connect, differing_parts, found_vm, running};
use crate::ErrorCode;
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL};
use crate::host::cli::sandbox::lock::LifecycleLock;
use crate::host::cli::sandbox::{Artifacts, error_with_cause};
use crate::host::cli::{Exit, Result, check_interrupt, output};
use crate::host::store::{IdentityPart, LockedStore, OUTCOME_NAME};
use crate::host::supervisor::{StopOutcome, Vmm};

/// Stops a running sandbox, and checks the outcome report that its OpenVMM wrote, unless another
/// runtime identity than the artifacts' started the VM.
///
/// The supervisor's stop runs one graceful step, which asks the guest to shut the VM down and
/// waits up to `timeout` seconds for OpenVMM to exit, so that Ctrl-C ends the wait. A VM whose
/// OpenVMM is still running then is reported, and keeps its runtime record: the stop never takes
/// the forced step.
///
/// A VM of another runtime identity may speak other contracts, so the stop asks it nothing: it
/// ends the VM as [`terminate_mismatched`] does.
pub(crate) fn stop(state_path: &str, artifacts: &Artifacts<'_>, timeout: f64) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    let locked = lock.locked();
    let Running { vmm, capability } = match found_vm(locked, artifacts)? {
        Found::Mismatched(vmm, differences) => {
            return terminate_mismatched(locked, &vmm, &differences, timeout);
        }
        found => running(&state_dir, locked, found)?,
    };
    // The graceful step waits for the exit itself, so the stop's own wait, until a deadline that
    // has passed by then, only checks whether OpenVMM has exited.
    let stopped = vmm
        .stop(locked, Instant::now(), None, |vmm, _| {
            {
                let mut session = connect(vmm, &capability, timeout)?;
                session.stop(timeout)?;
            }
            wait_for_exit(vmm, Deadline::after(timeout)).map(drop)
        })
        .map_err(error_with_cause)?;
    if let Some(error) = stopped.graceful_error {
        return Err(error);
    }
    if stopped.outcome == StopOutcome::TimedOut {
        return Err(Exit::error(
            ErrorCode::TimedOut,
            "OpenVMM did not terminate after managed stop",
        ));
    }
    read_openvmm_outcome(&state_dir.join(OUTCOME_NAME)).map(drop)
}

/// Ends the VM `vmm`, which another runtime identity than the artifacts' started, and which
/// differs from them in `differences`, by identity-verified forced termination, and warns on
/// standard error that it did.
///
/// Its guest is not asked to shut down, and no control session is opened to it: its OpenVMM is
/// terminated through its verified process identity, as [`Vmm::terminate`] does, and the stop
/// waits up to `timeout` seconds for it to exit, as [`terminate_within`] does, so that Ctrl-C ends
/// the wait. One that does not exit then, whose exit cannot be determined, or whose wait Ctrl-C
/// ends, keeps its runtime record, so that a later stop retries.
fn terminate_mismatched(
    locked: &mut LockedStore,
    vmm: &Vmm,
    differences: &[IdentityPart],
    timeout: f64,
) -> Result {
    let pid = vmm.record().process().pid;
    let parts = differing_parts(differences);
    let outcome = terminate_within(
        Deadline::after(timeout),
        |bound| vmm.terminate(locked, bound).map_err(error_with_cause),
        |deadline| wait_for_exit(vmm, deadline),
    )?;
    let ended = match outcome {
        StopOutcome::Forced => "was terminated without a guest shutdown",
        StopOutcome::Graceful => "had exited",
        StopOutcome::TimedOut => {
            return Err(Exit::error(
                ErrorCode::TimedOut,
                format!(
                    "OpenVMM process {pid} of the sandbox, which runs under another runtime \
                     identity than these artifacts, did not exit after it was terminated"
                ),
            ));
        }
    };
    output::print_stderr(&format!(
        "warning: sandbox ran under another runtime identity than these artifacts, which differ \
         in {parts}: its OpenVMM process {pid} {ended}"
    ));
    Ok(())
}

/// Ends an OpenVMM by identity-verified forced termination, and waits until `deadline` for it to
/// exit: `terminate` terminates it unless it has exited, waits up to the bound that it takes for
/// it to exit, and clears its runtime record once it has, as [`Vmm::terminate`] does, and `wait`
/// waits for its exit until a deadline, fails if Ctrl-C arrives first, and returns whether it
/// exited, as [`wait_for_exit`] does.
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
fn wait_for_exit(vmm: &Vmm, deadline: Deadline) -> Result<bool> {
    loop {
        check_interrupt()?;
        let slice = deadline
            .remaining()
            .map_or(RETRY_INTERVAL, |left| left.min(RETRY_INTERVAL));
        if vmm
            .wait(Instant::now() + slice)
            .map_err(Exit::from_os_error)?
        {
            return Ok(true);
        }
        if deadline.passed() {
            return Ok(false);
        }
    }
}

/// Removes a stopped sandbox and its state directory, as `deprovision` does.
pub(crate) fn deprovision(state_path: &str, artifacts: &Artifacts<'_>, timeout: f64) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    match found_vm(lock.locked(), artifacts)? {
        Found::Nothing | Found::Exited => {}
        Found::Live(_) | Found::Mismatched(..) => {
            return Err(Exit::error(
                ErrorCode::SandboxRunning,
                "sandbox must be stopped before deprovision",
            ));
        }
    }
    // No OpenVMM of the sandbox runs, and a start that was interrupted is over, so the store
    // removes the sandbox: its files and the configuration, then refuses files that are not
    // NVX's, and otherwise removes its lock file, while it still holds the lock, and the
    // directory.
    lock.remove(timeout)
}
