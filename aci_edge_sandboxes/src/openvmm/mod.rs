//! Default backend: drives the `openvmm` binary directly.
//!
//! Each sandbox is a microVM that runs the NVX guest's Alpine Linux userland directly from its
//! initramfs; there are no image layers, scratch disks, or container namespaces. Guest state lives
//! in memory and lasts until the sandbox stops.
//!
//! [`OpenVmmBackend`] keeps each sandbox in a state directory under
//! [`OpenVmmConfig::state_root`], so every operation can run in a different process:
//!
//! - **provision** validates the request and records the configuration. No VM runs yet.
//! - **start** launches a detached OpenVMM process with the managed lifecycle, passes it a fresh
//!   32-byte capability through standard input, and waits until the guest agent answers on the
//!   authenticated control console. The agent must also advertise the control features this
//!   backend depends on (cancellation, host path mappings, workload accounts, and workload
//!   containment). A guest image that lacks one is terminated and start fails with
//!   [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable), because such an
//!   image would silently ignore the policy or request that needs the feature.
//! - **exec** runs the workload through the control console and streams its output live.
//! - **stop** asks the guest to shut down and falls back to terminating OpenVMM after
//!   [`OpenVmmConfig::stop_timeout`].
//! - **deprovision** deletes the state directory.
//!
//! Every operation first reconciles the recorded OpenVMM process with the host: a sandbox whose
//! VM died is treated as provisioned again.
//!
//! # Policy honor matrix
//!
//! | Field | provision | exec |
//! | --- | --- | --- |
//! | `filesystem.readonlyPaths`, `readwritePaths` | mapped at their [`guest_path`] | n/a |
//! | `filesystem.deniedPaths` | hidden inside mapped paths | n/a |
//! | `network.egress` | default and IPv4 rules applied | n/a |
//! | `network.ingress`, `hostLoopback` | `deny` only | n/a |
//! | `microvm.provision.memoryMib` | applied | n/a |
//! | `process.commandLine` | n/a | run as `/bin/sh -c <commandLine>`, at most 4096 bytes |
//! | `process.cwd` | n/a | an absolute guest path, entered before the workload runs |
//! | `process.timeout` | n/a | up to one hour |
//! | `process.env`, `inheritDefaultEnv: false`, piped stdin | n/a | rejected |
//!
//! Host paths share OpenVMM's single virtio-fs export: the backend exports the deepest directory
//! that contains every mapped path to a guest directory that only the guest's root can enter, and
//! the guest agent bind-mounts each mapped path, read-only or read-write. Egress rules are expanded
//! into OpenVMM's IPv4 rules exactly; IPv6, ICMP-only rules, and rules that would need more than
//! 256 OpenVMM rules are rejected. Egress denied without allow rules, with ingress denied,
//! attaches no network device. Workloads run as the fixed non-root identity of
//! [`OpenVmmConfig::workload_uid`] and [`OpenVmmConfig::workload_gid`] (see
//! [`OpenVmmConfig::map_host_identity`] for Linux hosts) with no capabilities, read end-of-file on
//! standard input, and may produce at most 1 MiB of combined output.
//!
//! # Concurrency and cancellation
//!
//! Lifecycle transitions of one sandbox are serialized by a lock file. Executions are serialized
//! by the guest: a second exec waits up to [`OpenVmmConfig::control_timeout`] for the first to
//! finish. A stop that cannot reach the guest within its timeout terminates the VM, and any
//! running exec then fails.
//!
//! The guest agent contains each workload in a cgroup and kills whatever the workload leaves
//! behind when its first process exits, so no workload process outlives its exec.
//! [`Canceller::cancel`](crate::Canceller::cancel) asks the guest to kill the workload; the
//! execution then ends with [`ExecOutcome::Cancelled`]. If the process that runs an execution
//! dies, the guest kills the workload as soon as it notices that the control session is gone.

mod artifacts;
mod config;
mod contract;
mod filesystem;
mod launch;
mod network;
mod platform;
mod process;
mod protocol;
mod session;
mod state;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

pub use self::artifacts::Artifacts;
pub use self::config::{Hypervisor, OpenVmmConfig};
pub use self::filesystem::{guest_path, resolve_guest_path};
use self::protocol::{
    CAPABILITY_LEN, ExitCategory, GuestFeatures, MAX_ARGUMENT_BYTES, MAX_OUTPUT_BYTES,
    MAX_TIMEOUT_MS,
};
use self::session::{ControlSession, ExecEvent, SessionError};
use self::state::{
    BACKEND_KEY, LaunchRecord, ProcessIdentity, RuntimeRecord, STATE_FORMAT, SandboxRecord,
    StateStore,
};
use crate::backend::{Backend, ExecControl, ExecIo};
use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::exec::{Completion, ExecFailure, ExecOutcome};
use crate::id::SandboxId;
use crate::model::{
    Command, DeprovisionResult, ExecRequest, Metadata, ProcessSpec, ProvisionRequest,
    ProvisionResult, StartResult, StopResult, duration_millis,
};

