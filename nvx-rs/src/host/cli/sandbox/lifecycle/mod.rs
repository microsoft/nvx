//! Persistent sandboxes on the CLI's store and caller-owned [`crate::host::vm`] interface.
//!
//! Provision keeps today's configuration, image bindings, network and export. Start journals the
//! prepared capability and outputs before spawn, records the verified process, and commits the
//! matching saved VM only after authenticated readiness and mappings. Every lifecycle command
//! takes the CLI store's recoverable lock; indeterminate launches retain their evidence.
//!
//! Exec and stop reattach with runtime and process verification. Stop requests NVXC shutdown,
//! waits for exit, then forces only the verified process. Deprovision removes only this stopped
//! sandbox's owned files. No running sandbox from the old runtime store is migrated.

mod directory;
mod outcome;
mod start;
mod stop;

use std::path::Path;
use std::time::Duration;

use crate::ErrorCode;
use crate::host::artifacts::{Guest, KERNEL_NAME};
use crate::host::cli::artifacts::{Located, locate};
use crate::host::cli::deadline::Deadline;
use crate::host::cli::deadline::RETRY_INTERVAL;
use crate::host::cli::sandbox::lock::LifecycleLock;
use crate::host::cli::sandbox::state::{
    Config, RecordedDistro, RecordedScratch, read_config, serialize_launch, write_json,
};
use crate::host::cli::sandbox::{Artifacts, error_with_cause};
use crate::host::cli::session::Session;
use crate::host::cli::store::StateFile;
use crate::host::cli::{Exit, Result};
use crate::host::process::{Opened, Process};
use crate::host::runtime::{IdentityPart, LaunchOptions, Runtime, RuntimeIdentity};
use crate::host::sandbox::{SandboxLaunch, require_file};
use crate::host::vm::{self, SavedVm};
use crate::protocol::GuestFeatures;

pub(crate) use self::directory::{create_state_directory, prepare_state_directory};
pub(crate) use self::outcome::{
    copy_openvmm_outcome, validate_outcome_destination, write_exec_outcome,
};
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
    let runtime_path = state_dir.join(StateFile::Runtime.name());
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

enum Found {
    /// No VM is recorded, or a start that was interrupted is over.
    Nothing,
    /// The recorded OpenVMM exited without a stop, and its runtime record is cleared.
    Exited,
    /// The VM runs under the runtime identity of the artifacts.
    Live(SavedVm, Process),
    /// The VM runs under another runtime identity, which differs in these parts.
    Mismatched(SavedVm, Process, Vec<IdentityPart>),
}

fn find_vm(lock: &mut LifecycleLock, artifacts: &Artifacts<'_>) -> Result<Found> {
    let Some(saved) = lock.locked().runtime().map_err(error_with_cause)? else {
        return Ok(Found::Nothing);
    };
    let process = match Process::open(&saved.process).map_err(|error| {
        Exit::error(
            ErrorCode::VmmIndeterminate,
            format!("cannot determine whether the OpenVMM process of the sandbox runs: {error}"),
        )
    })? {
        Opened::Running(process) => process,
        Opened::Exited(_) => {
            lock.clear_runtime()?;
            return Ok(Found::Exited);
        }
    };
    let runtime = artifacts.runtime_identity()?;
    let differences = saved.runtime.differences(&runtime);
    Ok(if differences.is_empty() {
        Found::Live(saved, process)
    } else {
        Found::Mismatched(saved, process, differences)
    })
}

/// Returns the parts of a runtime identity that `differences` names, as messages list them.
fn differing_parts(differences: &[IdentityPart]) -> String {
    let parts: Vec<&str> = differences.iter().map(|part| part.name()).collect();
    parts.join(", ")
}

/// Returns the error of a transition other than stop on a VM that another runtime identity than
/// the artifacts' started, which differs in `differences`: the CLI does not drive it, since it may
/// speak other contracts, and only its stop ends it.
fn mismatched(saved: &SavedVm, differences: &[IdentityPart]) -> Exit {
    Exit::error(
        ErrorCode::RuntimeMismatch,
        format!(
            "sandbox runs under another runtime identity than these artifacts, which differ in \
             {}, so it is not driven: stop it to terminate its OpenVMM process {} without a \
             guest shutdown",
            differing_parts(differences),
            saved.process.pid
        ),
    )
}

/// The running VM of a sandbox, and the capability with which its control session attaches.
struct Running {
    /// The VM.
    saved: SavedVm,
    /// The held, identity-verified process.
    process: Process,
}

/// Returns the running VM of the sandbox in the locked store of `state_dir`, as exec requires it.
fn running_vm(
    state_dir: &Path,
    lock: &mut LifecycleLock,
    artifacts: &Artifacts<'_>,
) -> Result<Running> {
    let found = found_vm(lock, artifacts)?;
    running(state_dir, found)
}

/// Finds the sandbox's VM as [`find_vm`] does, for an operation other than a start, which holds
/// no download lock of its own: while it reads the runtime identity of the artifacts, which only a
/// recorded launch needs, it holds the lock of their downloads, as [`Artifacts::hold`] says, so
/// that no download replaces them meanwhile. A sandbox that records no launch needs neither the
/// artifacts nor their root.
fn found_vm(lock: &mut LifecycleLock, artifacts: &Artifacts<'_>) -> Result<Found> {
    if lock.locked().runtime().map_err(error_with_cause)?.is_none() {
        return Ok(Found::Nothing);
    }
    let _hold = artifacts.hold()?;
    find_vm(lock, artifacts)
}

/// Returns the running VM that `found` describes in the locked store of `state_dir`, as exec and
/// stop require it, with its control capability.
fn running(state_dir: &Path, found: Found) -> Result<Running> {
    let (saved, process) = match found {
        Found::Live(saved, process) => (saved, process),
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
        Found::Mismatched(saved, _, differences) => return Err(mismatched(&saved, &differences)),
    };
    Ok(Running { saved, process })
}

fn runtime_for(
    artifacts: &Artifacts<'_>,
    config: &Config,
    identity: RuntimeIdentity,
) -> Result<Runtime> {
    let Located { dir, openvmm } = locate(artifacts.root)?;
    Ok(Runtime {
        openvmm,
        kernel: dir.artifact(KERNEL_NAME),
        initramfs: dir.initramfs(Guest::Alpine),
        identity,
        hypervisor: config.hypervisor().parse().map_err(error_with_cause)?,
        cpu_profile: None,
        windows_breakaway: false,
        options: LaunchOptions::default(),
    })
}

fn connect(saved: &SavedVm, artifacts: &Artifacts<'_>, timeout: f64) -> Result<Session> {
    let _hold = artifacts.hold()?;
    let config = Config::new(read_config(&saved.vm_dir.path().join(CONFIG_NAME))?)?;
    let runtime = runtime_for(artifacts, &config, artifacts.runtime_identity()?)?;
    let attached = vm::reattach(saved, &runtime, Deadline::after(timeout).instant())
        .map_err(error_with_cause)?;
    Ok(Session::from_connection(attached.connection))
}

/// Checks that `state_dir` holds the configuration of a provisioned sandbox: a directory without
/// it holds none, whatever else it holds.
fn require_config(state_dir: &Path) -> Result {
    require_file(&state_dir.join(CONFIG_NAME), "sandbox configuration")
        .map_err(|error| Exit::error(ErrorCode::SandboxNotProvisioned, error.message()))
}

#[cfg(test)]
mod tests;
