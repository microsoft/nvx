//! The lifecycle of a managed sandbox, a port of the former
//! `scripts/nvx_tools/sandbox_lifecycle.py`, which it replaced (microsoft/nvx#359), on the
//! [`store`](crate::host::store), [`supervisor`], and [`session`](crate::host::session) that
//! every host of a sandbox shares.
//!
//! A sandbox lives in its state directory, which is the directory of its store:
//!
//! ```text
//! <state>/
//!   config.json          the launch that provision records, in format 7, with the export plan of
//!                        its shares or `null` without shares, the bindings of its distro layer
//!                        and of its scratch image, and its network
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
//! [`provision`], [`start`], [`stop`], and [`deprovision`] hold the [`LifecycleLock`] for their
//! whole transition, and [`exec_workload`] only while it finds the running VM, so a long workload
//! cannot delay a stop. `config.json` also records the binding of the distro layer that
//! `provision` verified, and that of the scratch image, which [`start`] checks from metadata
//! alone, and the network as its typed policy, which [`start`] checks again, in place of the
//! network options that `sandbox_lifecycle.py` recorded: it refuses to start a sandbox that
//! `nvx.py` or an earlier `nvx-rs` provisioned, which has no bindings or no typed network. It
//! records shares as the export plan that `provision` made, whose host objects [`start`] checks
//! again. [`start`] exports the shares, and once the guest agent is ready sends it the plan's
//! mapping table, which it mounts in order before any workload runs; a mapping that fails ends the
//! start, after the guest shuts down as a stop has it do.
//!
//! Every transition but provision finds the sandbox's VM with [`supervisor::reattach`], as an
//! orchestrator does after it restarts: it ends a start that was interrupted, holds a recorded
//! OpenVMM that still runs through its verified identity, and clears the runtime record of one
//! that exited without a stop, which the first transition to find it reports, or which a start
//! replaces. A VM that another runtime identity than the artifacts' started, as after the
//! artifacts were replaced, is never driven, since its guest may speak other contracts: [`stop`]
//! ends it by identity-verified forced termination, without a control session, and the other
//! transitions refuse it. [`start`] launches OpenVMM with [`supervisor::launch`], under the
//! runtime identity that the install metadata of the artifacts names, and [`stop`] asks the guest
//! of a VM of that identity to shut down, waits for OpenVMM to exit, and reports a VM that does
//! not, without ending it otherwise.
//!
//! [`start`] hands OpenVMM, through a [`PreparedLaunch`] of the checked files, the objects that
//! the checks found, rather than paths that something could replace meanwhile, and claims the
//! scratch image for this sandbox while OpenVMM runs. It holds the checked files until the guest
//! is ready, or until it has ended an OpenVMM whose start failed.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{Duration, Instant};

use super::lock::{LifecycleLock, OsErrorClass, os_error_class};
use super::state::{
    CONFIG_FORMATS, RecordedDistro, RecordedScratch, config_int, config_text, deserialize_binding,
    deserialize_launch, deserialize_network, deserialize_scratch_binding, hex, member,
    random_bytes, read_json, serialize_launch, temporary_path, write_json,
};
use super::{Artifacts, error_with_cause};
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL};
use crate::host::cli::session::{EXIT_CATEGORIES, ExecRequest, ExecResult, Session};
use crate::host::cli::{Exit, Result, check_interrupt, output};
use crate::host::python::builtins::{Int, repr};
use crate::host::python::dumps::{self, Value, dict};
use crate::host::python::fs::{self, os_error_message};
use crate::host::python::json::{Document, Node};
use crate::host::python::path::PurePath;
use crate::host::sandbox::distro::DISTRO_POLICY_REVISION;
use crate::host::sandbox::scratch::CLAIM_RELEASE_WAIT;
use crate::host::sandbox::{PreparedLaunch, SandboxLaunch, require_file};
use crate::host::store::{
    IdentityPart, LockedStore, OUTCOME_NAME, RUNTIME_NAME, Recovery, SandboxStore,
};
use crate::host::supervisor::{self, Launched, Reattached, StopOutcome, Vmm, VmmCommand};
use crate::protocol::{CAPABILITY_LEN, GuestFeatures};
use crate::{ErrorCode, ErrorKind};

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
    pub(crate) memory_mib: &'a Int,
    /// `--cmdline`.
    pub(crate) cmdline: &'a str,
}

