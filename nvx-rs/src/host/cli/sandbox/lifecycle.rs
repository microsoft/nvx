//! The lifecycle of a managed sandbox, a port of the former
//! `scripts/nvx_tools/sandbox_lifecycle.py`, which it replaced (microsoft/nvx#359).
//!
//! A sandbox lives in its state directory:
//!
//! ```text
//! <state>/
//!   config.json          the launch that provision records, in formats 1 to 6
//!   lifecycle.lock       the lock that serializes provision, start, stop, and deprovision
//!   runtime.json         format 1: the pid, start_time, and control_endpoint of OpenVMM
//!   control.capability   the 32-byte capability of the running OpenVMM
//!   control.sock         the control endpoint of the running OpenVMM, on POSIX
//!   openvmm.log          OpenVMM's output, appended by each start
//!   outcome.json         OpenVMM's outcome report of the latest start
//! ```
//!
//! [`provision`], [`start`], [`stop`], and [`deprovision`] hold the [`LifecycleLock`] for their
//! whole transition; [`exec_workload`] does not, so a long workload cannot delay a stop. Every
//! state file is written as `sandbox_lifecycle.py` wrote it, and a state directory that either
//! implementation wrote serves the other, so an older `nvx.py` and `nvx-rs` share sandboxes.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::time::Duration;

use super::Artifacts;
use super::identity::{process_running, process_start_time};
use super::lock::{
    LifecycleLock, OsErrorClass, directory_not_empty, directory_status, entries, lingers,
    os_error_class,
};
use super::state::{
    CONFIG_FORMATS, STATE_FORMAT, config_int, config_text, deserialize_launch, hex, member,
    random_bytes, read_json, serialize_launch, temporary_path, uuid4_hex, write_json,
};
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL, sleep};
use crate::host::cli::session::{EXIT_CATEGORIES, ExecRequest, ExecResult, Session};
use crate::host::cli::{Exit, Result};
use crate::host::process::{self, Launched};
use crate::host::python::builtins::{Int, repr};
use crate::host::python::dumps::{self, Value, dict};
use crate::host::python::fs::{self, os_error_message};
use crate::host::python::json::{Document, Node};
use crate::host::python::path::PurePath;
use crate::host::python::value::py_str;
use crate::host::sandbox::{SandboxLaunch, require_file};
use crate::protocol::{CAPABILITY_LEN, GuestFeatures};

/// The launch configuration.
pub(crate) const CONFIG_NAME: &str = "config.json";
/// The record of the running OpenVMM process.
pub(crate) const RUNTIME_NAME: &str = "runtime.json";
/// The control capability of the running OpenVMM process.
pub(crate) const CAPABILITY_NAME: &str = "control.capability";
/// OpenVMM's output.
pub(crate) const LOG_NAME: &str = "openvmm.log";
/// The control endpoint on POSIX.
pub(crate) const CONTROL_SOCKET_NAME: &str = "control.sock";
/// OpenVMM's outcome report.
pub(crate) const OUTCOME_NAME: &str = "outcome.json";
/// The lifecycle lock.
pub(crate) const LOCK_NAME: &str = "lifecycle.lock";
/// The files that exist only from the start that launches an OpenVMM process until the stop or
/// failed start that ends it, whether or not the runtime record names it.
pub(crate) const RUNTIME_FILE_NAMES: [&str; 3] =
    [RUNTIME_NAME, CAPABILITY_NAME, CONTROL_SOCKET_NAME];
/// How often a caller retries a lifecycle lock that another caller holds, and the removal of a
/// state directory that a removed lock file still keeps.
pub(crate) const LOCK_RETRY_INTERVAL: Duration = RETRY_INTERVAL;
/// What deprovision writes into the lock file before it removes the file, so that a waiter that
/// then takes the lock of the removed file knows that the directory goes away.
pub(crate) const LOCK_TOMBSTONE: &[u8] = b"deprovisioned\n";
/// The version of outcome reports.
pub(crate) const OUTCOME_SCHEMA_VERSION: u64 = 1;

