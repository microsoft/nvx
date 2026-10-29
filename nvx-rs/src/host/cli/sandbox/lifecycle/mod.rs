//! The lifecycle of a managed sandbox, a port of the former
//! `scripts/nvx_tools/sandbox_lifecycle.py`, which it replaced (microsoft/nvx#359), on the
//! [`store`](crate::host::store), [`supervisor`], and [`session`](crate::host::session) that
//! every host of a sandbox shares.
//!
//! A sandbox lives in its state directory, which is the directory of its store:
//!
//! ```text
//! <state>/
//!   config.json          the launch that provision records, in format 8, with the export plan of
//!                        its shares or `null` without shares, its upper, the bindings of its
//!                        distro layer and of its scratch image, if any, and its network
//!   sandbox.lock         the store's lock, which serializes the lifecycle transitions
//!   launch.json          the store's journal of a start in progress, or of an interrupted one
//!   runtime.json         the store's runtime record of the running VM, in its format 4
//!   control.capability   the 32-byte capability of the running OpenVMM
//!   control.sock         the control endpoint of the running OpenVMM, on POSIX
//!   openvmm.log          OpenVMM's output, which each start replaces
//!   outcome.json         OpenVMM's outcome report of the latest start
//! ```
//!
//! The CLI keeps its own record, the configuration, in its own formats; the store keeps the
//! runtime records, and reads no other format than its own: a sandbox that an earlier build
//! started must be stopped with that build.
//!
//! [`provision`], [`fn@crate::host::cli::sandbox::lifecycle::start`], [`fn@crate::host::cli::sandbox::lifecycle::stop`], and [`deprovision`] hold the [`LifecycleLock`] for their
//! whole transition, and [`exec_workload`] only while it finds the running VM, so a long workload
//! cannot delay a stop. `config.json` also records the binding of the distro layer that
//! `provision` verified, and that of the scratch image of an upper on scratch, which [`fn@crate::host::cli::sandbox::lifecycle::start`] checks from metadata
//! alone, and the network as its typed policy, which [`fn@crate::host::cli::sandbox::lifecycle::start`] checks again, in place of the
//! network options that `sandbox_lifecycle.py` recorded: it refuses to start a sandbox that
//! `nvx.py` or an earlier `nvx-rs` provisioned, which has no bindings or no typed network. It
//! records shares as the export plan that `provision` made, whose host objects [`fn@crate::host::cli::sandbox::lifecycle::start`] checks
//! again. [`fn@crate::host::cli::sandbox::lifecycle::start`] exports the shares, and once the guest agent is ready sends it the plan's
//! mapping table, which it mounts in order before any workload runs; a mapping that fails ends the
//! start, after the guest shuts down as a stop has it do.
//!
//! Every transition but provision finds the sandbox's VM with [`supervisor::reattach`], as an
//! orchestrator does after it restarts: it ends a start that was interrupted, holds a recorded
//! OpenVMM that still runs through its verified identity, and clears the runtime record of one
//! that exited without a stop, which the first transition to find it reports, or which a start
//! replaces. A VM that another runtime identity than the artifacts' started, as after the
//! artifacts were replaced, is never driven, since its guest may speak other contracts: [`fn@crate::host::cli::sandbox::lifecycle::stop`]
//! ends it by identity-verified forced termination, without a control session, and the other
//! transitions refuse it. [`fn@crate::host::cli::sandbox::lifecycle::start`] launches OpenVMM with [`supervisor::launch`], under the
//! runtime identity that the install metadata of the artifacts names, and [`fn@crate::host::cli::sandbox::lifecycle::stop`] asks the guest
//! of a VM of that identity to shut down, waits for OpenVMM to exit, and reports a VM that does
//! not, without ending it otherwise.
//!
//! [`fn@crate::host::cli::sandbox::lifecycle::start`] hands OpenVMM, through a [`crate::host::sandbox::PreparedLaunch`] of the checked files, the objects that
//! the checks found, rather than paths that something could replace meanwhile, and claims the
//! scratch image, if any, for this sandbox while OpenVMM runs. It holds the checked files until
//! the guest is ready, when it releases them, and on Windows OpenVMM's copies of them, or until it
//! has ended an OpenVMM whose start failed. A sandbox whose upper is in RAM starts each time with
//! an empty upper, which its stop discards.

mod directory;
mod outcome;
mod start;
mod stop;

use std::path::Path;
use std::time::Duration;