/// Returns the error for an `OSError` that an operation on `path` raised.
fn os_error(error: &std::io::Error, path: &PurePath) -> Exit {
    Exit::error(ErrorCode::Io, os_error_message(error, Some(path.as_str())))
}

/// Removes `path` unless it is missing, as `Path.unlink(missing_ok=True)` does.
fn unlink_missing_ok(path: &PurePath) -> Result {
    match std::fs::remove_file(path.as_path()) {
        Err(error) if os_error_class(&error) != OsErrorClass::NotFound => {
            Err(os_error(&error, path))
        }
        _ => Ok(()),
    }
}

/// Returns a builder of directories of `mode`, which Windows ignores, as `os.mkdir` does.
fn dir_builder(mode: u32) -> std::fs::DirBuilder {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, mode);
    #[cfg(not(unix))]
    let _ = mode;
    builder
}

/// Creates `path` and its missing parents, as `Path.mkdir(mode, parents=True, exist_ok=True)`
/// does: parents with the default mode, `path` with `mode`, and no error if `path` is a
/// directory already.
fn make_directories(path: &PurePath, mode: u32) -> Result {
    match dir_builder(mode).create(path.as_path()) {
        Ok(()) => Ok(()),
        Err(error) if os_error_class(&error) == OsErrorClass::NotFound => {
            match path.parents().next() {
                Some(parent) if parent != *path => {
                    make_directories(&parent, 0o777)?;
                    make_directories_once(path, mode)
                }
                _ => Err(os_error(&error, path)),
            }
        }
        Err(_) if fs::is_dir(path.as_path()) => Ok(()),
        Err(error) => Err(os_error(&error, path)),
    }
}

/// Creates `path` once its parent exists, as `self.mkdir(mode, parents=False, exist_ok=True)`
/// does.
fn make_directories_once(path: &PurePath, mode: u32) -> Result {
    match dir_builder(mode).create(path.as_path()) {
        Ok(()) => Ok(()),
        Err(_) if fs::is_dir(path.as_path()) => Ok(()),
        Err(error) => Err(os_error(&error, path)),
    }
}

/// Creates the state directory at `path`, as provision does. An existing directory stays as it is,
/// and the store then refuses one that is not private to the current user; any other existing
/// path fails as `Path.mkdir(exist_ok=True)` fails for it. A new directory is created as the
/// store creates it, private to the current user, below parents that are created as needed, even
/// when an existing one goes meanwhile: nothing else creates the directory itself.
pub(crate) fn create_state_directory(path: &PurePath) -> Result {
    if fs::is_dir(path.as_path()) {
        return Ok(());
    }
    if fs::lexists(path.as_path()) {
        return Err(os_error(&already_exists(), path));
    }
    if let Some(parent) = path.parents().next().filter(|parent| parent != path) {
        make_directories(&parent, 0o777)?;
    }
    SandboxStore::create(path.as_path())
        .map(drop)
        .map_err(error_with_cause)
}

/// Returns the error with which `os.mkdir` fails for a path that exists.
fn already_exists() -> std::io::Error {
    #[cfg(windows)]
    let error = std::io::Error::from_raw_os_error(183);
    #[cfg(unix)]
    let error = std::io::Error::from_raw_os_error(nix::errno::Errno::EEXIST as i32);
    #[cfg(not(any(windows, unix)))]
    let error = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    error
}

/// Resolves the state directory at `path`, and creates it with `create`, as
/// `_prepare_state_directory` does. It must be a directory and not a symbolic link.
pub(crate) fn prepare_state_directory(path: &str, create: bool) -> Result<PurePath> {
    let resolved = fs::resolve(&PurePath::native(path)).map_err(Exit::from_os_error)?;
    if create {
        create_state_directory(&resolved)?;
    }
    if fs::is_symlink(resolved.as_path()) || !fs::is_dir(resolved.as_path()) {
        return Err(Exit::error(
            ErrorCode::SandboxNotProvisioned,
            format!("sandbox state path is not a plain directory: {resolved}"),
        ));
    }
    Ok(resolved)
}

