//! Supervision of one VM instance: its OpenVMM process, its control endpoint, and the records
//! that name them, through the [`store`](crate::host::store), the [`crate::host::supervisor`], and the control
//! [`session`].
//!
//! An instance lives in a [`SandboxStore`] of its own: a directory private to the current user,
//! in which the store keeps the instance's lock, the journal of its launch, and the runtime
//! record of its VM, and in which the caller may keep files of its own under other names, which it
//! declares as [`CallerFiles`], such as the configuration record whose presence means that the
//! instance is provisioned:
//!
//! ```text
//! <dir>/
//!   sandbox.lock            the lock that serializes the instance's lifecycle transitions
//!   launch.json             the journal of a start in progress, or of one that was interrupted
//!   runtime.json            the runtime record of the running VM
//!   control.capability      launch capability (running only)
//!   control.sock            Linux control endpoint (running only)
//!   openvmm.log             OpenVMM output of the latest start
//!   outcome.json            OpenVMM outcome report of the latest run
//!   ...                     the caller's files
//! ```
//!
//! Lifecycle transitions run under the instance's lock, through the [`LockedInstance`] that
//! [`Instance::lock`] returns, so that every operation can run in a different process. The lock is
//! the store's [lifecycle lock](crate::host::store#lifecycle-locks) of an existing sandbox, which knows
//! the caller's files:
//!
//! - [`LockedInstance::find`] finds the instance's VM as an orchestrator does after it restarts
//!   within the same host boot, with [`crate::host::supervisor::reattach`]: it ends a start whose caller died,
//!   forgets a VM that exited, and compares the caller's runtime identity with the one that a
//!   running VM was launched with. A VM of another runtime identity is [`Found::Mismatched`]: it
//!   runs through artifacts or contracts that the caller does not have, so the instance does not
//!   drive it, and only ends it.
//! - [`LockedInstance::start`] launches a detached OpenVMM process with the managed lifecycle
//!   through [`crate::host::supervisor::launch`], passes it a fresh 32-byte capability through standard input,
//!   and waits until the guest agent answers on the authenticated control console. The agent must
//!   also advertise the control features in [`crate::protocol::GuestFeatures::REQUIRED`]; a guest image that lacks
//!   one is terminated, because it would silently ignore the policy or request that needs the
//!   feature. Start then hands the agent the table of mapped host paths, and fails if the agent
//!   cannot mount them all.
//! - [`LockedInstance::stop`] asks the guest to shut down and falls back to terminating OpenVMM
//!   through its verified process identity.
//! - [`LockedInstance::terminate`] stops a mismatched VM: it terminates OpenVMM through its
//!   verified process identity, opens no control session, whose protocol the guest might not
//!   speak, and reads no outcome report, which follows the other runtime's contracts.
//! - [`LockedInstance::remove`] removes a stopped instance, with the caller's files, and its
//!   directory, through the store's [removal](crate::host::store#removal).
//!
//! [`Instance::exec`] runs a workload through the control console of a [`Running`] VM without the
//! lock, and [`Execution::pump`] streams its output live. The guest serializes executions: a
//! second execution waits until the first finishes. Messages call the instance `sandbox NAME`,
//! after the name that [`Instance::create`] or [`Instance::open`] gives it.

mod execution;
mod files;
mod locked;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::host::artifacts::Artifacts;
use crate::host::launch::LaunchSpec;
use crate::host::platform;
use crate::host::report::Report;
use crate::host::session::{self, ControlSession, SessionError};
use crate::host::store::{
    CallerFiles, IdentityPart, OUTCOME_NAME, Recovery, RuntimeRecord, SOCKET_NAME, SandboxStore,
};
use crate::host::supervisor::{Starting, Vmm};
use crate::protocol::{CAPABILITY_LEN, Workload};

pub use self::execution::{Execution, OutputSink, WorkloadOutcome};
use self::files::io_error;
pub use self::files::{
    read_json, remove_if_present, remove_interrupted_writes, write_json, write_private,
};
pub use self::locked::LockedInstance;
use self::locked::check_timeout;

/// Returns a fresh launch capability, which is never all zero.
pub fn generate_capability() -> Result<[u8; CAPABILITY_LEN]> {
    let mut capability = [0u8; CAPABILITY_LEN];
    while capability == [0; CAPABILITY_LEN] {
        getrandom::fill(&mut capability).map_err(|error| {
            Error::other("cannot generate a control capability").with_source(error)
        })?;
    }
    Ok(capability)
}