use crate::ErrorCode;
use crate::host::cli::deadline::RETRY_INTERVAL;
use crate::host::cli::sandbox::lock::LifecycleLock;
use crate::host::cli::sandbox::state::{
    RecordedDistro, RecordedScratch, serialize_launch, write_json,
};
use crate::host::cli::sandbox::{Artifacts, error_with_cause};
use crate::host::cli::session::Session;
use crate::host::cli::{Exit, Result};
use crate::host::sandbox::{SandboxLaunch, require_file};
use crate::host::store::{IdentityPart, LockedStore, RUNTIME_NAME};
use crate::host::supervisor::{self, Reattached, Vmm};
use crate::protocol::{CAPABILITY_LEN, GuestFeatures};

pub(crate) use self::directory::{create_state_directory, prepare_state_directory};
pub(crate) use self::outcome::{validate_outcome_destination, write_exec_outcome};
pub(crate) use self::start::{exec_workload, start};
pub(crate) use self::stop::{deprovision, stop};

/// The launch configuration, the CLI's own record beside those of the store.
pub(crate) const CONFIG_NAME: &str = "config.json";
/// How often a caller retries a lifecycle lock that another caller holds, and the removal of a
/// state directory that open files still keep.
pub(crate) const LOCK_RETRY_INTERVAL: Duration = RETRY_INTERVAL;
/// The version of outcome reports.
pub(crate) const OUTCOME_SCHEMA_VERSION: u64 = 1;

/// The options of a launch, besides the launch itself, that `provision` records.
pub(crate) struct ProvisionConfig<'a> {
    /// `--hypervisor`, resolved.
    pub(crate) hypervisor: &'a str,
    /// `--memory-mib`.
    pub(crate) memory_mib: u64,
    /// `--cmdline`.
    pub(crate) cmdline: &'a str,
}

/// The control features that the layered sandbox root must provide for the requests of
/// `nvx-rs sandbox`.
pub(crate) const REQUIRED_FEATURES: GuestFeatures = GuestFeatures::LAYERED_SANDBOX;

/// Records a launch and its distro layer, at the path at which it was verified, with the layer's
/// binding, in a new state directory, as `provision` does, with the binding of its scratch image,
/// if its upper is on scratch, at the path that it records for it.
pub(crate) fn provision(
    state_path: &str,
    launch: &SandboxLaunch,
    distro: &RecordedDistro,
    config: &ProvisionConfig<'_>,
    timeout: f64,
) -> Result {
    let state_dir = prepare_state_directory(state_path, true)?;
    let config_path = state_dir.join(CONFIG_NAME);
    let runtime_path = state_dir.join(RUNTIME_NAME);
    let _lock = LifecycleLock::acquire(&state_dir, timeout, true)?;
    if config_path.exists() || runtime_path.exists() {
        return Err(Exit::error(
            ErrorCode::SandboxProvisioned,
            "sandbox is already provisioned",
        ));
    }
    launch.validated()?;
    let scratch = RecordedScratch::new(launch)?;
    write_json(
        &config_path,
        &serialize_launch(launch, distro, scratch.as_ref(), config)?,
    )
}

/// What a transition finds of the sandbox's VM, through [`supervisor::reattach`].
enum Found {
    /// No VM is recorded, or a start that was interrupted is over.
    Nothing,
    /// The recorded OpenVMM exited without a stop, and its runtime record is cleared.
    Exited,
    /// The VM runs under the runtime identity of the artifacts.
    Live(Vmm),
    /// The VM runs under another runtime identity, which differs in these parts.
    Mismatched(Vmm, Vec<IdentityPart>),
}

/// Finds the sandbox's VM in the locked store, as an orchestrator does after it restarts within
/// the same host boot: with [`supervisor::reattach`], under the runtime identity of `artifacts`.
///
/// Only a recorded launch needs that identity, so a sandbox that never started, or that stopped,
/// needs no install metadata. Fails for a record that the store does not read, such as one of an
/// earlier format, and when whether the VM runs cannot be determined.
fn find_vm(locked: &mut LockedStore, artifacts: &Artifacts<'_>) -> Result<Found> {
    if !records_launch(locked)? {
        return Ok(Found::Nothing);
    }
    let runtime = artifacts.runtime_identity()?;
    Ok(
        match supervisor::reattach(locked, &runtime).map_err(error_with_cause)? {
            Reattached::Live(vmm) => Found::Live(vmm),
            Reattached::Mismatched { vmm, differences } => Found::Mismatched(vmm, differences),
            Reattached::Exited {
                record: Some(_), ..
            } => Found::Exited,
            Reattached::Exited { record: None, .. } => Found::Nothing,
            Reattached::Indeterminate(reason) => {
                return Err(Exit::error(
                    ErrorCode::VmmIndeterminate,
                    format!(
                        "cannot determine whether the OpenVMM process of the sandbox runs: \
                         {reason}"
                    ),
                ));
            }
        },
    )
}