/// Returns the destination of an outcome report, as `_outcome_destination` does: in a plain
/// directory, with a file name, and new.
fn outcome_destination(path: &str) -> Result<PurePath> {
    let path = PurePath::native(path);
    let candidate = if path.is_absolute() {
        path
    } else {
        fs::cwd().map_err(Exit::from_os_error)?.join(path.as_str())
    };
    let parent = candidate
        .parents()
        .next()
        .unwrap_or_else(|| candidate.clone());
    if fs::is_symlink(parent.as_path()) || !fs::is_dir(parent.as_path()) {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            format!("outcome report parent is not a plain directory: {parent}"),
        ));
    }
    if candidate.name().is_empty() {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "outcome report path has no filename",
        ));
    }
    let resolved = fs::resolve(&parent)
        .map_err(Exit::from_os_error)?
        .join(candidate.name());
    if fs::lexists(resolved.as_path()) {
        return Err(Exit::error(
            ErrorCode::OutcomeReportExists,
            format!("outcome report already exists: {resolved}"),
        ));
    }
    Ok(resolved)
}

/// Checks the destination of an outcome report before a workload runs, as
/// `validate_outcome_destination` does.
pub(crate) fn validate_outcome_destination(path: &str) -> Result {
    outcome_destination(path).map(drop)
}

/// Writes `value` to a new file at `path`, as `_write_new_json` does: through a temporary file of
/// mode 0600, synced, then linked to its name, which must not exist, with `\n` line ends on every
/// host.
fn write_new_json(path: &str, value: &Value) -> Result {
    let resolved = outcome_destination(path)?;
    let temporary = temporary_path(&resolved)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = match options.open(temporary.as_path()) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Exit::error(
                ErrorCode::Io,
                "failed to reserve an outcome report staging file",
            ));
        }
        Err(error) => return Err(os_error(&error, &temporary)),
    };
    let written = (|| -> Result {
        let text = format!("{}\n", dumps::dumps(value));
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(Exit::from_os_error)?;
        match std::fs::hard_link(temporary.as_path(), resolved.as_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(Exit::error(
                ErrorCode::OutcomeReportExists,
                format!("outcome report already exists: {resolved}"),
            )),
            Err(_) => Err(Exit::error(
                ErrorCode::Io,
                format!("failed to publish outcome report: {resolved}"),
            )),
        }
    })();
    drop(file);
    let _ = std::fs::remove_file(temporary.as_path());
    written
}

/// The sections of an OpenVMM outcome report that a reader requires.
const OUTCOME_SECTIONS: [&str; 3] = ["outcome", "network_policy", "teardown"];

/// Reads an OpenVMM outcome report, as `_read_openvmm_outcome` does: of schema version 1, with
/// `outcome`, `network_policy`, and `teardown` objects.
fn read_openvmm_outcome(path: &PurePath) -> Result<Document> {
    let document = read_json(
        path,
        "OpenVMM outcome report",
        "schema_version",
        &[OUTCOME_SCHEMA_VERSION],
    )?;
    for name in OUTCOME_SECTIONS {
        let section = member(&document, document.root(), name);
        if !section.is_some_and(|section| matches!(document.node(section), Node::Object(_))) {
            return Err(Exit::error(
                ErrorCode::Io,
                format!("OpenVMM outcome report has an invalid {name} section: {path}"),
            ));
        }
    }
    Ok(document)
}

/// Writes the outcome report of a managed workload, as `write_exec_outcome` does: its category
/// and status, with a random operation ID, and nothing of its output.
pub(crate) fn write_exec_outcome(path: &str, result: &ExecResult) -> Result {
    if !EXIT_CATEGORIES.contains(&result.category) {
        return Err(Exit::error(
            ErrorCode::Protocol,
            "managed workload returned an unsupported outcome category",
        ));
    }
    let operation_id = hex(&random_bytes::<16>()?);
    write_new_json(
        path,
        &dict([
            ("schema_version", Value::from(OUTCOME_SCHEMA_VERSION)),
            ("operation_id", Value::from(operation_id)),
            (
                "outcome",
                dict([
                    ("operation", Value::from("exec")),
                    ("category", Value::from(result.category)),
                    ("status_code", Value::from(i64::from(result.returncode))),
                ]),
            ),
        ]),
    )
}

