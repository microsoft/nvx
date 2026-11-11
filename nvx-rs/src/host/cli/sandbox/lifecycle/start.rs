//! Starting a managed sandbox and executing workloads in its running VM.

use std::path::Path;
use std::time::Instant;

use serde_json::Value;

use super::directory::prepare_state_directory;
use super::{
    CONFIG_NAME, Found, REQUIRED_FEATURES, connect, find_vm, mismatched, require_config,
    running_vm, runtime_for,
};
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL};
use crate::host::cli::exec_lock::ExecLock;
use crate::host::cli::sandbox::lock::LifecycleLock;
use crate::host::cli::sandbox::state::{Config, read_config};
use crate::host::cli::sandbox::{Artifacts, error_with_cause};
use crate::host::cli::session::{ExecRequest, ExecResult, Session};
use crate::host::cli::{Exit, Result, output};
use crate::host::process::KILL_TIMEOUT;
use crate::host::runtime::Runtime;
use crate::host::sandbox::distro::DISTRO_POLICY_REVISION;
use crate::host::sandbox::scratch::CLAIM_RELEASE_WAIT;
use crate::host::vm::{self, SavedVm, VmDir};
use crate::{ErrorCode, ErrorKind};

/// Starts a provisioned sandbox, as `start` does.
pub(crate) fn start(state_path: &str, artifacts: &Artifacts<'_>, timeout: f64) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    start_locked(&state_dir, &mut lock, artifacts, timeout)
}

/// Starts the sandbox of a locked state directory, as `_start` does, through the launch journal
/// of its store.
fn start_locked(
    state_dir: &Path,
    lock: &mut LifecycleLock,
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
    match find_vm(lock, artifacts)? {
        Found::Nothing | Found::Exited => {}
        Found::Live(..) => {
            return Err(Exit::error(
                ErrorCode::SandboxRunning,
                "sandbox is already running or has stale runtime state",
            ));
        }
        Found::Mismatched(saved, _, differences) => return Err(mismatched(&saved, &differences)),
    }
    let config = Config::new(config)?;
    let launch = config.launch()?;
    let binding = config.distro_binding();
    let scratch_binding = config.scratch_binding()?;
    let distro = binding
        .check(launch.distro(), DISTRO_POLICY_REVISION)
        .map_err(binding_error)?;
    let scratch = match (scratch_binding, launch.scratch()) {
        (Some(scratch_binding), Some(scratch)) => Some(
            scratch_binding
                .check(scratch, Instant::now() + CLAIM_RELEASE_WAIT)
                .map_err(binding_error)?,
        ),
        _ => None,
    };
    let plan = launch.bound_plan(binding)?;
    // Keep provision's refusal of caller-owned nvx_* tokens, without launching its old plan.
    plan.kernel_command_line(config.cmdline())?;
    artifacts.require()?;
    let runtime = runtime_for(artifacts, &config, artifacts.launch_identity()?)?;
    let config = config.vm_config(&launch)?;
    let dir = VmDir::open(state_dir).map_err(error_with_cause)?;
    lock.clear_outputs()?;
    let mut prepared = vm::create(&runtime, &config, None, &dir).map_err(error_with_cause)?;
    if let Some(scratch) = &scratch {
        #[cfg(not(windows))]
        prepared.inherit(scratch.file().try_clone().map_err(Exit::from_os_error)?);
        #[cfg(windows)]
        prepared.inherit(scratch.claim().try_clone().map_err(Exit::from_os_error)?);
    }
    let mut starting = lock
        .locked()
        .begin_launch(&prepared, &runtime.identity)
        .map_err(error_with_cause)?;
    let launched = vm::spawn(prepared).map_err(error_with_cause)?;
    let saved = SavedVm {
        vm_dir: dir,
        endpoint: launched.outputs.endpoint.clone(),
        capability: *launched.capability.as_bytes(),
        process: launched.process.reference().clone(),
        runtime: runtime.identity.clone(),
    };
    let mut stopping = false;
    let ready = starting
        .record_process(&saved.process)
        .map_err(error_with_cause)
        .and_then(|()| check_ready(&saved, &runtime, timeout))
        .and_then(|mut session| {
            let maps = config.mapping_table();
            if maps.is_empty() {
                return Ok(());
            }
            session.map_host_paths(&maps, timeout).inspect_err(|error| {
                if !matches!(error, Exit::Interrupted) {
                    match session.stop(timeout) {
                        Ok(()) => stopping = true,
                        Err(error) => output::print_stderr(&format!(
                            "warning: startup guest shutdown failed: {error:?}"
                        )),
                    }
                }
            })
        });
    let Err(mut error) = ready else {
        return starting.commit(&saved).map_err(error_with_cause);
    };
    drop(starting);
    let grace = if stopping || matches!(error, Exit::EndpointClosed) {
        Deadline::after(timeout)
    } else {
        Deadline::after(0.0)
    };
    if let Err(wait) = super::stop::wait_for_exit(&launched.process, grace) {
        if !matches!(wait, Exit::Interrupted) {
            return Err(wait);
        }
        error = wait;
    }
    let now = Instant::now();
    let stopped = vm::stop(
        &launched.process,
        &launched.outputs,
        |_| false,
        now,
        now + KILL_TIMEOUT,
    )
    .map_err(error_with_cause)?;
    lock.locked()
        .recover(Instant::now() + KILL_TIMEOUT)
        .map_err(error_with_cause)?;
    lock.clear_runtime()?;
    drop((distro, scratch));
    let report = stopped.report.map_err(error_with_cause)?;
    if stopped.self_exited && !stopping && !matches!(error, Exit::Interrupted) {
        let detail = match report {
            Some(report) => match (report.category, report.status_code) {
                (Some(category), Some(status)) => format!(": {category} status {status}"),
                _ => String::new(),
            },
            None => " without an outcome report".to_owned(),
        };
        return Err(Exit::error(
            ErrorCode::Exited,
            format!("OpenVMM exited during startup{detail}"),
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
fn check_ready(saved: &SavedVm, runtime: &Runtime, timeout: f64) -> Result<Session> {
    let mut session = Session::reattach(saved, runtime, timeout)?;
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

/// Runs a workload under exec admission. The lifecycle lock is released before connecting, so
/// a long workload cannot delay a stop.
///
/// Admission tries the sandbox's exec lock once at least, and retries it only before `wait`
/// seconds pass, even when a retry waited past them for the lifecycle lock, which
/// `response_timeout` bounds.
pub(crate) fn exec_workload(
    state_path: &str,
    artifacts: &Artifacts<'_>,
    request: &ExecRequest,
    response_timeout: f64,
    wait: f64,
) -> Result<ExecResult> {
    let busy = || {
        Exit::error(
            ErrorCode::SandboxBusy,
            "another exec in the sandbox is still in progress",
        )
    };
    let state_dir = prepare_state_directory(state_path, false)?;
    let deadline = Deadline::after(wait);
    let mut retry = false;
    let (_exec, running) = loop {
        let mut lock = LifecycleLock::acquire(&state_dir, response_timeout, false)?;
        require_config(&state_dir)?;
        if retry && deadline.passed() {
            return Err(busy());
        }
        if let Some(exec) = ExecLock::try_acquire(lock.locked())? {
            let running = running_vm(&state_dir, &mut lock, artifacts)?;
            break (exec, running);
        }
        drop(lock);
        retry = true;
        if !deadline.pause(RETRY_INTERVAL)? {
            return Err(busy());
        }
    };
    let mut session = connect(&running.saved, artifacts, response_timeout)?;
    session.exec(request, response_timeout)
}
