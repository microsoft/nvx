//! Starting a managed sandbox and executing workloads in its running VM.

use std::path::Path;
use std::time::Instant;

use serde_json::Value;

use super::directory::prepare_state_directory;
use super::{
    CONFIG_NAME, Found, REQUIRED_FEATURES, connect, find_vm, mismatched, require_config,
    running_vm, runtime_for,
};
use crate::activation::{Journal, MachineIdMode, OverlayMode, View};
use crate::host::cli::deadline::Deadline;
use crate::host::cli::sandbox::lock::LifecycleLock;
use crate::host::cli::sandbox::state::{Config, read_config};
use crate::host::cli::sandbox::{Artifacts, error_with_cause};
use crate::host::cli::session::{ExecRequest, ExecResult, Session};
use crate::host::cli::{Exit, Result, output};
use crate::host::config::{
    Boot, ConsoleAttachment, LayeredRoot, NetworkConfig, RootConfig, UpperConfig, VmConfig,
};
use crate::host::process::KILL_TIMEOUT;
use crate::host::runtime::Runtime;
use crate::host::sandbox::distro::DISTRO_POLICY_REVISION;
use crate::host::sandbox::scratch::CLAIM_RELEASE_WAIT;
use crate::host::sandbox::{SandboxLaunch, SandboxPlan};
use crate::host::vm::{self, SavedVm, VmDir};
use crate::net::Ipv4Cidr;
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
    let plan = launch.bound_plan(&binding)?;
    // Keep provision's refusal of caller-owned nvx_* tokens, without launching its old plan.
    plan.kernel_command_line(config.cmdline())?;
    artifacts.require()?;
    let runtime = runtime_for(artifacts, &config, artifacts.launch_identity()?)?;
    let config = vm_config(&config, &launch, &plan)?;
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

fn vm_config(config: &Config, launch: &SandboxLaunch, plan: &SandboxPlan) -> Result<VmConfig> {
    let (uid, gid) = launch.settings().workload_identity;
    let mut command_line = format!("nvx_sandbox=1 nvx_mode=managed nvx_identity={uid}:{gid}");
    if !config.cmdline().is_empty() {
        command_line.push(' ');
        command_line.push_str(config.cmdline());
    }
    let network = launch
        .network()
        .map(|network| -> Result<NetworkConfig> {
            Ok(NetworkConfig {
                guest_network: Some(
                    Ipv4Cidr::parse(network.guest_network())
                        .map_err(|error| Exit::error(ErrorCode::Unsupported, error.to_string()))?,
                ),
                policy: network.policy().clone(),
            })
        })
        .transpose()?;
    Ok(VmConfig {
        processors: 1,
        memory_mib: config.memory_mib(),
        root: RootConfig::Layered(LayeredRoot {
            image: launch.distro().path().to_path_buf(),
            format: plan.distro().image(),
            gpt_first_partition_bound: None,
            upper: match launch.scratch() {
                Some(path) => UpperConfig::Scratch {
                    path: path.to_path_buf(),
                    journal: Journal::Allowed,
                },
                None => UpperConfig::Ram {
                    max_memory_percent: UpperConfig::DEFAULT_MAX_MEMORY_PERCENT,
                },
            },
            overlay: OverlayMode::TrustedLowers,
        }),
        view: View {
            hostname: launch.settings().hostname.as_bytes().to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network,
        filesystem: launch.export().cloned(),
        console: ConsoleAttachment::Disconnected,
        boot: Some(Boot {
            command_line,
            memory_capacity_mib: None,
            capture: None,
        }),
    })
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
        running_vm(&state_dir, &mut lock, artifacts)?
    };
    let mut session = connect(&running.saved, artifacts, response_timeout)?;
    session.exec(request, response_timeout)
}