/// Shell that interprets `process.commandLine`.
const SHELL: &str = "/bin/sh";

/// Longest delay between a cancellation request and its delivery to the guest.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// [`Backend`] that drives the `openvmm` binary directly.
///
/// See the [module documentation](self) for the lifecycle, policy honor matrix, and concurrency
/// behavior.
#[derive(Debug)]
pub struct OpenVmmBackend {
    config: OpenVmmConfig,
    store: StateStore,
}

enum RunState {
    Provisioned,
    Running(RuntimeRecord),
}

impl OpenVmmBackend {
    /// Creates the backend, resolving relative paths and creating the state root.
    ///
    /// Fails with [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable) when
    /// the configuration cannot work on this host.
    pub fn new(config: OpenVmmConfig) -> Result<Self> {
        let config = config.normalized()?;
        let store = StateStore::open(&config.state_root).map_err(|error| {
            Error::backend_unavailable(format!(
                "cannot open the sandbox state root {}",
                config.state_root.display()
            ))
            .with_source(error)
        })?;
        Ok(Self { config, store })
    }

    /// Returns the normalized configuration.
    pub fn config(&self) -> &OpenVmmConfig {
        &self.config
    }

    /// Returns the path of the OpenVMM log of a sandbox's latest start, for diagnostics.
    pub fn log_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.store.log_path(sandbox_id)
    }

    /// Returns whether the recorded OpenVMM process still runs, clearing stale runtime state.
    ///
    /// State is cleared only when the process is definitely gone; an inconclusive check fails
    /// the operation instead, so a live VM is never forgotten. A start whose caller died before
    /// recording OpenVMM's identity is recovered through its launch marker.
    fn reconcile(&self, sandbox_id: &SandboxId) -> Result<RunState> {
        if let Some(runtime) = self.store.runtime(sandbox_id)? {
            let current = platform::process_start_time(runtime.pid).map_err(|error| {
                Error::backend_error(format!(
                    "cannot determine whether OpenVMM process {} of sandbox {sandbox_id} is \
                     running",
                    runtime.pid
                ))
                .with_source(error)
            })?;
            if current == Some(runtime.start_time) {
                self.store.remove_launch(sandbox_id);
                return Ok(RunState::Running(runtime));
            }
            self.store.clear_runtime(sandbox_id)?;
            return Ok(RunState::Provisioned);
        }
        if let Some(launch) = self.store.launch(sandbox_id)? {
            self.recover_launch(sandbox_id, &launch)?;
            self.store.clear_runtime(sandbox_id)?;
        }
        Ok(RunState::Provisioned)
    }

    /// Terminates the OpenVMM process of a start whose caller died before recording it.
    ///
    /// Lifecycle locks serialize starts, so a launch marker seen under the lock always belongs
    /// to an interrupted start. Recorded identity is authoritative even before an endpoint
    /// exists; legacy markers can be recovered only when their endpoint identifies the child.
    fn recover_launch(&self, sandbox_id: &SandboxId, launch: &LaunchRecord) -> Result<()> {
        let interrupted = |detail: &str| {
            Error::backend_error(format!(
                "an interrupted start of sandbox {sandbox_id} {detail}"
            ))
        };
        let identity = match &launch.process {
            Some(identity) => identity.clone(),
            None => {
                let pid = platform::endpoint_server_pid(&launch.endpoint)
                    .map_err(|error| interrupted("cannot be recovered yet").with_source(error))?;
                let Some(pid) = pid else {
                    return Err(interrupted(
                        "has no recorded process identity or observable endpoint; cannot verify \
                         that OpenVMM exited, so the launch marker is retained",
                    ));
                };
                let Some(start_time) = platform::process_start_time(pid)
                    .map_err(|error| interrupted("cannot be recovered yet").with_source(error))?
                else {
                    return Ok(());
                };
                ProcessIdentity { pid, start_time }
            }
        };
        match process::kill(identity.pid, identity.start_time) {
            Ok(true) => Ok(()),
            Ok(false) => Err(interrupted(&format!(
                "left OpenVMM process {} running, and it did not exit",
                identity.pid
            ))),
            Err(error) => Err(interrupted(&format!(
                "left OpenVMM process {} running, and it could not be terminated",
                identity.pid
            ))
            .with_source(error)),
        }
    }

    fn session_error(&self, sandbox_id: &SandboxId, error: SessionError) -> Error {
        match error {
            SessionError::ProcessExited => {
                Error::not_started(format!("sandbox {sandbox_id} stopped unexpectedly"))
            }
            SessionError::TimedOut => Error::backend_error(format!(
                "sandbox {sandbox_id} did not accept a control session within {} s; another exec \
                 may still be running",
                self.config.control_timeout.as_secs()
            )),
            other => Error::backend_error(format!(
                "the control session with sandbox {sandbox_id} failed: {other}"
            ))
            .with_source(other),
        }
    }

    fn start_failure(&self, sandbox_id: &SandboxId, reason: &str) -> Error {
        Error::backend_error(format!(
            "sandbox {sandbox_id} failed to start: {reason}; see {}",
            self.store.log_path(sandbox_id).display()
        ))
    }

    /// Terminates a launch that did not become ready.
    ///
    /// Runtime state is cleared only once the process has exited. Otherwise the sandbox stays
    /// marked as running so that `stop` can retry instead of a second VM being started.
    fn abort_start(&self, sandbox_id: &SandboxId, runtime: &RuntimeRecord, reason: &str) -> Error {
        let cleanup = match process::kill(runtime.pid, runtime.start_time) {
            Ok(true) => {
                let _ = self.store.clear_runtime(sandbox_id);
                return self.start_failure(sandbox_id, reason);
            }
            Ok(false) => "it did not exit".to_owned(),
            Err(error) => error.to_string(),
        };
        self.start_failure(
            sandbox_id,
            &format!(
                "{reason}; OpenVMM process {} could not be terminated ({cleanup}), so the \
                 sandbox stays marked as running until it is stopped",
                runtime.pid
            ),
        )
    }

    /// Terminates a launch whose guest image lacks control features that this backend needs.
    ///
    /// An older image would boot and answer, but it would silently ignore host path mappings and
    /// cancellation requests, so the sandbox must not run at all.
    fn reject_guest(
        &self,
        sandbox_id: &SandboxId,
        runtime: &RuntimeRecord,
        missing: &[&str],
    ) -> Error {
        let reason = format!(
            "the guest image lacks control features that the openvmm backend needs ({}); use an \
             NVX release or build that includes them, for example through {}",
            missing.join(", "),
            Artifacts::ENV_DIR
        );
        Error::backend_unavailable(self.abort_start(sandbox_id, runtime, &reason).message())
    }
}

