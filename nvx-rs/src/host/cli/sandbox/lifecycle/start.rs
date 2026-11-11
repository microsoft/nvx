//! Starting a managed sandbox and executing workloads in its running VM, and running one workload
//! in a VM of its own, without a state directory.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

use serde_json::Value;

use super::directory::prepare_state_directory;
use super::outcome::{copy_openvmm_outcome, validate_outcome_destination};
use super::{
    CONFIG_NAME, Found, ProvisionConfig, REQUIRED_FEATURES, connect, find_vm, mismatched,
    require_config, running_vm, runtime_for,
};
use crate::host::cli::artifacts::locate;
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL};
use crate::host::cli::download::Hold;
use crate::host::cli::exec_lock::ExecLock;
use crate::host::cli::sandbox::lock::LifecycleLock;
use crate::host::cli::sandbox::state::{
    Config, RecordedDistro, RecordedScratch, read_config, serialize_launch,
};
use crate::host::cli::sandbox::{
    Artifacts, Options, error_with_cause, hypervisor, path_string, refuse_own_files,
};
use crate::host::cli::session::{ExecRequest, ExecResult, Session};
use crate::host::cli::shell::format_command;
use crate::host::cli::store::StateFile;
use crate::host::cli::{Exit, Result, hex, io_error, output, random_bytes};
use crate::host::process::KILL_TIMEOUT;
use crate::host::report::Report;
use crate::host::runtime::{ArtifactIdentity, Runtime, RuntimeIdentity};
use crate::host::sandbox::SandboxLaunch;
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
    refuse_own_files(
        Some(("state directory", state_dir)),
        &launch,
        Some(&locate(artifacts.root)?),
    )?;
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
        return Err(exited_during_startup(report));
    }
    Err(error)
}

