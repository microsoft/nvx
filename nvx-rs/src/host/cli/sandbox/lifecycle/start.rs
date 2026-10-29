//! Starting a managed sandbox and executing workloads in its running VM.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::directory::{prepare_state_directory, unlink_missing_ok};
use super::outcome::read_openvmm_outcome;
use super::{
    CONFIG_NAME, Found, REQUIRED_FEATURES, connect, find_vm, mismatched, require_config, running_vm,
};
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL};
use crate::host::cli::sandbox::lock::LifecycleLock;
use crate::host::cli::sandbox::state::{Config, random_bytes, read_config};
use crate::host::cli::sandbox::{Artifacts, error_with_cause};
use crate::host::cli::session::{ExecRequest, ExecResult, Session};
use crate::host::cli::{Exit, Result};
use crate::host::sandbox::PreparedLaunch;
use crate::host::sandbox::distro::DISTRO_POLICY_REVISION;
use crate::host::sandbox::scratch::CLAIM_RELEASE_WAIT;
use crate::host::store::{LaunchId, LockedStore, OUTCOME_NAME, Recovery};
use crate::host::supervisor::{self, Launched, VmmCommand};
use crate::protocol::CAPABILITY_LEN;
use crate::{ErrorCode, ErrorKind};

/// Starts a provisioned sandbox, as `start` does.
pub(crate) fn start(state_path: &str, artifacts: &Artifacts<'_>, timeout: f64) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    start_locked(&state_dir, lock.locked(), artifacts, timeout)
}