/// Returns a control endpoint of an instance whose Linux control socket is `socket`: the socket
/// on Linux, and a fresh named pipe, whose name always has the same length, on Windows, as a
/// launch chooses it.
fn control_endpoint(socket: &Path) -> Result<String> {
    platform::control_endpoint(socket)
        .map_err(|error| Error::io("cannot choose a control endpoint").with_source(error))
}

/// Fails with [`ErrorKind::Unsupported`] if OpenVMM's command line for `spec`, launched for an
/// instance in the directory `dir`, would not fit the limit of a Windows process command line;
/// see [`LaunchSpec::check_process_command_line`].
///
/// The directory need not exist yet, so that a caller can check a launch before it creates the
/// instance. Every control endpoint of an instance has the same length.
pub fn check_launch(dir: &Path, spec: &LaunchSpec) -> Result<()> {
    let dir = std::path::absolute(dir).map_err(io_error(format!(
        "cannot resolve the instance directory {}",
        dir.display()
    )))?;
    let endpoint = control_endpoint(&dir.join(SOCKET_NAME))?;
    spec.check_process_command_line(&endpoint, &dir.join(OUTCOME_NAME))
}

/// Classifies a session failure.
fn session_kind(error: &SessionError) -> ErrorKind {
    match error {
        SessionError::TimedOut => ErrorKind::TimedOut,
        SessionError::ProcessExited => ErrorKind::Exited,
        SessionError::Refused(_) | SessionError::Io(_) => ErrorKind::Io,
        SessionError::Closed
        | SessionError::Reset
        | SessionError::AuthenticationFailed
        | SessionError::Protocol(_) => ErrorKind::Protocol,
    }
}

/// Returns the code of a session failure: [`ErrorCode::EndpointClosed`] for a session that the
/// control endpoint closed, as OpenVMM closes it when the VM shuts down, and otherwise the code of
/// its [kind](session_kind).
fn session_code(error: &SessionError) -> ErrorCode {
    match error {
        SessionError::Closed => ErrorCode::EndpointClosed,
        other => session_kind(other).into(),
    }
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

/// A VM that [`LockedInstance::start`] launched and the guest agent of which is ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// The runtime record of its launch: its OpenVMM process, its control endpoint, and the
    /// runtime identity that it was launched with.
    pub runtime: RuntimeRecord,
    /// Time from launch to guest readiness.
    pub boot: Duration,
}

/// A VM that [`LockedInstance::stop`] stopped, or that [`LockedInstance::terminate`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopped {
    /// Whether OpenVMM was terminated: because the guest did not shut down in time, or because
    /// the VM ran under another runtime identity.
    pub forced: bool,
    /// Summary of OpenVMM's outcome report, if it wrote one. [`LockedInstance::terminate`] reads
    /// none, since the other runtime's OpenVMM wrote it.
    pub report: Option<Report>,
}

/// The running VM of an instance, which [`LockedInstance::find`] found under the caller's runtime
/// identity, and the capability with which its control sessions attach.
pub struct Running {
    /// The VM, held through its verified process identity.
    vmm: Vmm,
    /// The launch capability of the VM, or the error with which reading it failed: only the
    /// VM's control sessions need it, and a stop ends the VM without one.
    capability: Result<[u8; CAPABILITY_LEN]>,
}

impl Running {
    /// Returns the VM, held through its verified process identity.
    pub fn vmm(&self) -> &Vmm {
        &self.vmm
    }

    /// Returns the runtime record of the VM's launch.
    pub fn runtime(&self) -> &RuntimeRecord {
        self.vmm.record()
    }

    /// Returns the launch capability of the VM, or fails as reading it failed, with the same
    /// code, description, and the description of its cause.
    fn capability(&self) -> Result<&[u8; CAPABILITY_LEN]> {
        self.capability.as_ref().map_err(|error| {
            let copy = Error::new(error.code(), error.message());
            match std::error::Error::source(error) {
                Some(cause) => copy.with_source(io::Error::other(cause.to_string())),
                None => copy,
            }
        })
    }
}

impl fmt::Debug for Running {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The capability authenticates control sessions, so it is never shown.
        formatter
            .debug_struct("Running")
            .field("vmm", &self.vmm)
            .finish_non_exhaustive()
    }
}

/// The running VM of an instance, which [`LockedInstance::find`] found under another runtime
/// identity than the caller's, and which [`LockedInstance::terminate`] ends.
#[derive(Debug)]
pub struct Mismatched {
    /// The instance's name in messages.
    name: String,
    /// The VM, held through its verified process identity.
    vmm: Vmm,
    /// The parts of the runtime identity that differ.
    differences: Vec<IdentityPart>,
}