/// The options of a launch, besides the launch itself, that `provision` records.
pub(crate) struct ProvisionConfig<'a> {
    /// `--hypervisor`, resolved.
    pub(crate) hypervisor: &'a str,
    /// `--memory-mib`.
    pub(crate) memory_mib: &'a Int,
    /// `--net`.
    pub(crate) net: Option<&'a str>,
    /// `--network-profile`.
    pub(crate) network_profile: Option<&'a str>,
    /// `--network-egress`.
    pub(crate) network_egress: Option<&'a str>,
    /// `--network-ingress`.
    pub(crate) network_ingress: Option<&'a str>,
    /// The egress allow rules.
    pub(crate) network_egress_allow: &'a [String],
    /// The egress deny rules.
    pub(crate) network_egress_deny: &'a [String],
    /// `--host-loopback`.
    pub(crate) host_loopback: Option<&'a str>,
    /// `--network-proxy`.
    pub(crate) network_proxy: Option<&'a str>,
    /// The `--host-loopback-forward` options.
    pub(crate) host_loopback_forward: &'a [String],
    /// `--cmdline`.
    pub(crate) cmdline: &'a str,
}

/// Returns the error for an `OSError` that an operation on `path` raised.
fn os_error(error: &std::io::Error, path: &PurePath) -> Exit {
    Exit::error(os_error_message(error, Some(path.as_str())))
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

/// Gives a directory mode 0700, as `os.chmod(path, 0o700)` does: on Windows, that only clears the
/// read-only attribute.
fn make_private(path: &PurePath) -> Result {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path.as_path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|error| os_error(&error, path))
    }
    #[cfg(not(unix))]
    {
        let metadata = std::fs::metadata(path.as_path()).map_err(|error| os_error(&error, path))?;
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            std::fs::set_permissions(path.as_path(), permissions)
                .map_err(|error| os_error(&error, path))?;
        }
        Ok(())
    }
}

/// Resolves the state directory at `path`, and creates it with `create`, as
/// `_prepare_state_directory` does. It must be a directory and not a symbolic link.
pub(crate) fn prepare_state_directory(path: &str, create: bool) -> Result<PurePath> {
    let resolved = fs::resolve(&PurePath::native(path))
        .map_err(|error| Exit::error(os_error_message(&error, None)))?;
    if create {
        make_directories(&resolved, 0o700)?;
        make_private(&resolved)?;
    }
    if fs::is_symlink(resolved.as_path()) || !fs::is_dir(resolved.as_path()) {
        return Err(Exit::error(format!(
            "sandbox state path is not a plain directory: {resolved}"
        )));
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
        fs::cwd()
            .map_err(|error| Exit::error(os_error_message(&error, None)))?
            .join(path.as_str())
    };
    let parent = candidate
        .parents()
        .next()
        .unwrap_or_else(|| candidate.clone());
    if fs::is_symlink(parent.as_path()) || !fs::is_dir(parent.as_path()) {
        return Err(Exit::error(format!(
            "outcome report parent is not a plain directory: {parent}"
        )));
    }
    if candidate.name().is_empty() {
        return Err(Exit::error("outcome report path has no filename"));
    }
    let resolved = fs::resolve(&parent)
        .map_err(|error| Exit::error(os_error_message(&error, None)))?
        .join(candidate.name());
    if fs::lexists(resolved.as_path()) {
        return Err(Exit::error(format!(
            "outcome report already exists: {resolved}"
        )));
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
                "failed to reserve an outcome report staging file",
            ));
        }
        Err(error) => return Err(os_error(&error, &temporary)),
    };
    let written = (|| -> Result {
        let text = format!("{}\n", dumps::dumps(value));
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| Exit::error(os_error_message(&error, None)))?;
        match std::fs::hard_link(temporary.as_path(), resolved.as_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(Exit::error(
                format!("outcome report already exists: {resolved}"),
            )),
            Err(_) => Err(Exit::error(format!(
                "failed to publish outcome report: {resolved}"
            ))),
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
            return Err(Exit::error(format!(
                "OpenVMM outcome report has an invalid {name} section: {path}"
            )));
        }
    }
    Ok(document)
}