/// Starts the sandbox of a locked state directory, as `_start` does, through the launch journal
/// of its store.
fn start_locked(
    state_dir: &Path,
    locked: &mut LockedStore,
    artifacts: &Artifacts<'_>,
    timeout: f64,
) -> Result {
    let config_path = state_dir.join(CONFIG_NAME);
    require_config(state_dir)?;
    let config = read_config(&config_path)?;
    // The release of a checkout without artifacts is downloaded before the runtime identity of a
    // recorded launch is read from their install metadata, and no download replaces them until
    // the start ends, once OpenVMM has read them, so that the runtime identity that the start
    // records is theirs. A configuration without a hypervisor is malformed, which reading it below
    // reports.
    let _hold = match config.get("hypervisor").and_then(Value::as_str) {
        Some(hypervisor) => Some(artifacts.ensure(hypervisor)?),
        None => None,
    };
    match find_vm(locked, artifacts)? {
        Found::Nothing | Found::Exited => {}
        Found::Live(_) => {
            return Err(Exit::error(
                ErrorCode::SandboxRunning,
                "sandbox is already running or has stale runtime state",
            ));
        }
        Found::Mismatched(vmm, differences) => return Err(mismatched(&vmm, &differences)),
    }
    let outcome_path = state_dir.join(OUTCOME_NAME);
    unlink_missing_ok(&outcome_path)?;
    let config = Config::new(config)?;
    let launch = config.launch()?;
    // The checks read no content of the distro layer, its manifest, or the scratch image, and
    // never verify them again. The prepared launch holds the files that they return, and hands
    // OpenVMM those objects: on Linux, OpenVMM inherits their descriptors; on Windows, it opens
    // them by their final paths, which the files, and OpenVMM's copies of them, keep anything from
    // replacing before OpenVMM, whose opens share them, has opened them, even if the start ends
    // first. The scratch image's check also claims it for this sandbox, until OpenVMM exits. An
    // upper in RAM has no scratch image, and its configuration no binding of one.
    let binding = config.distro_binding()?;
    let scratch_binding = config.scratch_binding()?;
    let distro = binding
        .check(launch.distro(), DISTRO_POLICY_REVISION)
        .map_err(binding_error)?;
    // Reading the network checks its policy again, as provision did, before anything launches.
    let launch = launch.with_network(config.network()?);
    let scratch = match (scratch_binding, launch.scratch()) {
        (Some(scratch_binding), Some(scratch)) => Some(
            scratch_binding
                .check(scratch, Instant::now() + CLAIM_RELEASE_WAIT)
                .map_err(binding_error)?,
        ),
        _ => None,
    };
    // The plan takes the format and identity of the distro layer from its binding.
    let plan = launch.bound_plan(&binding)?;
    let [executable, kernel, initrd] = artifacts.require()?;
    let runtime = artifacts.launch_identity()?;
    let capability = random_bytes::<CAPABILITY_LEN>()?;
    if capability == [0; CAPABILITY_LEN] {
        return Err(Exit::error(
            ErrorCode::Other,
            "the system returned an all-zero control capability",
        ));
    }
    let memory_mib = config.memory_mib();
    let prepared = PreparedLaunch::new(plan, distro, scratch, config.cmdline())?;
    // The control endpoint, which the launch chooses, goes between these arguments.
    let mut head = vec![executable];
    head.extend(prepared.openvmm_arguments().iter().cloned());
    head.extend(
        [
            "--microvm-lifecycle",
            "managed",
            "--single-process",
            "--hypervisor",
        ]
        .map(str::to_owned),
    );
    head.push(config.hypervisor().to_owned());
    head.push("--memory".to_owned());
    head.push(format!("{memory_mib}M"));
    head.extend(["--kernel".to_owned(), kernel, "--initrd".to_owned(), initrd]);
    head.push("--cmdline".to_owned());
    head.push(prepared.kernel_command_line().to_owned());
    head.extend(["--virtio-console", "none", "--microvm-control-console"].map(str::to_owned));
    let tail = vec![
        "--microvm-control-auth-stdin".to_owned(),
        "--microvm-report".to_owned(),
        outcome_path.display().to_string(),
    ];
    let directory = std::env::current_dir().map_err(Exit::from_os_error)?;
    let mut command = Vec::new();
    let launch = LaunchId::generate().map_err(error_with_cause)?;
    let mut starting = supervisor::launch(locked, &launch, &runtime, &capability, |endpoint| {
        command = head;
        command.push(format!("listen={endpoint}"));
        command.extend(tail);
        let arguments = command[1..].iter().map(OsString::from).collect();
        let mut vmm = VmmCommand::new(&command[0], arguments, directory);
        // OpenVMM inherits the checked files that the prepared launch passes, and nothing else.
        vmm.inherit = prepared.inherited();
        Ok(vmm)
    })
    .map_err(error_with_cause)?;
    let endpoint = starting.endpoint().to_owned();
    let maps = prepared.plan().mapping_table();
    // Whether the guest agent acknowledged a request to shut down after a mapping failed.
    let mut stopping = false;
    let ready =
        check_ready(&endpoint, starting.child(), &capability, timeout).and_then(|mut session| {
            // Workloads run only once the guest agent has mapped every share in order, so a
            // mapping that fails ends the start. A sandbox without shares maps nothing.
            if maps.is_empty() {
                return Ok(());
            }
            session.map_host_paths(&maps, timeout).inspect_err(|error| {
                // The agent then refuses workloads, so the guest shuts down as a stop has it do:
                // it unmounts what it mapped, the export, the overlay, and the upper before the
                // VM powers off. A failed transfer closed the session, so a guest that stopped
                // answering is terminated at once.
                if !matches!(error, Exit::Interrupted) {
                    stopping = session.stop(timeout).is_ok();
                }
            })
        });
    // The start holds the checked files until the guest is ready, or until it has ended an
    // OpenVMM whose start failed.
    let mut prepared = Some(prepared);
    let ready = ready.and_then(|()| {
        // The guest is ready, so OpenVMM holds the images: the checked files can go, with
        // OpenVMM's copies of them on Windows, which releases their guards, while OpenVMM keeps
        // the scratch image's claim.
        prepared
            .take()
            .expect("the start holds the checked files until the guest is ready")
            .release(starting.process())
            .map_err(error_with_cause)
    });
    let Err(error) = ready else {
        return starting.commit().map(drop).map_err(error_with_cause);
    };
    // OpenVMM closes the control endpoint as it tears the VM down, before it publishes its
    // outcome report and exits, so only a closed endpoint, or a guest that shuts down, warrants
    // waiting for it. The session reports an OpenVMM that exited before it served the endpoint
    // as a closed endpoint too, whose wait then ends at once.
    let grace = if stopping || matches!(error, Exit::EndpointClosed) {
        timeout
    } else {
        0.0
    };
    let status = end_failed_start(starting.child(), grace, &command)?;
    // OpenVMM has exited, so the launch ends: its capability, control socket, report staging
    // file, and journal go, and its log and outcome report stay.
    let unended = match starting.abandon() {
        Ok(Recovery::Indeterminate(reason)) => Some(reason),
        Ok(_) => None,
        Err(failure) => Some(failure.message().to_owned()),
    };
    // The checked files can go too, which releases their guards and the scratch image's claim.
    drop(prepared);
    let error = match status {
        // A guest that shut down after a mapping failed exits as a stopped one does, so the
        // mapping's failure tells why the start failed.
        Some(status) if !stopping && !matches!(error, Exit::Interrupted) => Exit::error(
            ErrorCode::Exited,
            startup_exit_message(&outcome_path, status)?,
        ),
        _ => error,
    };
    if let Some(reason) = unended
        && let Some(message) = error.message()
    {
        return Err(Exit::error(
            error.code().unwrap_or(ErrorCode::Other),
            format!("{message}; the launch could not be ended, and remains for recovery: {reason}"),
        ));
    }
    Err(error)
}