/// Returns the error of a launch whose OpenVMM exited on its own before its guest was ready, with
/// the category and status of `report`, its outcome report, if it holds them.
fn exited_during_startup(report: Option<Report>) -> Exit {
    let detail = match report {
        Some(report) => match (report.category, report.status_code) {
            (Some(category), Some(status)) => format!(": {category} status {status}"),
            _ => String::new(),
        },
        None => " without an outcome report".to_owned(),
    };
    Exit::error(
        ErrorCode::Exited,
        format!("OpenVMM exited during startup{detail}"),
    )
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

/// Runs one workload in a VM of its own and stops the VM, as `sandbox run` does, and returns how
/// the workload ended, or nothing for a dry run, which launches nothing.
///
/// The VM is the one that `start` launches for the configuration that `provision` records for
/// `launch`, whose distro layer `distro` is at the path that `provision` records, with the options
/// of `options`: the run serializes that configuration and reads it back as `start` does, then
/// launches it as `start` does, but in a temporary VM directory rather than a state directory.
/// Before it downloads or launches anything, it refuses what `start` refuses of the configuration
/// and `exec` of `request`, after a missing artifact; it then refuses the shares that `start`
/// refuses, with its VM directory in place of a state directory, and those of the OpenVMM that it
/// runs, and prints the OpenVMM command as `>> COMMAND`.
///
/// Once nvx-init is ready and has mapped the shares, the run executes `request` over NVXC, asks
/// nvx-init to stop the VM, and waits up to `--timeout` seconds for the VM to exit before it
/// forces it down. A VM that exits before it is ready fails the run as it fails `start`. Once the
/// VM has exited, the run forwards OpenVMM's outcome report to `--outcome-report`, if given, and
/// removes the VM directory, which it keeps only if it cannot confirm that exit. It holds `held`,
/// the lock of the downloads, until the VM is ready or has failed to be.
pub(crate) fn run(
    launch: &SandboxLaunch,
    distro: &RecordedDistro,
    artifacts: &Artifacts<'_>,
    options: &Options,
    request: &ExecRequest,
    held: Hold,
) -> Result<Option<ExecResult>> {
    let hypervisor = hypervisor(options.hypervisor);
    let provision = ProvisionConfig {
        hypervisor,
        memory_mib: options.memory_mib,
        cmdline: &options.cmdline,
        ram_upper_percent: options.ram_upper_percent,
    };
    let recorded = record(launch, distro, &provision).and_then(|(config, launch)| {
        // As `start` does, refuse the user's own nvx_* tokens, and the command line that the
        // launch would refuse, such as one that holds double quotes.
        launch
            .bound_plan(config.distro_binding())?
            .kernel_command_line(config.cmdline())?;
        let vm_config = config.vm_config(&launch)?;
        vm_config.check_command_line().map_err(error_with_cause)?;
        Session::exec_payload(request)?;
        Ok((config, launch, vm_config))
    });
    // A run that its options refuse downloads nothing, and reports a missing artifact first.
    let hold = match recorded {
        Ok(_) => artifacts.ensure_under(held, hypervisor)?,
        Err(_) => held,
    };
    artifacts.require()?;
    let (config, launch, vm_config) = recorded?;
    let name = hex(&random_bytes::<16>()?);
    let path = std::env::temp_dir().join(format!("nvx-sandbox-run-{name}"));
    refuse_own_files(
        Some(("VM directory", &path)),
        &launch,
        Some(&locate(artifacts.root)?),
    )?;
    if !options.dry_run
        && let Some(report) = &options.outcome_report
    {
        validate_outcome_destination(report)?;
    }
    // A run never records or reattaches its VM, so it names no runtime identity of its artifacts.
    let identity = ArtifactIdentity::new(format!("unrecorded-run:{name}"))?;
    let runtime = runtime_for(
        artifacts,
        &config,
        RuntimeIdentity::new(identity.clone(), identity.clone(), identity),
    )?;
    let images = if options.dry_run {
        None
    } else {
        let distro = config
            .distro_binding()
            .check(launch.distro(), DISTRO_POLICY_REVISION)
            .map_err(error_with_cause)?;
        let scratch = match (config.scratch_binding()?, launch.scratch()) {
            (Some(binding), Some(scratch)) => Some(
                binding
                    .check(scratch, Instant::now() + CLAIM_RELEASE_WAIT)
                    .map_err(error_with_cause)?,
            ),
            _ => None,
        };
        Some((distro, scratch))
    };
    if path.try_exists().map_err(Exit::from_os_error)? {
        return Err(Exit::error(
            ErrorCode::Io,
            "the temporary VM directory already exists",
        ));
    }
    vm::create_private_dir(&path).map_err(error_with_cause)?;
    let dir = VmDir::open(&path).map_err(error_with_cause)?;
    let timeout = options.timeout;
    let mut cleanup = true;
    let result = (|| {
        let mut prepared = vm::create(&runtime, &vm_config, None, &dir)
            .map_err(error_with_cause)?
            .for_foreground();
        let command = std::iter::once(path_string(&runtime.openvmm))
            .chain(prepared.arguments().iter().map(|argument| {
                argument.to_str().map(str::to_owned).ok_or_else(|| {
                    Exit::error(ErrorCode::Unsupported, "an OpenVMM argument is not Unicode")
                })
            }))
            .collect::<Result<Vec<_>>>()?;
        output::print_stdout(&format!(">> {}", format_command(&command)))
            .map_err(Exit::from_os_error)?;
        let Some((_, scratch)) = &images else {
            return Ok(None);
        };
        if let Some(scratch) = scratch {
            #[cfg(not(windows))]
            prepared.inherit(scratch.file().try_clone().map_err(Exit::from_os_error)?);
            #[cfg(windows)]
            prepared.inherit(scratch.claim().try_clone().map_err(Exit::from_os_error)?);
        }
        let launched = vm::spawn(prepared).map_err(|error| {
            cleanup = error.kind() != ErrorKind::StillRunning;
            error_with_cause(error)
        })?;
        cleanup = false;
        let saved = SavedVm {
            vm_dir: dir.clone(),
            endpoint: launched.outputs.endpoint.clone(),
            capability: *launched.capability.as_bytes(),
            process: launched.process.reference().clone(),
            runtime: runtime.identity.clone(),
        };
        let mut session = None;
        let ready = check_ready(&saved, &runtime, timeout).and_then(|ready| {
            let maps = vm_config.mapping_table();
            let session = session.insert(ready);
            if maps.is_empty() {
                Ok(())
            } else {
                session.map_host_paths(&maps, timeout)
            }
        });
        drop(hold);
        let started = ready.is_ok();
        let executed = ready.and_then(|()| {
            session
                .as_mut()
                .expect("a ready VM has a session")
                .exec(request, timeout)
        });
        // A VM whose control endpoint closed is shutting down already.
        let requested = match session.take() {
            Some(mut session)
                if !matches!(executed, Err(Exit::Interrupted | Exit::EndpointClosed)) =>
            {
                Some(session.stop(timeout))
            }
            _ => None,
        };
        // So is one whose endpoint closed while it was asked to stop: it gets a stop's grace.
        let stopping = matches!(requested, Some(Ok(()) | Err(Exit::EndpointClosed)));
        let grace = if stopping || matches!(executed, Err(Exit::EndpointClosed)) {
            Deadline::after(timeout)
        } else {
            Deadline::after(0.0)
        };
        let mut executed = executed;
        // No record of the VM outlives the run, so it forces the VM down whatever ended the wait.
        if let Err(error) = super::stop::wait_for_exit(&launched.process, grace) {
            executed = Err(error);
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
        cleanup = true;
        if let Some(report) = &options.outcome_report
            && let Err(error) = copy_openvmm_outcome(&launched.outputs.outcome, report)
        {
            error.report();
        }
        // Ctrl-C during the request ends the run as interrupted, which is no failed shutdown.
        if let Some(Err(error)) = &requested
            && !matches!(error, Exit::Interrupted)
        {
            output::print_stderr(&format!(
                "warning: managed guest shutdown failed: {error:?}"
            ));
        }
        if !started {
            let report = stopped.report.map_err(error_with_cause)?;
            if stopped.self_exited && !stopping && !matches!(executed, Err(Exit::Interrupted)) {
                return Err(exited_during_startup(report));
            }
        } else if executed.is_ok() && stopped.forced {
            output::print_stderr("warning: OpenVMM was forcibly terminated after the run");
        }
        executed.map(Some)
    })();
    if cleanup && let Err(error) = remove_vm_directory(&dir) {
        output::print_stderr(&format!(
            "warning: cannot remove the temporary VM directory {}: {error:?}",
            dir.path().display()
        ));
    }
    result
}

/// Returns the configuration that `provision` records for `launch`, whose distro layer `distro` is
/// at the path that `provision` records, with the options of `config`, as `start` reads it back,
/// and the launch that `start` makes of it, which checks its shares again.
fn record(
    launch: &SandboxLaunch,
    distro: &RecordedDistro,
    config: &ProvisionConfig<'_>,
) -> Result<(Config, SandboxLaunch)> {
    let scratch = RecordedScratch::new(launch)?;
    let record = serialize_launch(launch, distro, scratch.as_ref(), config)?;
    let config = Config::new(
        serde_json::from_str(&record)
            .map_err(|error| Exit::error(ErrorCode::Other, error.to_string()))?,
    )?;
    let launch = config.launch()?;
    Ok((config, launch))
}

/// Removes the temporary VM directory of a run, with what its VM left in it: OpenVMM's log and
/// outcome report, its control socket on Linux, and the staging file of a report that OpenVMM
/// did not finish writing, which [`StateFile::owns_temporary`] names.
fn remove_vm_directory(dir: &VmDir) -> Result {
    let remove = |path: &Path| match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(ErrorCode::Io, &error, path)),
    };
    for file in [StateFile::Log, StateFile::Outcome, StateFile::Socket] {
        remove(&dir.path().join(file.name()))?;
    }
    let failed = |error: io::Error| io_error(ErrorCode::Io, &error, dir.path());
    for entry in fs::read_dir(dir.path()).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(StateFile::owns_temporary)
        {
            remove(&entry.path())?;
        }
    }
    fs::remove_dir(dir.path()).map_err(failed)
}

#[cfg(test)]
mod tests;