/// Shell prelude that enters the working directory passed as `$1`. A missing directory ends the
/// workload with status 125 before it runs.
const CWD_PRELUDE: &str = "cd -- \"$1\" || exit 125\nshift\n";

fn workload_argv(process: &ProcessSpec) -> Result<Vec<String>> {
    let too_long = || {
        Error::policy_validation(format!(
            "process.commandLine exceeds the {MAX_ARGUMENT_BYTES}-byte limit of the openvmm \
             backend"
        ))
    };
    let Some(cwd) = &process.cwd else {
        return Ok(match &process.command {
            Command::CommandLine(command_line) => {
                if command_line.len() > MAX_ARGUMENT_BYTES {
                    return Err(too_long());
                }
                vec![SHELL.to_owned(), "-c".to_owned(), command_line.clone()]
            }
            Command::Argv(argv) => argv.clone(),
        });
    };
    if !cwd.starts_with('/') {
        return Err(Error::policy_validation(format!(
            "process.cwd {cwd:?} must be an absolute guest path; map host paths with \
             openvmm::guest_path"
        )));
    }
    // The guest agent has no working-directory field, so a shell enters the directory first.
    let mut argv = vec![SHELL.to_owned(), "-c".to_owned()];
    match &process.command {
        Command::CommandLine(command_line) => {
            let script = format!("{CWD_PRELUDE}{command_line}");
            if script.len() > MAX_ARGUMENT_BYTES {
                return Err(too_long());
            }
            argv.extend([script, SHELL.to_owned(), cwd.clone()]);
        }
        Command::Argv(command) => {
            argv.extend([
                format!("{CWD_PRELUDE}exec \"$@\""),
                SHELL.to_owned(),
                cwd.clone(),
            ]);
            argv.extend(command.iter().cloned());
        }
    }
    Ok(argv)
}