/// Returns the error of a start whose distro layer or scratch image fails the check of its
/// binding: a binding that no longer holds needs a new provision, which verifies the layer and
/// binds the image again, while a scratch image that another sandbox claims, and a failure of the
/// host, do not. The error keeps its code.
fn binding_error(error: crate::Error) -> Exit {
    match error.kind() {
        ErrorKind::Io | ErrorKind::TimedOut => error.into(),
        _ => Exit::error(
            error.code(),
            format!(
                "{}; deprovision the sandbox and provision it again",
                error.message()
            ),
        ),
    }
}

/// Waits until the guest of a VM that starts answers on `endpoint`, which `launched`, the OpenVMM
/// process of the start, serves, or until OpenVMM exits, checks that the guest provides the
/// features of [`REQUIRED_FEATURES`], and returns the session, on which the caller sends the
/// guest what it needs before the launch commits.
fn check_ready(
    endpoint: &str,
    launched: &mut Launched,
    capability: &[u8; CAPABILITY_LEN],
    timeout: f64,
) -> Result<Session> {
    let mut session = Session::connect_launched(endpoint, launched, capability, timeout)?;
    session.ping(timeout)?;
    let features = session.features(timeout)?;
    let missing = features.missing(REQUIRED_FEATURES);
    if !missing.is_empty() {
        return Err(Exit::error(
            ErrorCode::GuestFeaturesMissing,
            format!(
                "the guest image lacks control features that layered sandboxes need ({}); use \
                 an NVX release or build that includes them",
                missing.join(", ")
            ),
        ));
    }
    Ok(session)
}

/// Ends the OpenVMM process of a failed start, as `_end_failed_start` does: returns OpenVMM's
/// status if it exits on its own within `grace` seconds, or terminates it and returns `None`.
/// Terminating OpenVMM while it publishes its outcome report would lose the report, and leave
/// only its staging file, which the store removes as the launch ends.
fn end_failed_start(
    launched: &mut Launched,
    grace: f64,
    command: &[String],
) -> Result<Option<i64>> {
    let waited = if grace > 0.0 {
        wait_interruptibly(launched, Deadline::after(grace))
    } else {
        Ok(())
    };
    let status = launched
        .wait_timeout(Duration::ZERO)
        .map_err(Exit::from_os_error)?;
    if status.is_none() {
        launched.terminate().map_err(Exit::from_os_error)?;
        let exited = launched
            .wait_timeout(Duration::from_secs(5))
            .map_err(Exit::from_os_error)?;
        if exited.is_none() {
            launched.kill().map_err(Exit::from_os_error)?;
            let killed = launched
                .wait_timeout(Duration::from_secs(5))
                .map_err(Exit::from_os_error)?;
            if killed.is_none() {
                return Err(Exit::error(
                    ErrorCode::TimedOut,
                    format!("command {command:?} did not exit within 5 seconds of its kill"),
                ));
            }
        }
    }
    // Ctrl-C during the grace period ends the start as interrupted, after OpenVMM.
    waited?;
    Ok(status)
}

/// Waits until `launched` exits or `deadline` passes, failing if Ctrl-C arrives.
fn wait_interruptibly(launched: &mut Launched, deadline: Deadline) -> Result {
    loop {
        let slice = deadline
            .remaining()
            .map_or(RETRY_INTERVAL, |left| left.min(RETRY_INTERVAL));
        crate::host::cli::check_interrupt()?;
        if launched
            .wait_timeout(slice)
            .map_err(Exit::from_os_error)?
            .is_some()
            || deadline.passed()
        {
            return Ok(());
        }
    }
}

/// Describes how OpenVMM ended a start, preferring its outcome report, which keeps the guest's
/// own status: its category and its integer status, when it holds both.
fn startup_exit_message(outcome_path: &Path, status: i64) -> Result<String> {
    let report = read_openvmm_outcome(outcome_path).ok();
    if let Some(outcome) = report.as_ref().and_then(|report| report.get("outcome"))
        && let Some(Value::String(category)) = outcome.get("category")
        && let Some(status_code) = outcome.get("status_code").and_then(Value::as_i64)
    {
        return Ok(format!(
            "OpenVMM exited during startup: {category} status {status_code}"
        ));
    }
    Ok(format!(
        "OpenVMM exited during startup with status {status}"
    ))
}

/// Runs a workload in a running sandbox, as `exec_workload` does. The lock is held only while the
/// running VM is found, so a long workload cannot delay a stop.
pub(crate) fn exec_workload(
    state_path: &str,
    artifacts: &Artifacts<'_>,
    request: &ExecRequest,
    response_timeout: f64,
) -> Result<ExecResult> {
    let state_dir = prepare_state_directory(state_path, false)?;
    let running = {
        let mut lock = LifecycleLock::acquire(&state_dir, response_timeout, false)?;
        running_vm(&state_dir, lock.locked(), artifacts)?
    };
    let mut session = connect(&running.vmm, &running.capability, response_timeout)?;
    session.exec(request, response_timeout)
}