impl Mismatched {
    /// Returns the VM, held through its verified process identity.
    pub fn vmm(&self) -> &Vmm {
        &self.vmm
    }

    /// Returns the parts of the runtime identity that differ, as
    /// [`crate::host::store::RuntimeIdentity::differences`] returns them.
    pub fn differences(&self) -> &[IdentityPart] {
        &self.differences
    }

    /// Returns the error that reports the VM, which the instance does not drive, since only the
    /// build and artifacts that started it speak its contracts: it names the parts that differ,
    /// and that stopping the VM terminates it. Its code is [`ErrorCode::RuntimeMismatch`].
    pub fn error(&self) -> Error {
        let parts: Vec<&str> = self.differences.iter().map(|part| part.name()).collect();
        Error::new(
            ErrorCode::RuntimeMismatch,
            format!(
                "sandbox {} runs under another runtime identity, which differs in {}, so it is \
                 not driven: stop it to terminate its OpenVMM process {} without a guest shutdown",
                self.name,
                parts.join(", "),
                self.vmm.record().process().pid
            ),
        )
    }
}

/// What [`LockedInstance::find`] found of an instance's VM.
#[derive(Debug)]
pub enum Found {
    /// No VM of the instance runs: none was started, the last one stopped or exited, or a start
    /// that was interrupted is over.
    Stopped,
    /// The VM runs under the caller's runtime identity.
    Running(Running),
    /// The VM runs under another runtime identity: the caller ends it with
    /// [`LockedInstance::terminate`], or reports it, as [`Mismatched::error`] does.
    Mismatched(Mismatched),
}

/// One VM instance in a sandbox store of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// The store of the instance's runtime records, logs, and control endpoint.
    store: SandboxStore,
    /// Sandbox name used in diagnostics.
    name: String,
    /// The files that the caller keeps in the instance's directory.
    files: CallerFiles,
}

impl Instance {
    /// Creates the directory `dir` of a new instance, whose parent must exist, private to the
    /// current user, as [`SandboxStore::create`] does, and supervises the instance in it, which
    /// messages call `sandbox NAME`, and in which the caller keeps `files`.
    pub fn create(
        dir: impl Into<PathBuf>,
        name: impl Into<String>,
        files: CallerFiles,
    ) -> Result<Self> {
        Ok(Self {
            store: SandboxStore::create(dir)?,
            name: name.into(),
            files,
        })
    }

    /// Supervises the instance in the existing directory `dir`, which must be private to the
    /// current user, as [`SandboxStore::open`] requires, which messages call `sandbox NAME`, and
    /// in which the caller keeps `files`. Fails with [`ErrorKind::NotFound`] if `dir` does not
    /// exist.
    pub fn open(
        dir: impl Into<PathBuf>,
        name: impl Into<String>,
        files: CallerFiles,
    ) -> Result<Self> {
        Ok(Self {
            store: SandboxStore::open(dir)?,
            name: name.into(),
            files,
        })
    }

    /// Returns the store of the instance's runtime records.
    pub fn store(&self) -> &SandboxStore {
        &self.store
    }

    /// Returns the instance's directory, as an absolute path.
    pub fn dir(&self) -> &Path {
        self.store.dir()
    }

    /// Returns the instance's name in messages.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the files that the caller keeps in the instance's directory.
    pub fn files(&self) -> &CallerFiles {
        &self.files
    }

    /// Returns the path of the OpenVMM log of the latest start.
    pub fn log_path(&self) -> PathBuf {
        self.store.log_path()
    }

    /// Returns the path of OpenVMM's outcome report of the latest run.
    pub fn report_path(&self) -> PathBuf {
        self.store.outcome_path()
    }

    /// Takes the instance's lock, which serializes its lifecycle transitions, waiting for it
    /// until `deadline`, or for as long as it takes without one, as
    /// [`SandboxStore::lock_existing`] does with the caller's files.
    ///
    /// Fails with [`ErrorKind::NotFound`] once the instance's directory is gone, as a removal
    /// that this waited for leaves it, and with [`ErrorKind::TimedOut`] when the deadline passes
    /// first. A caller that keeps a configuration record checks it under the lock: the lock of a
    /// directory without it is released without a lock file.
    pub fn lock(&self, deadline: Option<Instant>) -> Result<LockedInstance> {
        Ok(LockedInstance {
            locked: SandboxStore::lock_existing(self.dir(), &self.files, deadline)?,
            instance: self.clone(),
        })
    }