fn exec_timeout_ms(process: &ProcessSpec) -> Result<u32> {
    let millis = process.timeout.map_or(0, duration_millis);
    u32::try_from(millis)
        .ok()
        .filter(|millis| *millis <= MAX_TIMEOUT_MS)
        .ok_or_else(|| {
            Error::policy_validation(format!(
                "process.timeout exceeds the openvmm backend limit of {MAX_TIMEOUT_MS} ms"
            ))
        })
}

fn prepare_exec(process: &ProcessSpec) -> Result<(Vec<String>, u32)> {
    let argv = workload_argv(process)?;
    let timeout_ms = exec_timeout_ms(process)?;
    let validate = |arguments: &[String]| {
        protocol::encode_exec_payload(arguments, timeout_ms)
            .map(|_| ())
            .map_err(|error| Error::policy_validation(error.0))
    };
    // The cwd wrapper's /bin/sh must not hide an invalid program in the original argv.
    if let Command::Argv(arguments) = &process.command {
        validate(arguments)?;
    }
    validate(&argv)?;
    Ok((argv, timeout_ms))
}

/// Returns the calling user's IDs when workloads that map host paths should use them; see
/// [`OpenVmmConfig::map_host_identity`].
fn host_identity(config: &OpenVmmConfig, maps_host_paths: bool) -> Option<(u32, u32)> {
    if !(maps_host_paths && config.map_host_identity) {
        return None;
    }
    #[cfg(unix)]
    {
        // SAFETY: geteuid and getegid have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        (uid != 0 && gid != 0).then_some((uid, gid))
    }
    #[cfg(not(unix))]
    None
}

/// Explains why a launch did not become ready.
fn readiness_failure(error: &SessionError, runs_as_host_user: bool) -> String {
    let mut reason = error.to_string();
    // An image that predates account creation ends its boot, before the guest agent starts, when
    // it finds no account for the host user that the workloads run as. The VM then vanishes, or
    // its control endpoint closes first.
    let vm_ended = matches!(error, SessionError::ProcessExited | SessionError::Closed);
    if runs_as_host_user && vm_ended {
        reason.push_str(
            "; a guest image that predates workload account creation exits during boot when \
             workloads run under the host user's IDs, so use an NVX release or build that \
             includes it",
        );
    }
    reason
}

fn read_report(path: &Path) -> Option<Metadata> {
    let report: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let mut metadata = Metadata::new();
    if let Some(outcome) = report.get("outcome") {
        if let Some(category) = outcome.get("category").and_then(Value::as_str) {
            metadata.insert("vmOutcome".to_owned(), category.into());
        }
        if let Some(status) = outcome.get("status_code").and_then(Value::as_i64) {
            metadata.insert("vmStatusCode".to_owned(), status.into());
        }
    }
    if let Some(teardown) = report.get("teardown").and_then(Value::as_object) {
        let complete = teardown.values().all(|step| step.as_bool() == Some(true));
        metadata.insert("teardownComplete".to_owned(), complete.into());
    }
    Some(metadata)
}

impl Backend for OpenVmmBackend {
    fn name(&self) -> &str {
        BACKEND_KEY
    }

    fn capabilities(&self) -> Capabilities {
        let mut capabilities = Capabilities::new(BACKEND_KEY);
        capabilities.exec.command_line = true;
        capabilities.exec.argv = true;
        capabilities.exec.cancel = true;
        capabilities.exec.max_timeout_ms = Some(MAX_TIMEOUT_MS.into());
        capabilities.exec.max_output_bytes = Some(MAX_OUTPUT_BYTES as u64);
        capabilities.network.egress_allow = true;
        capabilities.network.egress_deny = true;
        capabilities.network.ingress_deny = true;
        capabilities.network.host_loopback_deny = true;
        capabilities.network.egress_rules = true;
        capabilities.filesystem.readonly_paths = true;
        capabilities.filesystem.readwrite_paths = true;
        capabilities.filesystem.denied_paths = true;
        capabilities.exec.cwd = true;
        capabilities
    }

    fn probe(&self) -> Result<()> {
        if !platform::SUPPORTED {
            return Err(Error::backend_unavailable(
                "the openvmm backend supports Linux and Windows hosts only",
            ));
        }
        for (path, description) in [
            (&self.config.openvmm, "OpenVMM executable"),
            (&self.config.kernel, "NVX guest kernel"),
            (&self.config.initrd, "NVX guest initramfs"),
        ] {
            if !path.is_file() {
                return Err(Error::backend_unavailable(format!(
                    "{description} not found: {}",
                    path.display()
                )));
            }
        }
        #[cfg(feature = "testing")]
        if self.config.skip_hypervisor_probe {
            return Ok(());
        }
        platform::probe_hypervisor(self.config.hypervisor).map_err(Error::backend_unavailable)
    }