/// Writes the outcome report of a managed workload, as `write_exec_outcome` does: its category
/// and status, with a random operation ID, and nothing of its output.
pub(crate) fn write_exec_outcome(path: &str, result: &ExecResult) -> Result {
    if !EXIT_CATEGORIES.contains(&result.category) {
        return Err(Exit::error(
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

/// Records a launch in a new state directory, as `provision` does.
pub(crate) fn provision(
    state_path: &str,
    launch: &SandboxLaunch,
    config: &ProvisionConfig<'_>,
    timeout: f64,
) -> Result {
    let state_dir = prepare_state_directory(state_path, true)?;
    let config_path = state_dir.join(CONFIG_NAME);
    let runtime_path = state_dir.join(RUNTIME_NAME);
    let _lock = LifecycleLock::acquire(&state_dir, timeout, true)?;
    if fs::exists(config_path.as_path()) || fs::exists(runtime_path.as_path()) {
        return Err(Exit::error("sandbox is already provisioned"));
    }
    launch.validated()?;
    write_json(&config_path, &serialize_launch(launch, config)?)
}

/// The pid and start time that a runtime record names, as `_runtime_process` reads them.
struct RuntimeProcess {
    /// The process ID, of any size.
    pid: Int,
    /// The start time, if the record has one.
    start_time: Option<Int>,
}

/// Reads the pid and start time of a runtime record, as `_runtime_process` does.
fn runtime_process(document: &Document) -> Result<RuntimeProcess> {
    let invalid_pid = || Exit::error("sandbox runtime state has an invalid process ID");
    let root = document.root();
    let pid = member(document, root, "pid").ok_or_else(invalid_pid)?;
    let pid = crate::host::python::value::py_int(document, pid).map_err(|error| match error {
        crate::host::python::value::IntError::Invalid => invalid_pid(),
        crate::host::python::value::IntError::Overflow => {
            Exit::error("cannot convert float infinity to integer")
        }
    })?;
    let start_time = match member(document, root, "start_time").map(|id| document.node(id)) {
        None | Some(Node::Null) => None,
        Some(Node::Integer(literal)) => {
            let value = Int::parse(literal).map_err(|_| invalid_start_time())?;
            if value.is_negative() {
                return Err(invalid_start_time());
            }
            Some(value)
        }
        Some(_) => return Err(invalid_start_time()),
    };
    Ok(RuntimeProcess { pid, start_time })
}

/// Returns the error of a runtime record with an invalid start time.
fn invalid_start_time() -> Exit {
    Exit::error("sandbox runtime state has an invalid process start time")
}

/// A running sandbox, as `_load_running` finds it.
struct Running {
    /// The runtime record.
    runtime: Document,
    /// The control capability.
    capability: [u8; CAPABILITY_LEN],
}

/// Reads the runtime record and capability of a running sandbox, as `_load_running` does.
fn load_running(state_dir: &PurePath) -> Result<Running> {
    let runtime_path = state_dir.join(RUNTIME_NAME);
    if !fs::is_file(runtime_path.as_path()) {
        return Err(Exit::error("sandbox is not running"));
    }
    let runtime = read_json(
        &runtime_path,
        "sandbox runtime state",
        "format",
        &[STATE_FORMAT],
    )?;
    let process = runtime_process(&runtime)?;
    if !process_running(&process.pid, process.start_time.as_ref())? {
        return Err(Exit::error(
            "sandbox runtime state is stale because the OpenVMM process is not running",
        ));
    }
    let capability_path = state_dir.join(CAPABILITY_NAME);
    require_file(capability_path.as_path(), "sandbox control capability")?;
    let bytes = std::fs::read(capability_path.as_path())
        .map_err(|error| os_error(&error, &capability_path))?;
    let capability: [u8; CAPABILITY_LEN] = bytes
        .try_into()
        .ok()
        .filter(|capability| *capability != [0; CAPABILITY_LEN])
        .ok_or_else(|| Exit::error("sandbox control capability is invalid"))?;
    Ok(Running {
        runtime,
        capability,
    })
}

/// Returns the control endpoint of a runtime record, as `_endpoint` does.
fn endpoint(runtime: &Document) -> Result<String> {
    let value = member(runtime, runtime.root(), "control_endpoint")
        .ok_or_else(|| Exit::error("sandbox runtime state has no control endpoint"))?;
    py_str(runtime, value)
        .ok_or_else(|| Exit::error("sandbox runtime state has no control endpoint"))
}

/// Connects to the running sandbox of `running`.
fn connect(running: &Running, timeout: f64) -> Result<Session> {
    let endpoint = endpoint(&running.runtime)?;
    let process = runtime_process(&running.runtime)?;
    // A running process has an ID of 32 bits.
    let pid = process.pid.to_u32().unwrap_or_default();
    Session::connect(&endpoint, pid, &running.capability, timeout)
}

/// Starts a provisioned sandbox, as `start` does.
pub(crate) fn start(state_path: &str, artifacts: &Artifacts<'_>, timeout: f64) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let _lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    start_locked(&state_dir, artifacts, timeout)
}

/// The control features that the layered sandbox root must provide for the requests of
/// `nvx-rs sandbox`.
pub(crate) const REQUIRED_FEATURES: GuestFeatures = GuestFeatures::LAYERED_SANDBOX;

/// Starts the sandbox of a locked state directory, as `_start` does.
fn start_locked(state_dir: &PurePath, artifacts: &Artifacts<'_>, timeout: f64) -> Result {
    let config_path = state_dir.join(CONFIG_NAME);
    require_file(config_path.as_path(), "sandbox configuration")?;
    let config = read_json(
        &config_path,
        "sandbox configuration",
        "format",
        &CONFIG_FORMATS,
    )?;
    // Any runtime file belongs to an OpenVMM process that may still run, even without a runtime
    // record, as after a start that was killed. Without them, every runtime file that appears
    // while this start holds the lifecycle lock is its own.
    if RUNTIME_FILE_NAMES
        .iter()
        .any(|name| fs::lexists(state_dir.join(name).as_path()))
    {
        return Err(Exit::error(
            "sandbox is already running or has stale runtime state",
        ));
    }
    let outcome_path = state_dir.join(OUTCOME_NAME);
    unlink_missing_ok(&outcome_path)?;
    let launch = deserialize_launch(&config)?;
    let [executable, kernel, initrd] = artifacts.require()?;
    let capability = random_bytes::<CAPABILITY_LEN>()?;
    if capability == [0; CAPABILITY_LEN] {
        return Err(Exit::error(
            "the system returned an all-zero control capability",
        ));
    }
    let endpoint = if cfg!(windows) {
        format!("//./pipe/openvmm-microvm-{}", uuid4_hex()?)
    } else {
        state_dir
            .join(super::lifecycle::CONTROL_SOCKET_NAME)
            .as_str()
            .to_owned()
    };
    let mut command = vec![executable];
    command.extend(launch.openvmm_arguments()?);
    command.extend(
        [
            "--microvm-lifecycle",
            "managed",
            "--single-process",
            "--hypervisor",
        ]
        .map(str::to_owned),
    );
    command.push(config_text(&config, "hypervisor")?);
    command.push("--memory".to_owned());
    command.push(format!("{}M", config_int(&config, "memory_mib")?));
    command.extend(["--kernel".to_owned(), kernel, "--initrd".to_owned(), initrd]);
    command.push("--cmdline".to_owned());
    command.push(launch.kernel_command_line(&config_text(&config, "cmdline")?)?);
    command.extend(["--virtio-console", "none", "--microvm-control-console"].map(str::to_owned));
    command.push(format!("listen={endpoint}"));
    command.push("--microvm-control-auth-stdin".to_owned());
    command.push("--microvm-report".to_owned());
    command.push(outcome_path.as_str().to_owned());
    extend_recorded_network_arguments(&mut command, &config)?;

    let capability_path = state_dir.join(CAPABILITY_NAME);
    create_capability(&capability_path, &capability)?;
    let log_path = state_dir.join(LOG_NAME);
    let log = match OpenOptions::new()
        .append(true)
        .create(true)
        .open(log_path.as_path())
    {
        Ok(log) => log,
        Err(error) => {
            let _ = unlink_missing_ok(&capability_path);
            return Err(os_error(&error, &log_path));
        }
    };
    let mut launched: Option<Launched> = None;
    let started = launch_and_connect(
        &command,
        &capability,
        log,
        &endpoint,
        state_dir,
        timeout,
        &mut launched,
    );
    let Err(error) = started else {
        if let Some(launched) = launched {
            process::detach_child(launched);
        }
        return Ok(());
    };
    let mut status = None;
    let mut ended = launched.is_none();
    if let Some(launched) = launched.as_mut() {
        // OpenVMM closes the control endpoint as it tears the VM down, before it publishes its
        // outcome report and exits, so only a closed endpoint warrants waiting for it.
        let grace = if matches!(error, Exit::EndpointClosed) {
            timeout
        } else {
            0.0
        };
        match end_failed_start(launched, grace, &command) {
            Ok(exited) => {
                status = exited;
                ended = true;
            }
            Err(exit) => return Err(exit),
        }
    }
    if ended {
        // This start found none of these files and has held the lifecycle lock since, so they are
        // its own. The runtime record goes last, so that an interrupted cleanup leaves a record
        // that shows OpenVMM gone rather than runtime files that deprovision cannot vouch for.
        unlink_missing_ok(&state_dir.join(CONTROL_SOCKET_NAME))?;
        unlink_missing_ok(&capability_path)?;
        unlink_missing_ok(&state_dir.join(RUNTIME_NAME))?;
    }
    match status {
        Some(status) if !matches!(error, Exit::Interrupted) => {
            Err(Exit::error(startup_exit_message(&outcome_path, status)?))
        }
        _ => Err(error),
    }
}

/// Launches OpenVMM, records it, and waits until its guest is ready and provides the features of
/// [`REQUIRED_FEATURES`], leaving the launched process in `launched` for the caller to end if
/// that fails.
fn launch_and_connect(
    command: &[String],
    capability: &[u8; CAPABILITY_LEN],
    log: File,
    endpoint: &str,
    state_dir: &PurePath,
    timeout: f64,
    launched: &mut Option<Launched>,
) -> Result {
    let arguments: Vec<std::ffi::OsString> = command[1..].iter().map(Into::into).collect();
    let directory =
        std::env::current_dir().map_err(|error| Exit::error(os_error_message(&error, None)))?;
    let child = process::spawn(
        std::path::Path::new(&command[0]),
        false,
        &arguments,
        capability,
        log,
        &directory,
    )
    .map_err(|error| Exit::error(super::spawn_error(&error, &command[0])))?;
    let child = launched.insert(child);
    // Until this process reaps the child or closes its handle, no other process can reuse its ID,
    // so this identifies OpenVMM itself.
    let pid = child.id();
    let start_time = process_start_time(&Int::from(pid))
        .map_err(|error| Exit::error(format!("cannot identify the OpenVMM process: {error}")))?;
    let Some(start_time) = start_time else {
        let exited = child
            .wait_timeout(Duration::ZERO)
            .map_err(|error| Exit::error(os_error_message(&error, None)))?;
        return Err(Exit::error(if exited.is_some() {
            "OpenVMM exited during startup"
        } else {
            "cannot identify the OpenVMM process"
        }));
    };
    write_json(
        &state_dir.join(RUNTIME_NAME),
        &dict([
            ("format", Value::from(STATE_FORMAT)),
            ("pid", Value::from(pid)),
            ("start_time", Value::from(start_time)),
            ("control_endpoint", Value::from(endpoint)),
        ]),
    )?;
    let mut session = Session::connect(endpoint, pid, capability, timeout)?;
    session.ping(timeout)?;
    let features = session.features(timeout)?;
    let missing = features.missing(REQUIRED_FEATURES);
    if !missing.is_empty() {
        return Err(Exit::error(format!(
            "the guest image lacks control features that layered sandboxes need ({}); use an \
             NVX release or build that includes them",
            missing.join(", ")
        )));
    }
    Ok(())
}

/// Appends the network options that a configuration records, in the order of `_start`.
fn extend_recorded_network_arguments(command: &mut Vec<String>, config: &Document) -> Result {
    let root = config.root();
    let text =
        |id| py_str(config, id).ok_or_else(|| Exit::error("sandbox configuration is malformed"));
    let present =
        |name| member(config, root, name).filter(|&id| !matches!(config.node(id), Node::Null));
    if let Some(net) = present("net") {
        command.push("--net".to_owned());
        command.push(text(net)?);
        command.push("--network-profile".to_owned());
        command.push(match member(config, root, "network_profile") {
            Some(profile) => text(profile)?,
            None => "None".to_owned(),
        });
    }
    for name in ["network_egress", "network_ingress", "host_loopback"] {
        if let Some(value) = present(name) {
            command.push(format!("--{}", name.replace('_', "-")));
            command.push(text(value)?);
        }
    }
    for name in [
        "network_egress_allow",
        "network_egress_deny",
        "host_loopback_forward",
    ] {
        let Some(values) = member(config, root, name) else {
            continue;
        };
        let Node::Array(items) = config.node(values) else {
            return Err(Exit::error("sandbox configuration is malformed"));
        };
        for &item in items {
            command.push(format!("--{}", name.replace('_', "-")));
            command.push(text(item)?);
        }
    }
    if let Some(proxy) = present("network_proxy") {
        command.push("--network-proxy".to_owned());
        command.push(text(proxy)?);
    }
    Ok(())
}

/// Creates the control capability file of a start, which only it may hold, as
/// `_create_capability` does.
fn create_capability(path: &PurePath, capability: &[u8; CAPABILITY_LEN]) -> Result {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = match options.open(path.as_path()) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Exit::error(
                "sandbox is already running or has stale runtime state",
            ));
        }
        Err(error) => return Err(os_error(&error, path)),
    };
    if let Err(error) = file.write_all(capability) {
        drop(file);
        let _ = unlink_missing_ok(path);
        return Err(os_error(&error, path));
    }
    Ok(())
}

/// Ends the OpenVMM process of a failed start, as `_end_failed_start` does: returns OpenVMM's
/// status if it exits on its own within `grace` seconds, or terminates it and returns `None`.
/// Terminating OpenVMM while it publishes its outcome report would leave the report's staging
/// file in the state directory, which deprovision refuses to remove.
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
        .map_err(|error| Exit::error(os_error_message(&error, None)))?;
    if status.is_none() {
        launched
            .terminate()
            .map_err(|error| Exit::error(os_error_message(&error, None)))?;
        let exited = launched
            .wait_timeout(Duration::from_secs(5))
            .map_err(|error| Exit::error(os_error_message(&error, None)))?;
        if exited.is_none() {
            launched
                .kill()
                .map_err(|error| Exit::error(os_error_message(&error, None)))?;
            let killed = launched
                .wait_timeout(Duration::from_secs(5))
                .map_err(|error| Exit::error(os_error_message(&error, None)))?;
            if killed.is_none() {
                let arguments: Vec<String> =
                    command.iter().map(|argument| repr(argument)).collect();
                return Err(Exit::error(format!(
                    "Command '[{}]' timed out after 5 seconds",
                    arguments.join(", ")
                )));
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
            .map_err(|error| Exit::error(os_error_message(&error, None)))?
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
        let status_code = Int::parse(status_code).map_err(|_| Exit::error("invalid status"))?;
        return Ok(format!(
            "OpenVMM exited during startup: {} status {status_code}",
            category.render()
        ));
    }
    Ok(format!(
        "OpenVMM exited during startup with status {status}"
    ))
}

/// Runs a workload in a running sandbox, as `exec_workload` does.
pub(crate) fn exec_workload(
    state_path: &str,
    request: &ExecRequest,
    response_timeout: f64,
) -> Result<ExecResult> {
    let state_dir = prepare_state_directory(state_path, false)?;
    let running = load_running(&state_dir)?;
    let mut session = connect(&running, response_timeout)?;
    session.exec(request, response_timeout)
}

/// Stops a running sandbox and returns OpenVMM's outcome report, as `stop` does.
pub(crate) fn stop(state_path: &str, timeout: f64) -> Result<Document> {
    let state_dir = prepare_state_directory(state_path, false)?;
    let _lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    let running = load_running(&state_dir)?;
    let process = runtime_process(&running.runtime)?;
    {
        let mut session = connect(&running, timeout)?;
        session.stop(timeout)?;
    }
    let deadline = Deadline::after(timeout);
    while process_running(&process.pid, process.start_time.as_ref())? {
        if deadline.passed() {
            return Err(Exit::error("OpenVMM did not terminate after managed stop"));
        }
        sleep(RETRY_INTERVAL)?;
    }
    let outcome = read_openvmm_outcome(&state_dir.join(OUTCOME_NAME));
    // As in a failed start, the runtime record goes last.
    unlink_missing_ok(&state_dir.join(CONTROL_SOCKET_NAME))?;
    unlink_missing_ok(&state_dir.join(CAPABILITY_NAME))?;
    unlink_missing_ok(&state_dir.join(RUNTIME_NAME))?;
    outcome
}

/// Removes a stopped sandbox and its state directory, as `deprovision` does.
pub(crate) fn deprovision(state_path: &str, timeout: f64) -> Result {
    let state_dir = prepare_state_directory(state_path, false)?;
    let mut lock = LifecycleLock::acquire(&state_dir, timeout, false)?;
    let runtime_path = state_dir.join(RUNTIME_NAME);
    if fs::is_file(runtime_path.as_path()) {
        let runtime = read_json(
            &runtime_path,
            "sandbox runtime state",
            "format",
            &[STATE_FORMAT],
        )?;
        let process = runtime_process(&runtime)?;
        if process_running(&process.pid, process.start_time.as_ref())? {
            return Err(Exit::error("sandbox must be stopped before deprovision"));
        }
    } else if RUNTIME_FILE_NAMES
        .iter()
        .any(|name| fs::lexists(state_dir.join(name).as_path()))
    {
        // As start does, refuse runtime files that no record vouches for.
        return Err(Exit::error(format!(
            "sandbox has runtime files but no runtime record, so its OpenVMM process may still \
             run: end any OpenVMM process whose arguments name the state directory, then remove \
             {CAPABILITY_NAME} and {CONTROL_SOCKET_NAME}"
        )));
    }
    for name in [
        RUNTIME_NAME,
        CAPABILITY_NAME,
        CONTROL_SOCKET_NAME,
        OUTCOME_NAME,
        LOG_NAME,
        CONFIG_NAME,
    ] {
        unlink_missing_ok(&state_dir.join(name))?;
    }
    let unknown: Vec<String> = entries(&state_dir)
        .map_err(|error| os_error(&error, &state_dir))?
        .into_iter()
        .filter(|name| name != LOCK_NAME)
        .collect();
    if !unknown.is_empty() {
        // `iterdir` lists the names in the order of the directory, which the message keeps.
        return Err(Exit::error(format!(
            "sandbox state directory contains files not owned by NVX: {}",
            unknown.join(", ")
        )));
    }
    // Holding the lock until the directory is gone keeps a waiting transition from creating a new
    // lock file in it.
    lock.remove()?;
    let lingering = match std::fs::remove_dir(state_dir.as_path()) {
        Ok(()) => false,
        Err(error) if directory_not_empty(&error) => true,
        Err(error) => {
            lock.retract();
            return Err(os_error(&error, &state_dir));
        }
    };
    if !lingering {
        lock.retract();
        return Ok(());
    }
    // A removed lock file that a handle still keeps stays in the directory, as do the lock files of
    // transitions that start meanwhile. Each waiter closes its handle once it finds the sandbox
    // gone, so the directory empties after this call releases the lock.
    let (removing, held) = hold_directory(&state_dir)?;
    lock.release();
    let removed = remove_emptied_state_directory(&state_dir, removing, timeout);
    drop(held);
    removed
}

/// Returns the identity of a directory, and a handle that keeps it, as `_hold_directory` does: on
/// POSIX, the open directory keeps a directory that replaces it from reusing its inode number;
/// NTFS and ReFS do not reuse file IDs.
fn hold_directory(path: &PurePath) -> Result<((u64, u128), Option<File>)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let file = OpenOptions::new()
            .read(true)
            .custom_flags((nix::fcntl::OFlag::O_DIRECTORY | nix::fcntl::OFlag::O_NOFOLLOW).bits())
            .open(path.as_path())
            .map_err(|error| os_error(&error, path))?;
        let status =
            fs::fstat(&file).map_err(|error| Exit::error(os_error_message(&error, None)))?;
        Ok((status.identity, Some(file)))
    }
    #[cfg(not(unix))]
    {
        let status = fs::lstat(path.as_path()).map_err(|error| os_error(&error, path))?;
        Ok((status.identity(), None))
    }
}