    /// Describes why this instance did not start and points the caller to its log.
    fn start_failure(&self, code: impl Into<ErrorCode>, reason: &str) -> Error {
        Error::new(
            code,
            format!(
                "sandbox {} failed to start: {reason}; see {}",
                self.name,
                self.log_path().display()
            ),
        )
    }

    /// Ends a launch that did not become ready, as [`Starting::abandon`] does, and describes why
    /// the instance did not start.
    ///
    /// A launch whose OpenVMM cannot be ended keeps its journal, and the next transition of the
    /// instance ends it, when [`LockedInstance::find`] recovers it.
    fn abort_start(
        &self,
        starting: Starting<'_>,
        code: impl Into<ErrorCode>,
        reason: &str,
    ) -> Error {
        let code = code.into();
        let unended = match starting.abandon() {
            Ok(Recovery::Indeterminate(unended)) => unended,
            Ok(_) => return self.start_failure(code, reason),
            Err(error) => error.to_string(),
        };
        self.start_failure(
            code,
            &format!(
                "{reason}; the launch could not be ended, and remains for recovery: {unended}"
            ),
        )
    }

    /// Ends a launch whose guest image lacks control features that NVX needs.
    ///
    /// An older image would boot and answer, but it would silently ignore host path mappings and
    /// cancellation requests, so the VM must not run at all.
    fn reject_guest(&self, starting: Starting<'_>, missing: &[&str]) -> Error {
        let reason = format!(
            "the guest image lacks control features that the openvmm backend needs ({}); use an \
             NVX release or build that includes them, for example through {}",
            missing.join(", "),
            Artifacts::ENV_DIR
        );
        self.abort_start(starting, ErrorCode::GuestFeaturesMissing, &reason)
    }

    /// Maps a control-session failure onto an instance-facing error and message.
    fn session_error(&self, error: SessionError, control_timeout: Duration) -> Error {
        match error {
            SessionError::ProcessExited => Error::new(
                ErrorCode::SandboxExited,
                format!("sandbox {} stopped unexpectedly", self.name),
            ),
            SessionError::TimedOut => Error::timed_out(format!(
                "sandbox {} did not accept a control session within {} s; another exec may \
                 still be running",
                self.name,
                control_timeout.as_secs()
            )),
            other => Error::new(
                session_code(&other),
                format!(
                    "the control session with sandbox {} failed: {other}",
                    self.name
                ),
            )
            .with_source(other),
        }
    }

    /// Starts `workload` in the instance's running VM, `running`, without the instance's lock.
    ///
    /// It waits up to `control_timeout` for the control session, which the guest grants one
    /// execution at a time. [`Execution::pump`] then waits for the outcome up to the workload's
    /// own timeout plus `response_grace`, and without a limit when the workload has no timeout.
    /// Fails with [`ErrorKind::Exited`], and the code [`ErrorCode::SandboxExited`], when the VM is
    /// gone, and with [`ErrorKind::Unsupported`] when `control_timeout` is zero or either
    /// duration is longer than [`MAX_TIMEOUT`](crate::host::launch::MAX_TIMEOUT). Fails as reading the
    /// capability failed when [`find`](LockedInstance::find) could not read it.
    pub fn exec(
        &self,
        running: &Running,
        workload: &Workload<'_>,
        control_timeout: Duration,
        response_grace: Duration,
    ) -> Result<Execution> {
        check_timeout("control timeout", control_timeout, true)?;
        check_timeout("response grace", response_grace, false)?;
        let capability = running.capability()?;
        let deadline = Instant::now() + control_timeout;
        let process = running.vmm.record().process();
        let mut session = session::connect(
            running.vmm.endpoint(),
            process.pid,
            process.start_time,
            deadline,
        )
        .and_then(|transport| ControlSession::attach(transport, capability, deadline))
        .map_err(|error| self.session_error(error, control_timeout))?;
        let request_id = session
            .start_exec(workload, deadline)
            .map_err(|error| self.session_error(error, control_timeout))?;
        let timeout_ms = workload.timeout_ms;
        let response_deadline = (timeout_ms > 0)
            .then(|| Instant::now() + Duration::from_millis(timeout_ms.into()) + response_grace);
        Ok(Execution {
            session,
            request_id,
            deadline: response_deadline,
            control_timeout,
        })
    }
}

#[cfg(test)]
mod tests;
