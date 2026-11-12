//! Starting a managed sandbox and executing workloads in its running VM, and running one workload
//! in a VM of its own, without a state directory.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Instant;

use serde_json::Value;

use super::directory::prepare_state_directory;
use super::outcome::{copy_openvmm_outcome, validate_outcome_destination};
use super::{
    CONFIG_NAME, Found, ProvisionConfig, REQUIRED_FEATURES, connect, find_vm, mismatched,
    require_config, running_vm, runtime_for,
};
use crate::activation::{ActivationRecord, TransactionId};
use crate::host::cli::artifacts::locate;
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL};
use crate::host::cli::download::Hold;
use crate::host::cli::exec_lock::ExecLock;
use crate::host::cli::sandbox::lock::LifecycleLock;
use crate::host::cli::sandbox::state::{
    Config, RecordedDistro, RecordedScratch, Source, read_config, serialize_launch,
};
use crate::host::cli::sandbox::{
    Artifacts, Options, error_with_cause, export_tokens, path_string, refuse_own_files,
};
use crate::host::cli::session::{ExecRequest, ExecResult, Session};
use crate::host::cli::shell::format_command;
use crate::host::cli::snapshots::Store;
use crate::host::cli::store::StateFile;
use crate::host::cli::{Exit, Result, hex, io_error, output, random_bytes};
use crate::host::config::{COMMAND_LINE_BUDGET, RootConfig, UpperConfig, VmConfig};
use crate::host::platform;
use crate::host::process::KILL_TIMEOUT;
use crate::host::report::Report;
use crate::host::restore::{Attribution, RestoreError, RestoreEvidence};
use crate::host::restore_driver::{self, Cancel, ControlRestoreError, Workload};
use crate::host::runtime::{ArtifactIdentity, Runtime, RuntimeIdentity};
use crate::host::sandbox::distro::DISTRO_POLICY_REVISION;
use crate::host::sandbox::scratch::{CLAIM_RELEASE_WAIT, CheckedScratch};
use crate::host::session::{ExecEvent, SessionError};
use crate::host::snapshot::OPENVMM_DIR;
use crate::host::vm::{self, Launched, Prepared, SavedVm, VmDir};
use crate::protocol::cmdline::{self, Parameter};
use crate::protocol::root::RootLayout;
use crate::protocol::workload::WorkloadIdentity;
use crate::protocol::{self, ExitCategory, HostMap, WorkloadEnvironment};
use crate::{ErrorCode, ErrorKind};

/// Starts a provisioned sandbox, as `start` does. A workload of a restored sandbox that does not
/// exit with status 0 fails the start with an error that names how it ended, or, if
/// `forward_status`, as `run --restore-snapshot` asks, ends it with [`Exit::Status`] of its
/// status, as `sandbox run` ends: 128 plus the signal's number for a workload that a signal ends.
pub(crate) fn start(
    state_path: &str,
    artifacts: &Artifacts<'_>,
    timeout: f64,
    forward_status: bool,
) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    start_locked(&state_dir, &mut lock, artifacts, timeout, forward_status)
}

/// Starts the sandbox of a locked state directory, as `_start` does, through the launch journal
/// of its store.
fn start_locked(
    state_dir: &Path,
    lock: &mut LifecycleLock,
    artifacts: &Artifacts<'_>,
    timeout: f64,
    forward_status: bool,
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
    config.check()?;
    let source = config.source()?;
    let image = config.distro();
    let scratch_image = config.scratch();
    refuse_own_files(
        Some(("state directory", state_dir)),
        image.path(),
        scratch_image.as_deref(),
        config.export(),
        Some(&locate(artifacts.root)?),
    )?;
    // The view of each restore links the snapshot's files, which a share of the store would
    // give the guest to change.
    if let Some(source) = &source {
        refuse_own_files(
            Some(("snapshot store", &source.store)),
            image.path(),
            scratch_image.as_deref(),
            config.export(),
            None,
        )?;
    }
    let binding = config.distro_binding();
    let scratch_binding = config.scratch_binding()?;
    let distro = binding
        .check(&image, DISTRO_POLICY_REVISION)
        .map_err(binding_error)?;
    let scratch = match (scratch_binding, scratch_image) {
        (Some(scratch_binding), Some(scratch)) => Some(
            scratch_binding
                .check(&scratch, Instant::now() + CLAIM_RELEASE_WAIT)
                .map_err(binding_error)?,
        ),
        _ => None,
    };
    let vm_config = config.vm_config()?;
    check_command_line(&config, &vm_config)?;
    artifacts.require()?;
    let runtime = runtime_for(artifacts, &config, artifacts.launch_identity()?)?;
    let restore = source.as_ref().map(|source| Restore {
        source,
        identity: config.workload_identity(),
        forward_status,
    });
    let started = launch_locked(
        lock,
        &runtime,
        vm_config,
        restore,
        scratch,
        timeout,
        vm::spawn,
    );
    drop(distro);
    started
}