    fn validate_provision(&self, request: &ProvisionRequest) -> Result<()> {
        let memory_mib = request
            .microvm
            .provision
            .memory_mib
            .unwrap_or(self.config.memory_mib);
        if memory_mib == 0 {
            return Err(Error::policy_validation(
                "microvm.provision.memoryMib must be positive",
            ));
        }
        network::network_arguments(request.network.as_ref(), &self.config.guest_network)?;
        Ok(())
    }

    fn validate_exec(&self, request: &ExecRequest) -> Result<()> {
        prepare_exec(&request.process).map(|_| ())
    }

    fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionResult> {
        self.validate_provision(request)?;
        self.probe()?;
        let memory_mib = request
            .microvm
            .provision
            .memory_mib
            .unwrap_or(self.config.memory_mib);
        let filesystem = match &request.filesystem {
            Some(policy) => filesystem::plan(policy)?,
            None => None,
        };
        let (workload_uid, workload_gid, create_workload_account) =
            match host_identity(&self.config, filesystem.is_some()) {
                Some((uid, gid)) => (uid, gid, true),
                None => (self.config.workload_uid, self.config.workload_gid, false),
            };
        let record = SandboxRecord {
            format: STATE_FORMAT,
            backend: BACKEND_KEY.to_owned(),
            network: request.network.clone(),
            filesystem,
            memory_mib,
            workload_uid,
            workload_gid,
            create_workload_account,
            hostname: self.config.hostname.clone(),
        };
        launch::kernel_command_line(&self.config.kernel_command_line, &record)?;
        let sandbox_id = SandboxId::generate()?;
        self.store.create(&sandbox_id, &record)?;
        Ok(ProvisionResult {
            sandbox_id,
            metadata: None,
        })
    }

    fn start(&self, sandbox_id: &SandboxId) -> Result<StartResult> {
        let _guard = self.store.lock(sandbox_id)?;
        let record = self.store.load(sandbox_id)?;
        if let RunState::Running(_) = self.reconcile(sandbox_id)? {
            return Err(Error::already_started(format!(
                "sandbox {sandbox_id} is already running"
            )));
        }
        self.probe()?;
        if let Some(mapping) = &record.filesystem {
            filesystem::verify(mapping)?;
        }

        let mut capability = [0u8; CAPABILITY_LEN];
        while capability == [0; CAPABILITY_LEN] {
            getrandom::fill(&mut capability).map_err(|error| {
                Error::backend_error("cannot generate a control capability").with_source(error)
            })?;
        }
        let socket = self.store.socket_path(sandbox_id);
        state::remove_if_present(&socket)?;
        let endpoint = platform::control_endpoint(&socket).map_err(|error| {
            Error::backend_error("cannot choose a control endpoint").with_source(error)
        })?;
        let report = self.store.outcome_path(sandbox_id);
        state::remove_if_present(&report)?;
        let arguments = launch::openvmm_arguments(&self.config, &record, &endpoint, &report)?;

        self.store.write_capability(sandbox_id, &capability)?;
        let log = self.store.create_log(sandbox_id)?;
        // Until the child is identified, recovery must retain this marker unless the endpoint
        // identifies the child. An absent endpoint never proves that a launch has exited.
        self.store.write_launch(
            sandbox_id,
            &LaunchRecord {
                format: STATE_FORMAT,
                endpoint: endpoint.clone(),
                process: None,
            },
        )?;
        let started = Instant::now();
        let child = match process::spawn(
            &self.config,
            &arguments,
            &capability,
            log,
            &self.store.dir(sandbox_id),
        ) {
            Ok(child) => child,
            Err(error) => {
                let _ = self.store.clear_runtime(sandbox_id);
                return Err(Error::backend_error(format!(
                    "cannot launch {}",
                    self.config.openvmm.display()
                ))
                .with_source(error));
            }
        };
        let pid = child.id();
        let start_time = match platform::process_start_time(pid) {
            Ok(Some(start_time)) => start_time,
            outcome => {
                // Keep the launch marker unless OpenVMM is gone, so recovery can retry.
                if process::kill_child(child) {
                    let _ = self.store.clear_runtime(sandbox_id);
                }
                let reason = match outcome {
                    Err(error) => format!("cannot identify the OpenVMM process: {error}"),
                    _ => "OpenVMM exited during startup".to_owned(),
                };
                return Err(self.start_failure(sandbox_id, &reason));
            }
        };
        let runtime = RuntimeRecord {
            format: STATE_FORMAT,
            pid,
            start_time,
            endpoint: endpoint.clone(),
        };
        let identified = LaunchRecord {
            format: STATE_FORMAT,
            endpoint: endpoint.clone(),
            process: Some(ProcessIdentity { pid, start_time }),
        };
        if let Err(error) = self
            .store
            .write_launch(sandbox_id, &identified)
            .and_then(|()| self.store.write_runtime(sandbox_id, &runtime))
        {
            if process::kill_child(child) {
                let _ = self.store.clear_runtime(sandbox_id);
            }
            return Err(error);
        }
        process::detach_child(child);
        self.store.remove_launch(sandbox_id);
        let deadline = started + self.config.start_timeout;
        let ready = session::connect(&endpoint, pid, start_time, deadline)
            .and_then(|transport| ControlSession::attach(transport, &capability, deadline))
            .and_then(|mut session| {
                session.ping(deadline)?;
                session.features(deadline)
            });
        let features = match ready {
            Ok(features) => features,
            Err(error) => {
                let reason = readiness_failure(&error, record.create_workload_account);
                return Err(self.abort_start(sandbox_id, &runtime, &reason));
            }
        };
        let missing = features.missing(GuestFeatures::REQUIRED);
        if !missing.is_empty() {
            return Err(self.reject_guest(sandbox_id, &runtime, &missing));
        }
        let mut metadata = Metadata::new();
        let boot_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        metadata.insert("bootMilliseconds".to_owned(), boot_ms.into());
        Ok(StartResult {
            metadata: Some(metadata),
        })
    }