/// Returns whether the locked store records a launch of the sandbox, in its launch journal or its
/// runtime record, which alone needs the runtime identity of the artifacts.
fn records_launch(locked: &mut LockedStore) -> Result<bool> {
    Ok(locked.journal().map_err(error_with_cause)?.is_some()
        || locked.runtime().map_err(error_with_cause)?.is_some())
}

/// Returns the parts of a runtime identity that `differences` names, as messages list them.
fn differing_parts(differences: &[IdentityPart]) -> String {
    let parts: Vec<&str> = differences.iter().map(|part| part.name()).collect();
    parts.join(", ")
}

/// Returns the error of a transition other than stop on a VM that another runtime identity than
/// the artifacts' started, which differs in `differences`: the CLI does not drive it, since it may
/// speak other contracts, and only its stop ends it.
fn mismatched(vmm: &Vmm, differences: &[IdentityPart]) -> Exit {
    Exit::error(
        ErrorCode::RuntimeMismatch,
        format!(
            "sandbox runs under another runtime identity than these artifacts, which differ in \
             {}, so it is not driven: stop it to terminate its OpenVMM process {} without a \
             guest shutdown",
            differing_parts(differences),
            vmm.record().process().pid
        ),
    )
}

/// The running VM of a sandbox, and the capability with which its control session attaches.
struct Running {
    /// The VM.
    vmm: Vmm,
    /// Its control capability.
    capability: [u8; CAPABILITY_LEN],
}

/// Returns the running VM of the sandbox in the locked store of `state_dir`, as exec requires it.
fn running_vm(
    state_dir: &Path,
    locked: &mut LockedStore,
    artifacts: &Artifacts<'_>,
) -> Result<Running> {
    let found = found_vm(locked, artifacts)?;
    running(state_dir, locked, found)
}

/// Finds the sandbox's VM as [`find_vm`] does, for an operation other than a start, which holds
/// no download lock of its own: while it reads the runtime identity of the artifacts, which only a
/// recorded launch needs, it holds the lock of their downloads, as [`Artifacts::hold`] says, so
/// that no download replaces them meanwhile. A sandbox that records no launch needs neither the
/// artifacts nor their root.
fn found_vm(locked: &mut LockedStore, artifacts: &Artifacts<'_>) -> Result<Found> {
    if !records_launch(locked)? {
        return Ok(Found::Nothing);
    }
    let _hold = artifacts.hold()?;
    find_vm(locked, artifacts)
}

/// Returns the running VM that `found` describes in the locked store of `state_dir`, as exec and
/// stop require it, with its control capability.
fn running(state_dir: &Path, locked: &mut LockedStore, found: Found) -> Result<Running> {
    let vmm = match found {
        Found::Live(vmm) => vmm,
        Found::Nothing => {
            // A directory without the configuration of a sandbox holds no provisioned one, which
            // a start would report too, rather than one that is stopped.
            require_config(state_dir)?;
            return Err(Exit::error(
                ErrorCode::SandboxNotRunning,
                "sandbox is not running",
            ));
        }
        Found::Exited => {
            return Err(Exit::error(
                ErrorCode::SandboxExited,
                "sandbox runtime state is stale because the OpenVMM process is not running",
            ));
        }
        Found::Mismatched(vmm, differences) => return Err(mismatched(&vmm, &differences)),
    };
    let capability = locked.read_capability().map_err(error_with_cause)?;
    Ok(Running { vmm, capability })
}

/// Connects to the control endpoint of `vmm`, and attaches with `capability`.
fn connect(vmm: &Vmm, capability: &[u8; CAPABILITY_LEN], timeout: f64) -> Result<Session> {
    Session::connect(
        vmm.endpoint(),
        vmm.record().process().pid,
        capability,
        timeout,
    )
}

/// Checks that `state_dir` holds the configuration of a provisioned sandbox: a directory without
/// it holds none, whatever else it holds.
fn require_config(state_dir: &Path) -> Result {
    require_file(&state_dir.join(CONFIG_NAME), "sandbox configuration")
        .map_err(|error| Exit::error(ErrorCode::SandboxNotProvisioned, error.message()))
}

#[cfg(test)]
mod tests;