/// What a start restores in place of a cold boot: the named snapshot of a restored sandbox, and
/// the identity of the workload that the restore runs, and how a workload that does not exit with
/// status 0 ends it.
struct Restore<'a> {
    /// The snapshot and the workload, as the sandbox's configuration records them.
    source: &'a Source,
    /// The workload's UID and GID.
    identity: (u32, u32),
    /// Whether a workload that does not exit with status 0, such as one that a signal ends, ends
    /// the start with [`Exit::Status`] of its status, rather than with an error that names it.
    forward_status: bool,
}

/// Launches the VM of the sandbox of a locked state directory through the launch journal of its
/// store, with `spawn`: a cold boot of `config`, or a restore of `restore`, and commits it once
/// its guest is ready.
///
/// A restore holds its snapshot's lock from before it creates its view, which links the
/// snapshot's files into the state directory, until the start ends, past the restore-ready event
/// of the verified VM, or until the VM has verifiably exited: the lock first lets it remove the
/// view of an earlier start. It restores the snapshot's online processors and runs the restored
/// workload until it exits. A command that dies before the guest is ready leaves its journal,
/// whose recovery ends the VM, as for an interrupted cold boot. A start that cannot confirm that
/// its VM ended leaves the journal to the next command's recovery as well, and its lock goes with
/// the command.
///
/// A restore's guest accepts only a fresh upper, so a restore with a scratch image refuses to
/// start once an earlier start left its view, which each start creates before its guest can use
/// the image, even if that start failed before its guest did.
fn launch_locked(
    lock: &mut LifecycleLock,
    runtime: &Runtime,
    mut config: VmConfig,
    restore: Option<Restore<'_>>,
    scratch: Option<CheckedScratch>,
    timeout: f64,
    spawn: impl FnOnce(Prepared) -> crate::Result<Launched>,
) -> Result {
    let dir = VmDir::open(lock.locked().dir()).map_err(error_with_cause)?;
    if restore.is_some()
        && matches!(&config.root, RootConfig::Layered(root)
            if matches!(root.upper, UpperConfig::Scratch { .. }))
        && fs::symlink_metadata(dir.path().join(OPENVMM_DIR)).is_ok()
    {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "an earlier start of this restored sandbox may have given its scratch image to a \
             guest, and a restore needs a fresh one; deprovision the sandbox and restore the \
             snapshot again with a fresh scratch image",
        ));
    }
    let store = restore
        .as_ref()
        .map(|restore| Store::open(&restore.source.store))
        .transpose()?;
    let held = match (&store, &restore) {
        (Some(store), Some(restore)) => {
            Some(store.lock(&restore.source.snapshot, Deadline::after(timeout))?)
        }
        _ => None,
    };
    let mut snapshot = None;
    if let Some(held) = &held {
        held.remove_view(&dir)?;
        let opened = held.open()?;
        let key = opened.key();
        config.boot = None;
        config.processors = match key.boot_processors() {
            0 => key.processors(),
            online => online,
        };
        snapshot = Some(opened);
    }
    lock.clear_outputs()?;
    let mut prepared =
        vm::create(runtime, &config, snapshot.as_ref(), &dir).map_err(error_with_cause)?;
    let activation = match &snapshot {
        Some(_) => Some(
            prepared
                .activation_record(TransactionId::from_bytes(random_bytes()?))
                .map_err(error_with_cause)?,
        ),
        None => None,
    };
    // OpenVMM opens a restore's scratch on Windows only while no other handle may write it, so
    // the claim's handle closes there, and the guard that OpenVMM inherits keeps the claim.
    #[cfg(windows)]
    let (scratch, guard) = match scratch {
        Some(scratch) if snapshot.is_some() => (None, Some(scratch.into_file())),
        scratch => (scratch, None),
    };
    #[cfg(windows)]
    for file in scratch
        .as_ref()
        .map(CheckedScratch::claim)
        .into_iter()
        .chain(&guard)
    {
        prepared.inherit(file.try_clone().map_err(Exit::from_os_error)?);
    }
    #[cfg(not(windows))]
    if let Some(scratch) = &scratch {
        prepared.inherit(scratch.file().try_clone().map_err(Exit::from_os_error)?);
    }
    let mut starting = lock
        .locked()
        .begin_launch(&prepared, &runtime.identity)
        .map_err(error_with_cause)?;
    let mut launched = match spawn(prepared) {
        Ok(launched) => launched,
        Err(error) => {
            drop(starting);
            // A spawn that could not confirm the end of its VM leaves it to the journal, whose
            // recovery ends the VM of a restore before the snapshot's lock goes.
            if held.is_some() {
                lock.locked()
                    .recover(Instant::now() + KILL_TIMEOUT)
                    .map_err(error_with_cause)?;
            }
            return Err(error_with_cause(error));
        }
    };
    let saved = SavedVm {
        vm_dir: dir,
        endpoint: launched.outputs.endpoint.clone(),
        capability: *launched.capability.as_bytes(),
        process: launched.process.reference().clone(),
        runtime: runtime.identity.clone(),
    };
    let maps = config.mapping_table();
    let mut stopping = false;
    // The restore driver's error of a restore that failed before its guest was ready.
    let mut failure = None;
    let ready = starting
        .record_process(&saved.process)
        .map_err(error_with_cause)
        .and_then(|()| match (&activation, &restore) {
            (Some(record), Some(restore)) => {
                restore_guest(&mut launched, record, restore, &maps, timeout, &mut failure)
            }
            _ => check_ready(&saved, runtime, timeout).and_then(|mut session| {
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
            }),
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
    // The VM has exited, which ends its claim of the scratch image and its use of the snapshot.
    drop((scratch, held));
    let report = stopped
        .report
        .map_err(|report| with_attribution(error_with_cause(report), failure.as_ref()))?;
    if stopped.self_exited && !stopping && !matches!(error, Exit::Interrupted) {
        return Err(with_attribution(
            exited_during_startup(report),
            failure.as_ref(),
        ));
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

/// Returns the error `error` of a restore that failed before its guest was ready, with the
/// restore driver's error `restore`, if it failed, and what its evidence, such as the guest's
/// status, attributes the failure to.
fn with_attribution(error: Exit, restore: Option<&RestoreError>) -> Exit {
    match (error, restore) {
        (Exit::Error { code, message }, Some(restore)) => {
            Exit::error(code, format!("{message}; {}", attributed(restore)))
        }
        (error, _) => error,
    }
}

/// Returns the message of the restore driver's `error`, with what its evidence attributes the
/// restore's failure to: the request, the host, the snapshot, or the guest's repair, or nothing
/// that it establishes.
fn attributed(error: &RestoreError) -> String {
    let attribution = match error.attribution {
        Attribution::Request => "attributed to the request",
        Attribution::Host => "attributed to the host",
        Attribution::Snapshot => "attributed to the snapshot",
        Attribution::Repair => "attributed to the guest's repair",
        Attribution::Unknown => "of unknown attribution",
    };
    format!("{error}, {attribution}")
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

/// Restores the guest of `launched` from its activation `record` and runs the workload of
/// `restore` after the mapping table `maps`, through the blocking restore driver, within its own
/// budgets, which `timeout` seconds and Ctrl-C cut short, then forwards the workload's output
/// until it exits, which it must do with status 0 in those `timeout` seconds. Another status ends
/// the start with an error, or with [`Exit::Status`] of it if `restore` forwards it. A restore
/// that fails before its guest is ready fails with what its evidence attributes the failure to,
/// and keeps the restore driver's error in `failure`.
fn restore_guest(
    launched: &mut Launched,
    record: &ActivationRecord,
    restore: &Restore<'_>,
    maps: &[HostMap],
    timeout: f64,
    failure: &mut Option<RestoreError>,
) -> Result {
    let (uid, gid) = restore.identity;
    let workload = Workload {
        identity: WorkloadIdentity {
            uid,
            gid,
            create_account: false,
        },
        exec: protocol::Workload {
            argv: restore.source.workload.clone(),
            timeout_ms: 0,
            cwd: None,
            environment: WorkloadEnvironment::Default,
        },
        request_id: 1,
    };
    let deadline = Deadline::after(timeout);
    let cancel = Cancel::new(false);
    let restored = thread::scope(|scope| {
        // Dropping the sender, also as a panic unwinds, ends the watcher.
        let (_restoring, ended) = mpsc::channel::<()>();
        scope.spawn(|| {
            let ended = ended;
            while let Err(RecvTimeoutError::Timeout) = ended.recv_timeout(RETRY_INTERVAL) {
                if platform::interrupted() || deadline.passed() {
                    cancel.store(true, Ordering::Release);
                }
            }
        });
        restore_driver::restore(launched, record, &workload, maps, &cancel)
    });
    // Closing the session at once can lose the workload's Start, so the start keeps the session
    // until the workload ends, reading its events in slices that let Ctrl-C through.
    let error = match restored {
        Ok(mut session) => loop {
            if platform::interrupted() {
                return Err(Exit::Interrupted);
            }
            let slice = deadline.instant().min(Instant::now() + RETRY_INTERVAL);
            match session.next_exec_event(workload.request_id, Some(slice)) {
                Ok(ExecEvent::Stdout(data)) => {
                    output::write_stdout(&data).map_err(Exit::from_os_error)?;
                }
                Ok(ExecEvent::Stderr(data)) => {
                    output::write_stderr(&data).map_err(Exit::from_os_error)?;
                }
                Ok(ExecEvent::Exit {
                    category: ExitCategory::Exit,
                    status: 0,
                }) => return Ok(()),
                Ok(ExecEvent::Exit { status, .. }) if restore.forward_status => {
                    return Err(Exit::Status(status));
                }
                Ok(ExecEvent::Exit { category, status }) => {
                    return Err(Exit::error(
                        ErrorCode::Other,
                        format!("the restored workload ended with {category:?} status {status}"),
                    ));
                }
                Ok(ExecEvent::Rejected { status, category }) => {
                    return Err(Exit::error(
                        ErrorCode::ExecRefused,
                        format!("the guest refused the restored workload: {category} ({status})"),
                    ));
                }
                Err(SessionError::TimedOut) if !deadline.passed() => {}
                Err(_) if platform::interrupted() => return Err(Exit::Interrupted),
                Err(error) => break ControlRestoreError::Start(error),
            }
        },
        Err(_) if platform::interrupted() => return Err(Exit::Interrupted),
        Err(error) => error,
    };
    let code = match &error {
        _ if deadline.passed() => ErrorCode::TimedOut,
        ControlRestoreError::Machine(error) => match error.evidence {
            RestoreEvidence::Expired => ErrorCode::TimedOut,
            RestoreEvidence::Exited => ErrorCode::Exited,
            RestoreEvidence::Io | RestoreEvidence::HostFailure => ErrorCode::Io,
            _ => ErrorCode::Protocol,
        },
        ControlRestoreError::Maps(error) | ControlRestoreError::Start(error) => match error {
            SessionError::TimedOut => ErrorCode::TimedOut,
            // As for a cold start, a closed endpoint proves no exit, and gives OpenVMM its grace
            // to exit by itself before the start ends it.
            SessionError::ProcessExited | SessionError::Closed => return Err(Exit::EndpointClosed),
            SessionError::Io(_) => ErrorCode::Io,
            _ => ErrorCode::Protocol,
        },
    };
    let message = match error {
        ControlRestoreError::Machine(error) => attributed(failure.insert(error)),
        error => error.to_string(),
    };
    Err(Exit::error(
        code,
        format!("snapshot restore failed: {message}"),
    ))
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
/// The VM is the one that `start` launches for the configuration that `provision` records for the
/// options of `provision`, with the distro layer `distro` at the path that `provision` records,
/// over `scratch`, the scratch image of an upper on scratch, as given, and with the options of
/// `options`: the run serializes that configuration and reads it back as `start` does, then
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
    distro: &RecordedDistro,
    scratch: Option<&Path>,
    provision: &ProvisionConfig<'_>,
    artifacts: &Artifacts<'_>,
    options: &Options,
    request: &ExecRequest,
    held: Hold,
) -> Result<Option<ExecResult>> {
    let recorded = record(distro, scratch, provision).and_then(|config| {
        // As `start` does, refuse the user's own nvx_* tokens, and the command line that the
        // launch would refuse, such as one that holds double quotes.
        let vm_config = config.vm_config()?;
        check_command_line(&config, &vm_config)?;
        vm_config.check_command_line().map_err(error_with_cause)?;
        Session::exec_payload(request)?;
        Ok((config, vm_config))
    });
    // A run that its options refuse downloads nothing, and reports a missing artifact first.
    let hold = match recorded {
        Ok(_) => artifacts.ensure_under(held, provision.hypervisor)?,
        Err(_) => held,
    };
    artifacts.require()?;
    let (config, vm_config) = recorded?;
    let name = hex(&random_bytes::<16>()?);
    let path = std::env::temp_dir().join(format!("nvx-sandbox-run-{name}"));
    refuse_own_files(
        Some(("VM directory", &path)),
        config.distro().path(),
        config.scratch().as_deref(),
        config.export(),
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
            .check(&config.distro(), DISTRO_POLICY_REVISION)
            .map_err(error_with_cause)?;
        let scratch = match (config.scratch_binding()?, config.scratch()) {
            (Some(binding), Some(scratch)) => Some(
                binding
                    .check(&scratch, Instant::now() + CLAIM_RELEASE_WAIT)
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

/// Returns the configuration that `provision` records for the options of `config`, with the
/// distro layer `distro` at the path that `provision` records, over `scratch`, the scratch image of
/// an upper on scratch, as given, as `start` reads it back and checks it, shares included.
fn record(
    distro: &RecordedDistro,
    scratch: Option<&Path>,
    config: &ProvisionConfig<'_>,
) -> Result<Config> {
    let scratch = RecordedScratch::new(scratch)?;
    let record = serialize_launch(distro, scratch.as_ref(), config, None)?;
    let config = Config::new(
        serde_json::from_str(&record)
            .map_err(|error| Exit::error(ErrorCode::Other, error.to_string()))?,
    )?;
    config.check()?;
    Ok(config)
}

/// Refuses the kernel command line that a start or a run of the sandbox of `config` launches, as
/// `vm`, its VM's configuration, renders it: one whose user's part holds a NUL, or a token that
/// the sandbox profile or the `--mount` option owns, an `nvx_*` or `virtfs_*` one, as Linux or
/// the guest's init reads it; one that, with the bootstrap tokens that OpenVMM appends for the
/// shares and a NUL, exceeds [`COMMAND_LINE_BUDGET`]; one from which the guest cannot read the
/// boot tokens of its root, as when the user's part adds a virtio-mmio device at the scratch
/// disk's base beside a RAM upper; and one that hides from Linux the tokens that OpenVMM appends
/// to it, with a `--` or an unclosed quote, as Linux reads them.
fn check_command_line(config: &Config, vm: &VmConfig) -> Result {
    let refused = |message: &str| Exit::error(ErrorCode::Unsupported, message);
    let user = config.cmdline().as_bytes();
    if user.contains(&0) {
        return Err(refused("kernel command line contains an embedded NUL"));
    }
    let guest = cmdline::init_tokens(user).map(|token| Parameter {
        name: token.name,
        value: token.value,
    });
    for Parameter { name, .. } in cmdline::parameters(user).chain(guest) {
        let owner = if name.starts_with(b"nvx_") || name.starts_with(b"nvx-") {
            "the sandbox profile"
        } else if name.starts_with(b"virtfs_") || name.starts_with(b"virtfs-") {
            "the sandbox --mount option"
        } else {
            continue;
        };
        let name = String::from_utf8_lossy(name);
        return Err(refused(&format!("{name} is owned by {owner}")));
    }
    let line = vm.kernel_command_line().map_err(error_with_cause)?;
    let shares = vm
        .filesystem
        .as_ref()
        .map_or(0, |plan| export_tokens(plan).len());
    if line.len() + shares + 1 > COMMAND_LINE_BUDGET {
        return Err(refused(
            "sandbox kernel command line exceeds its 1024-byte x86 budget; use shorter arguments \
             and kernel parameters",
        ));
    }
    RootLayout::parse(line.as_bytes()).map_err(|error| {
        refused(&format!(
            "sandbox kernel command line conflicts with the boot tokens of its root: {error}"
        ))
    })?;
    // Linux must read the tokens that OpenVMM appends, as it reads one after the line whose name
    // the user's part, which holds no `nvx_*` token, cannot spell.
    let appended = format!("{line} nvx_appended");
    if !matches!(
        cmdline::parameters(appended.as_bytes()).last(),
        Some(Parameter {
            name: b"nvx_appended",
            value: None
        })
    ) {
        return Err(refused(
            "sandbox kernel command line hides the tokens that OpenVMM appends to it",
        ));
    }
    Ok(())
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