/// Removes a state directory once no lingering lock file keeps it, as
/// `_remove_emptied_state_directory` does. A provision that waits for the removal may complete it
/// and provision the path again, which ends this attempt too.
fn remove_emptied_state_directory(
    state_dir: &PurePath,
    removing: (u64, u128),
    timeout: f64,
) -> Result {
    let deadline = Deadline::after(timeout);
    let mut retried_at_once = false;
    loop {
        let error = match std::fs::remove_dir(state_dir.as_path()) {
            Ok(()) => return Ok(()),
            Err(error) if os_error_class(&error) == OsErrorClass::NotFound => return Ok(()),
            Err(error) => error,
        };
        if directory_status(state_dir) != Some(removing) {
            return Ok(());
        }
        let remaining = match entries(state_dir) {
            Ok(remaining) => remaining,
            Err(error) if os_error_class(&error) == OsErrorClass::NotFound => return Ok(()),
            Err(error) => return Err(os_error(&error, state_dir)),
        };
        let mut unexpected: Vec<String> = remaining
            .iter()
            .filter(|name| !lingers(name))
            .cloned()
            .collect();
        unexpected.sort();
        // A provision that starts after this deprovision removed the lock file, and before it
        // removed the directory, provisions it again.
        if unexpected.iter().any(|name| name == CONFIG_NAME) {
            return Err(Exit::error(
                "sandbox was provisioned again while deprovision removed it",
            ));
        }
        if !unexpected.is_empty() {
            return Err(Exit::error(format!(
                "sandbox state directory contains files not owned by NVX: {}",
                unexpected.join(", ")
            )));
        }
        if !directory_not_empty(&error) {
            return Err(os_error(&error, state_dir));
        }
        // Without remaining entries, the last lingering lock file went away after rmdir failed,
        // so the next attempt follows at once. Another such failure right after it pauses first,
        // so that a file system that does not report emptiness right away cannot spin.
        if remaining.is_empty() && !retried_at_once {
            retried_at_once = true;
            if deadline.passed() {
                return Err(os_error(&error, state_dir));
            }
            continue;
        }
        retried_at_once = false;
        if !deadline.pause(LOCK_RETRY_INTERVAL)? {
            return Err(os_error(&error, state_dir));
        }
    }
}