/// Records a launch and its distro layer, at the path at which it was verified, with the layer's
/// binding, in a new state directory, as `provision` does, with the binding of its scratch image,
/// at the path that it records for it.
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
    if fs::exists(config_path.as_path()) || fs::exists(runtime_path.as_path()) {
        return Err(Exit::error(
            ErrorCode::SandboxProvisioned,
            "sandbox is already provisioned",
        ));
    }
    launch.validated()?;
    let scratch = RecordedScratch::new(launch)?;
    write_json(
        &config_path,
        &serialize_launch(launch, distro, &scratch, config)?,
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
    if locked.journal().map_err(error_with_cause)?.is_none()
        && locked.runtime().map_err(error_with_cause)?.is_none()
    {
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
    state_dir: &PurePath,
    locked: &mut LockedStore,
    artifacts: &Artifacts<'_>,
) -> Result<Running> {
    let found = find_vm(locked, artifacts)?;
    running(state_dir, locked, found)
}

/// Returns the running VM that `found` describes in the locked store of `state_dir`, as exec and
/// stop require it, with its control capability.
fn running(state_dir: &PurePath, locked: &mut LockedStore, found: Found) -> Result<Running> {
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
fn require_config(state_dir: &PurePath) -> Result {
    require_file(
        state_dir.join(CONFIG_NAME).as_path(),
        "sandbox configuration",
    )
    .map_err(|error| Exit::error(ErrorCode::SandboxNotProvisioned, error.message()))
}

/// Starts a provisioned sandbox, as `start` does.
pub(crate) fn start(state_path: &str, artifacts: &Artifacts<'_>, timeout: f64) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    start_locked(&state_dir, lock.locked(), artifacts, timeout)
}

/// The control features that the layered sandbox root must provide for the requests of
/// `nvx-rs sandbox`.
pub(crate) const REQUIRED_FEATURES: GuestFeatures = GuestFeatures::LAYERED_SANDBOX;

/// Starts the sandbox of a locked state directory, as `_start` does, through the launch journal
/// of its store.
fn start_locked(
    state_dir: &PurePath,
    locked: &mut LockedStore,
    artifacts: &Artifacts<'_>,
    timeout: f64,
) -> Result {
    let config_path = state_dir.join(CONFIG_NAME);
    require_config(state_dir)?;
    let config = read_json(
        &config_path,
        "sandbox configuration",
        "format",
        &CONFIG_FORMATS,
    )?;
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
    let launch = deserialize_launch(&config)?;
    // The checks read no content of the distro layer, its manifest, or the scratch image, and
    // never verify them again. The prepared launch holds the files that they return, and hands
    // OpenVMM those objects: on Linux, OpenVMM inherits their descriptors; on Windows, it opens
    // them by their final paths, which the files keep anything from replacing before OpenVMM,
    // whose opens share them, has opened them. The scratch image's check also claims it for this
    // sandbox, until OpenVMM exits.
    let binding = deserialize_binding(&config)?;
    let scratch_binding = deserialize_scratch_binding(&config)?;
    let distro = binding
        .check(launch.distro(), DISTRO_POLICY_REVISION)
        .map_err(binding_error)?;
    // Reading the network checks its policy again, as provision did, before anything launches.
    let launch = launch.with_network(deserialize_network(&config)?);
    let scratch = scratch_binding
        .check(launch.scratch(), Instant::now() + CLAIM_RELEASE_WAIT)
        .map_err(binding_error)?;
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
    let hypervisor = config_text(&config, "hypervisor")?;
    let memory_mib = config_int(&config, "memory_mib")?;
    let prepared = PreparedLaunch::new(
        plan,
        distro,
        Some(scratch),
        &config_text(&config, "cmdline")?,
    )?;
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
    head.push(hypervisor);
    head.push("--memory".to_owned());
    head.push(format!("{memory_mib}M"));
    head.extend(["--kernel".to_owned(), kernel, "--initrd".to_owned(), initrd]);
    head.push("--cmdline".to_owned());
    head.push(prepared.kernel_command_line().to_owned());
    head.extend(["--virtio-console", "none", "--microvm-control-console"].map(str::to_owned));
    let tail = vec![
        "--microvm-control-auth-stdin".to_owned(),
        "--microvm-report".to_owned(),
        outcome_path.as_str().to_owned(),
    ];
    let directory = std::env::current_dir().map_err(Exit::from_os_error)?;
    let mut command = Vec::new();
    let mut starting = supervisor::launch(locked, &runtime, &capability, |endpoint| {
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
    let pid = starting.process().reference().pid;
    let maps = prepared.plan().mapping_table();
    // Whether the guest agent acknowledged a request to shut down after a mapping failed.
    let mut stopping = false;
    let ready =
        check_ready(starting.endpoint(), pid, &capability, timeout).and_then(|mut session| {
            // Workloads run only once the guest agent has mapped every share in order, so a
            // mapping that fails ends the start. A sandbox without shares maps nothing.
            if maps.is_empty() {
                return Ok(());
            }
            session.map_host_paths(&maps, timeout).inspect_err(|error| {
                // The agent then refuses workloads, so the guest shuts down as a stop has it do:
                // it unmounts what it mapped, the export, the overlay, and the scratch disk before
                // the VM powers off. A failed transfer closed the session, so a guest that stopped
                // answering is terminated at once.
                if !matches!(error, Exit::Interrupted) {
                    stopping = session.stop(timeout).is_ok();
                }
            })
        });
    let Err(error) = ready else {
        // The guest is ready, so OpenVMM holds the images: the checked files can go, which
        // releases their guards, while OpenVMM keeps the scratch image's claim.
        drop(prepared);
        return starting.commit().map(drop).map_err(error_with_cause);
    };
    // OpenVMM closes the control endpoint as it tears the VM down, before it publishes its
    // outcome report and exits, so only a closed endpoint, or a guest that shuts down, warrants
    // waiting for it.
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

/// Waits until the guest of a VM that starts answers on `endpoint`, which OpenVMM process `pid`
/// serves, checks that it provides the features of [`REQUIRED_FEATURES`], and returns the session,
/// on which the caller sends the guest what it needs before the launch commits.
fn check_ready(
    endpoint: &str,
    pid: u32,
    capability: &[u8; CAPABILITY_LEN],
    timeout: f64,
) -> Result<Session> {
    let mut session = Session::connect(endpoint, pid, capability, timeout)?;
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
                let arguments: Vec<String> =
                    command.iter().map(|argument| repr(argument)).collect();
                return Err(Exit::error(
                    ErrorCode::TimedOut,
                    format!(
                        "Command '[{}]' timed out after 5 seconds",
                        arguments.join(", ")
                    ),
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
/// own status, as `_startup_exit_message` does.
fn startup_exit_message(outcome_path: &PurePath, status: i64) -> Result<String> {
    let report = match read_openvmm_outcome(outcome_path) {
        Ok(report) => Some(report),
        // A `ValueError` of `json.loads` escapes the `except ScriptError`.
        Err(exit)
            if exit
                .message()
                .is_some_and(|message| message.starts_with("Exceeds the limit")) =>
        {
            return Err(exit);
        }
        Err(_) => None,
    };
    if let Some(report) = &report
        && let Some(outcome) = member(report, report.root(), "outcome")
        && let Some(category) = member(report, outcome, "category")
        && let Some(status_code) = member(report, outcome, "status_code")
        && let Node::String(category) = report.node(category)
        && let Node::Integer(status_code) = report.node(status_code)
    {
        let status_code =
            Int::parse(status_code).map_err(|_| Exit::error(ErrorCode::Io, "invalid status"))?;
        return Ok(format!(
            "OpenVMM exited during startup: {} status {status_code}",
            category.render()
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

/// Stops a running sandbox and returns OpenVMM's outcome report, as `stop` does, or `None` for a
/// VM that another runtime identity than the artifacts' started.
///
/// The supervisor's stop runs one graceful step, which asks the guest to shut the VM down and
/// waits up to `timeout` seconds for OpenVMM to exit, so that Ctrl-C ends the wait. A VM whose
/// OpenVMM is still running then is reported, and keeps its runtime record: the stop never takes
/// the forced step.
///
/// A VM of another runtime identity may speak other contracts, so the stop asks it nothing: it
/// ends the VM as [`terminate_mismatched`] does.
pub(crate) fn stop(
    state_path: &str,
    artifacts: &Artifacts<'_>,
    timeout: f64,
) -> Result<Option<Document>> {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    let locked = lock.locked();
    let Running { vmm, capability } = match find_vm(locked, artifacts)? {
        Found::Mismatched(vmm, differences) => {
            terminate_mismatched(locked, &vmm, &differences, timeout)?;
            return Ok(None);
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
    read_openvmm_outcome(&state_dir.join(OUTCOME_NAME)).map(Some)
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
fn terminate_within(
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
    match find_vm(lock.locked(), artifacts)? {
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

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    /// An OpenVMM of another runtime identity whose termination a test scripts, without a process.
    struct Scripted {
        /// Whether OpenVMM has exited.
        exited: Cell<bool>,
        /// Whether a termination ends OpenVMM within its bound, which one in uninterruptible I/O
        /// outlives.
        dies_when_terminated: bool,
        /// Whether the runtime record of its launch remains.
        recorded: Cell<bool>,
        /// The bounds of its terminations, in order.
        bounds: RefCell<Vec<Duration>>,
    }

    impl Scripted {
        /// Returns an OpenVMM that runs, which a termination ends if `dies_when_terminated`.
        fn running(dies_when_terminated: bool) -> Self {
            Self {
                exited: Cell::new(false),
                dies_when_terminated,
                recorded: Cell::new(true),
                bounds: RefCell::new(Vec::new()),
            }
        }

        /// Terminates OpenVMM within `bound`, as [`Vmm::terminate`] does: one that has exited is
        /// not signaled, and the runtime record goes once it has exited.
        fn terminate(&self, bound: Duration) -> Result<StopOutcome> {
            self.bounds.borrow_mut().push(bound);
            let outcome = if self.exited.get() {
                StopOutcome::Graceful
            } else if self.dies_when_terminated {
                self.exited.set(true);
                StopOutcome::Forced
            } else {
                StopOutcome::TimedOut
            };
            if self.exited.get() {
                self.recorded.set(false);
            }
            Ok(outcome)
        }

        /// Ends OpenVMM, as a stop with an hour's timeout does, and ends the stop's wait for its
        /// exit with `waited`: OpenVMM exits during the wait if that is `Ok(true)`.
        fn stop(&self, waited: Result<bool>) -> Result<StopOutcome> {
            terminate_within(
                Deadline::after(3600.0),
                |bound| self.terminate(bound),
                |_| {
                    if matches!(waited, Ok(true)) {
                        self.exited.set(true);
                    }
                    waited
                },
            )
        }

        /// Returns the bounds of its terminations.
        fn bounds(&self) -> Vec<Duration> {
            self.bounds.borrow().clone()
        }
    }

    #[test]
    fn ctrl_c_ends_the_wait_for_a_terminated_openvmm_that_has_not_exited() {
        // OpenVMM outlives its termination, so the termination waits one retry interval at most,
        // whatever the timeout, and the stop's own wait, which Ctrl-C ends, waits for the rest.
        let openvmm = Scripted::running(false);
        let stopped = openvmm.stop(Err(Exit::Interrupted));
        assert!(matches!(stopped, Err(Exit::Interrupted)), "{stopped:?}");
        let bounds = openvmm.bounds();
        assert_eq!(bounds.len(), 1, "{bounds:?}");
        assert!(bounds[0] <= RETRY_INTERVAL, "{bounds:?}");
        // Its exit was not seen, so its runtime record remains for the next stop.
        assert!(openvmm.recorded.get());
    }

    #[test]
    fn a_terminated_openvmm_that_exits_during_the_wait_was_forced() {
        // Once it has exited, a second termination finds it exited, signals nothing, and clears
        // its runtime record.
        let openvmm = Scripted::running(false);
        let stopped = openvmm.stop(Ok(true));
        assert!(matches!(stopped, Ok(StopOutcome::Forced)), "{stopped:?}");
        assert_eq!(openvmm.bounds().len(), 2);
        assert!(!openvmm.recorded.get());

        // One that still runs at the deadline keeps its record.
        let survivor = Scripted::running(false);
        let stopped = survivor.stop(Ok(false));
        assert!(matches!(stopped, Ok(StopOutcome::TimedOut)), "{stopped:?}");
        assert_eq!(survivor.bounds().len(), 1);
        assert!(survivor.recorded.get());
    }

    #[test]
    fn an_openvmm_that_its_termination_ends_or_that_exited_is_not_waited_for() {
        // The stop's wait, which would fail here, does not run.
        let openvmm = Scripted::running(true);
        let stopped = openvmm.stop(Err(Exit::Interrupted));
        assert!(matches!(stopped, Ok(StopOutcome::Forced)), "{stopped:?}");
        assert_eq!(openvmm.bounds().len(), 1);
        assert!(!openvmm.recorded.get());

        let exited = Scripted::running(true);
        exited.exited.set(true);
        let stopped = exited.stop(Err(Exit::Interrupted));
        assert!(matches!(stopped, Ok(StopOutcome::Graceful)), "{stopped:?}");
        assert_eq!(exited.bounds().len(), 1);
        assert!(!exited.recorded.get());
    }
}