    fn exec(
        &self,
        sandbox_id: &SandboxId,
        request: &ExecRequest,
        io: ExecIo,
    ) -> Result<Box<dyn ExecControl>> {
        let (argv, timeout_ms) = prepare_exec(&request.process)?;
        let (runtime, capability) = {
            let _guard = self.store.lock(sandbox_id)?;
            self.store.load(sandbox_id)?;
            match self.reconcile(sandbox_id)? {
                RunState::Provisioned => {
                    return Err(Error::not_started(format!(
                        "sandbox {sandbox_id} is not running"
                    )));
                }
                RunState::Running(runtime) => {
                    let capability = self.store.read_capability(sandbox_id)?;
                    (runtime, capability)
                }
            }
        };
        let deadline = Instant::now() + self.config.control_timeout;
        let mut session =
            session::connect(&runtime.endpoint, runtime.pid, runtime.start_time, deadline)
                .and_then(|transport| ControlSession::attach(transport, &capability, deadline))
                .map_err(|error| self.session_error(sandbox_id, error))?;
        let request_id = session
            .start_exec(&argv, timeout_ms, deadline)
            .map_err(|error| self.session_error(sandbox_id, error))?;
        let response_deadline = (timeout_ms > 0).then(|| {
            Instant::now()
                + Duration::from_millis(timeout_ms.into())
                + self.config.exec_response_grace
        });
        let shared = Arc::new(ExecShared::default());
        let worker = Arc::clone(&shared);
        let control_timeout = self.config.control_timeout;
        thread::Builder::new()
            .name("aci-edge-sandboxes-openvmm-exec".to_owned())
            .spawn(move || {
                let outcome = pump(
                    session,
                    request_id,
                    response_deadline,
                    io,
                    &worker.cancel_requested,
                    control_timeout,
                );
                worker.finish(outcome);
            })
            .map_err(|error| {
                Error::backend_error("cannot start an execution thread").with_source(error)
            })?;
        Ok(Box::new(OpenVmmExecution { shared }))
    }

    fn stop(&self, sandbox_id: &SandboxId) -> Result<StopResult> {
        let _guard = self.store.lock(sandbox_id)?;
        self.store.load(sandbox_id)?;
        let RunState::Running(runtime) = self.reconcile(sandbox_id)? else {
            return Err(Error::already_stopped(format!(
                "sandbox {sandbox_id} is not running"
            )));
        };
        let deadline = Instant::now() + self.config.stop_timeout;
        let graceful = (|| -> std::result::Result<(), SessionError> {
            let capability = self
                .store
                .read_capability(sandbox_id)
                .map_err(|error| SessionError::Protocol(error.to_string()))?;
            let transport =
                session::connect(&runtime.endpoint, runtime.pid, runtime.start_time, deadline)?;
            let mut session = ControlSession::attach(transport, &capability, deadline)?;
            session.stop(deadline)?;
            drop(session);
            if process::wait_for_exit(runtime.pid, runtime.start_time, deadline) {
                Ok(())
            } else {
                Err(SessionError::TimedOut)
            }
        })();
        let forced = !matches!(graceful, Ok(()) | Err(SessionError::ProcessExited));
        if forced {
            let exited = process::kill(runtime.pid, runtime.start_time).map_err(|error| {
                Error::backend_error(format!("cannot terminate OpenVMM process {}", runtime.pid))
                    .with_source(error)
            })?;
            if !exited {
                return Err(Error::backend_error(format!(
                    "OpenVMM process {} did not terminate",
                    runtime.pid
                )));
            }
        }
        let mut metadata = Metadata::new();
        metadata.insert("forced".to_owned(), forced.into());
        if let Some(report) = read_report(&self.store.outcome_path(sandbox_id)) {
            metadata.extend(report);
        }
        self.store.clear_runtime(sandbox_id)?;
        Ok(StopResult {
            metadata: Some(metadata),
        })
    }

    fn deprovision(&self, sandbox_id: &SandboxId) -> Result<DeprovisionResult> {
        let guard = self.store.lock(sandbox_id)?;
        self.store.load(sandbox_id)?;
        if let RunState::Running(_) = self.reconcile(sandbox_id)? {
            return Err(Error::already_started(format!(
                "sandbox {sandbox_id} is running; stop it before deprovisioning"
            )));
        }
        self.store.remove(sandbox_id)?;
        drop(guard);
        self.store.remove_lock(sandbox_id);
        Ok(DeprovisionResult::default())
    }
}

/// Relays workload events to the output sinks and returns the terminal outcome.
///
/// Events are read in short slices so that a cancellation request reaches the guest promptly.
/// The guest then terminates the workload and reports the cancelled outcome as usual.
fn pump(
    mut session: ControlSession,
    request_id: u64,
    mut deadline: Option<Instant>,
    mut io: ExecIo,
    cancel_requested: &AtomicBool,
    control_timeout: Duration,
) -> Result<ExecOutcome> {
    let mut forwarded = 0usize;
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut cancel_sent = false;
    loop {
        if !cancel_sent && cancel_requested.load(Ordering::Acquire) {
            let cancellation_deadline = Instant::now() + control_timeout;
            session
                .cancel_exec(request_id, cancellation_deadline)
                .map_err(|error| {
                    Error::backend_error(format!(
                        "cannot deliver the cancellation request: {error}"
                    ))
                    .with_source(error)
                })?;
            deadline = Some(deadline.map_or(cancellation_deadline, |deadline| {
                deadline.min(cancellation_deadline)
            }));
            cancel_sent = true;
        }
        let slice = Instant::now() + CANCEL_POLL_INTERVAL;
        let poll_deadline = if cancel_sent {
            deadline
        } else {
            Some(deadline.map_or(slice, |deadline| deadline.min(slice)))
        };
        let event = match session.next_exec_event(request_id, poll_deadline) {
            Err(SessionError::TimedOut)
                if poll_deadline != deadline
                    && deadline.is_none_or(|deadline| Instant::now() < deadline) =>
            {
                continue;
            }
            event => event,
        };
        let event = event.map_err(|error| match error {
            SessionError::TimedOut => {
                Error::backend_error("timed out waiting for the workload outcome")
            }
            SessionError::Closed | SessionError::Reset => Error::backend_error(
                "the control session ended before the workload finished; the sandbox VM may \
                     have stopped",
            ),
            other => Error::backend_error(format!("the control session failed: {other}"))
                .with_source(other),
        })?;
        let (chunk, sink, open) = match event {
            ExecEvent::Stdout(chunk) => (chunk, &mut io.stdout, &mut stdout_open),
            ExecEvent::Stderr(chunk) => (chunk, &mut io.stderr, &mut stderr_open),
            ExecEvent::Exit { category, status } => {
                return Ok(match category {
                    ExitCategory::Exit => ExecOutcome::Exited(status),
                    ExitCategory::Signal => {
                        ExecOutcome::Signaled(if status > 128 { status - 128 } else { status })
                    }
                    ExitCategory::Timeout => ExecOutcome::TimedOut,
                    ExitCategory::OutputLimit => {
                        ExecOutcome::Failed(ExecFailure::OutputLimitExceeded)
                    }
                    ExitCategory::Failed => ExecOutcome::Failed(ExecFailure::Workload),
                    ExitCategory::Cancelled => ExecOutcome::Cancelled,
                });
            }
            ExecEvent::Rejected { status, category } => {
                return if category == "launch-failed" {
                    Ok(ExecOutcome::Failed(ExecFailure::LaunchFailed))
                } else {
                    Err(Error::backend_error(format!(
                        "the guest agent rejected the workload: {category} (status {status})"
                    )))
                };
            }
        };
        forwarded = forwarded.saturating_add(chunk.len());
        if forwarded > MAX_OUTPUT_BYTES {
            return Err(Error::backend_error(
                "the guest exceeded the output limit of the control protocol",
            ));
        }
        if *open && sink.write(&chunk).is_err() {
            *open = false;
        }
    }
}

#[derive(Default)]
struct ExecShared {
    completion: Completion,
    cancel_requested: AtomicBool,
}

impl ExecShared {
    fn finish(&self, outcome: Result<ExecOutcome>) {
        self.completion.finish(outcome);
    }
}

struct OpenVmmExecution {
    shared: Arc<ExecShared>,
}

impl ExecControl for OpenVmmExecution {
    fn wait(&self) -> Result<ExecOutcome> {
        self.shared.completion.wait()
    }

    fn cancel(&self) -> Result<()> {
        // The execution thread delivers the request; a finished execution ignores it.
        self.shared.cancel_requested.store(true, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    #[test]
    fn command_lines_run_through_the_shell() {
        let request = ExecRequest::command_line("echo hi | wc -c");
        assert_eq!(
            workload_argv(&request.process).unwrap(),
            ["/bin/sh", "-c", "echo hi | wc -c"]
        );
        let long = ExecRequest::command_line("x".repeat(MAX_ARGUMENT_BYTES + 1));
        assert_eq!(
            workload_argv(&long.process).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
    }

    #[test]
    fn working_directories_are_entered_by_a_shell() {
        let request = ExecRequest::command_line("pwd").with_cwd("/mnt/c/work");
        assert_eq!(
            workload_argv(&request.process).unwrap(),
            [
                "/bin/sh",
                "-c",
                "cd -- \"$1\" || exit 125\nshift\npwd",
                "/bin/sh",
                "/mnt/c/work",
            ]
        );
        let request = ExecRequest::argv(["/bin/ls", "-l"]).with_cwd("/tmp");
        assert_eq!(
            workload_argv(&request.process).unwrap(),
            [
                "/bin/sh",
                "-c",
                "cd -- \"$1\" || exit 125\nshift\nexec \"$@\"",
                "/bin/sh",
                "/tmp",
                "/bin/ls",
                "-l",
            ]
        );
        let relative = ExecRequest::command_line("pwd").with_cwd(r"C:\work");
        assert_eq!(
            workload_argv(&relative.process).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
    }

    #[test]
    fn timeouts_are_bounded_by_the_guest_agent() {
        let within = ExecRequest::command_line("true").with_timeout(Duration::from_secs(3600));
        assert_eq!(exec_timeout_ms(&within.process).unwrap(), MAX_TIMEOUT_MS);
        let beyond = ExecRequest::command_line("true").with_timeout(Duration::from_secs(3601));
        assert_eq!(
            exec_timeout_ms(&beyond.process).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
        let none = ExecRequest::command_line("true");
        assert_eq!(exec_timeout_ms(&none.process).unwrap(), 0);
    }

    #[test]
    fn readiness_failures_explain_images_without_workload_accounts() {
        let hint = "predates workload account creation";
        for ended in [SessionError::ProcessExited, SessionError::Closed] {
            assert!(readiness_failure(&ended, true).contains(hint));
            // Only a sandbox that needs the account suggests it.
            assert!(!readiness_failure(&ended, false).contains(hint));
        }
        for other in [
            SessionError::TimedOut,
            SessionError::Protocol("invalid record".to_owned()),
        ] {
            assert_eq!(readiness_failure(&other, true), other.to_string());
        }
    }

    #[test]
    fn reports_are_summarized_without_free_form_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("outcome.json");
        fs::write(
            &path,
            r#"{"schema_version":1,"outcome":{"operation":"managed","category":"success",
               "status_code":0},"teardown":{"workers":true,"memory":true}}"#,
        )
        .unwrap();
        let report = read_report(&path).unwrap();
        assert_eq!(report["vmOutcome"], "success");
        assert_eq!(report["vmStatusCode"], 0);
        assert_eq!(report["teardownComplete"], true);
        assert!(read_report(&directory.path().join("missing.json")).is_none());
    }
}
