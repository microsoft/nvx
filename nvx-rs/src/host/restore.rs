//! Caller-driven restores and their peer-verified restore-ready endpoint.
//!
//! [`RestoreMachine`] orders activation without I/O or a clock. The caller supplies every
//! [`RestoreInput`], performs each [`RestoreAction`], and owns retries, timing and the VM process.
//! Success ends at Ready: mappings and workload startup remain the caller's protocol.
//!
//! A launch binds before spawning the VM and receives through its verified [`Process`].
//! Parse the completed bytes with
//! [`parse_ready_event`].
//! External I/O users take the native listener with `ReadyListener::into_std` on Linux or
//! Windows and check its client's process identity before reading the event.

use std::error::Error as StdError;
#[cfg(not(windows))]
use std::path::Path;
use std::time::Duration;
use std::{fmt, io};

use super::platform;
use super::process::{Process, ProcessRef};
use super::report::Report;
use super::vm::VmDir;
use crate::activation::{ActivationRecord, ReadyActivation};
use crate::protocol::restore_ready::parse_ready_event;
use crate::{Error, Result};

/// The pending part of a restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RestorePhase {
    /// Waiting for the launched VM's restore-ready event.
    ReadyEvent,
    /// Connecting and preparing the caller's verified control session.
    Connect,
    /// Sending the canonical activation record.
    Activate,
    /// Waiting for Ready to echo the activation's transaction.
    Ready,
}

/// An observed fact about a failed restore, without attribution or cleanup policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RestoreEvidence {
    /// A caller's peer check refused a process.
    PeerMismatch,
    /// A caller's I/O operation failed.
    Io,
    /// An event, response or ordering violated the restore protocol.
    Malformed,
    /// The caller could not read the VM's outcome report.
    ReportUnreadable,
    /// The caller's budget expired.
    Expired,
    /// The caller cancelled the restore.
    Cancelled,
    /// The caller verified that the VM exited before Ready.
    Exited,
}

/// A restore's failure phase, evidence and original cause.
///
/// This error neither stops the VM nor records a disposition. The caller retains its process,
/// session and storage and decides what to do with them.
#[derive(Debug)]
#[non_exhaustive]
pub struct RestoreError {
    /// The phase of the failed operation.
    pub phase: RestorePhase,
    /// What the caller or the machine observed.
    pub evidence: RestoreEvidence,
    /// The original cause, when the failed operation supplied one.
    pub source: Option<Box<dyn StdError + Send + Sync>>,
}

impl fmt::Display for RestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "restore failed during {:?}: {:?}",
            self.phase, self.evidence
        )
    }
}

impl StdError for RestoreError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

/// A caller's result or observation for a restore's pending step.
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreInput {
    /// The caller launched and holds the VM named by this reference.
    Spawned(ProcessRef),
    /// Completed bytes read from the caller's peer-verified restore-ready listener.
    ReadyEvent(Vec<u8>),
    /// The caller prepared a new, peer-verified control session.
    Connected,
    /// The activation write completed; Ready is still pending.
    ///
    /// A caller that combines the write and Ready read may supply GuestReady directly instead.
    ActivateSent,
    /// The control session was lost; the caller decides whether to reconnect.
    SessionLost,
    /// The activation reported by the guest's Ready.
    GuestReady(ReadyActivation),
    /// A final operation failure, preserving its explicit phase, evidence and original source.
    OperationFailed {
        /// The operation's phase, including a write failure after taking SendActivate.
        phase: RestorePhase,
        /// What the caller observed.
        evidence: RestoreEvidence,
        /// The operation's original cause.
        source: Option<Box<dyn StdError + Send + Sync>>,
    },
    /// A verified VM exit and an already-read outcome report, when available.
    Exited(Option<Report>),
    /// The caller's budget for the pending step expired.
    Expired,
    /// The caller gave up.
    Cancelled,
}

/// An operation for the caller, or the terminal result of a restore.
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreAction {
    /// Receive the complete, peer-verified restore-ready event.
    AwaitReadyEvent,
    /// Prepare a new, peer-verified control session.
    Connect,
    /// Send these canonical record bytes as the first request of the session and await Ready.
    SendActivate(Vec<u8>),
    /// Ready echoed the expected transaction; no mappings or workload Start were sent.
    Done,
    /// The restore failed; its VM process remains the caller's.
    Fail(RestoreError),
}

/// Progress that distinguishes queued actions from the caller's pending operations.
#[derive(Clone, Copy, Debug)]
enum MachineState {
    /// Awaiting the caller's spawn notification.
    Spawn,
    /// Awaiting the restore-ready event.
    Event,
    /// Awaiting a verified control session.
    Connect,
    /// The activation has not yet been offered to the caller.
    ActivateQueued,
    /// The activation write or combined write-and-Ready operation is pending.
    Activate,
    /// The write completed and Ready is pending.
    Ready,
    /// A terminal result was reached.
    Finished,
}

impl MachineState {
    /// Returns the pending phase, or None after the terminal result.
    fn phase(self) -> Option<RestorePhase> {
        match self {
            Self::Spawn | Self::Event => Some(RestorePhase::ReadyEvent),
            Self::Connect => Some(RestorePhase::Connect),
            Self::ActivateQueued | Self::Activate => Some(RestorePhase::Activate),
            Self::Ready => Some(RestorePhase::Ready),
            Self::Finished => None,
        }
    }
}

/// The ordered rules of one restore, without I/O, a clock or a retry budget.
///
/// Feed [`RestoreInput::Spawned`] after launching the VM, then perform the offered actions and
/// supply their results. Every session receives the same canonical activation bytes. A lost
/// session offers Connect again; the caller may instead cancel or report an expired budget.
/// Terminal success or failure absorbs later inputs and is offered only once.
#[derive(Debug)]
pub struct RestoreMachine {
    /// The one canonical record sent in every session.
    encoded: Vec<u8>,
    /// The activation that a successful Ready must report.
    expected: ReadyActivation,
    /// The pending operation or terminal state.
    state: MachineState,
    /// The next action, which an input may invalidate before it is taken.
    action: Option<RestoreAction>,
}

impl RestoreMachine {
    /// Latches the record's canonical encoding and expected transaction.
    ///
    /// A cold record without a restore transaction queues Fail immediately. A valid restore
    /// waits for Spawned; it never reads a record file or changes its record between sessions.
    pub fn new(record: &ActivationRecord) -> Self {
        let mut machine = Self {
            encoded: record.encode(),
            expected: ReadyActivation::of(record),
            state: MachineState::Spawn,
            action: None,
        };
        if record.restore().is_none() {
            machine.fail(
                RestorePhase::ReadyEvent,
                RestoreEvidence::Malformed,
                Some(Box::new(Error::unsupported(
                    "a restore needs an activation record with a transaction",
                ))),
            );
        }
        machine
    }

    /// Applies an observation, replacing any action invalidated by failure or session loss.
    ///
    /// OperationFailed keeps the caller's phase even when it differs from the machine's pending
    /// phase. Expiry, cancellation and verified exit fail every pending phase. An unexpected
    /// progress input fails explicitly; inputs after a terminal result are ignored.
    pub fn input(&mut self, input: RestoreInput) {
        let Some(phase) = self.state.phase() else {
            return;
        };
        match (self.state, input) {
            (MachineState::Spawn, RestoreInput::Spawned(_)) => {
                self.state = MachineState::Event;
                self.action = Some(RestoreAction::AwaitReadyEvent);
            }
            (MachineState::Event, RestoreInput::ReadyEvent(bytes)) => {
                if let Err(source) = parse_ready_event(&bytes) {
                    self.fail(phase, RestoreEvidence::Malformed, Some(Box::new(source)));
                } else {
                    self.state = MachineState::Connect;
                    self.action = Some(RestoreAction::Connect);
                }
            }
            (MachineState::Connect, RestoreInput::Connected) => {
                self.state = MachineState::ActivateQueued;
                self.action = Some(RestoreAction::SendActivate(self.encoded.clone()));
            }
            (MachineState::Activate, RestoreInput::ActivateSent) => {
                self.state = MachineState::Ready;
            }
            (MachineState::Activate | MachineState::Ready, RestoreInput::GuestReady(reported)) => {
                if reported == self.expected {
                    self.state = MachineState::Finished;
                    self.action = Some(RestoreAction::Done);
                } else {
                    self.fail(
                        RestorePhase::Ready,
                        RestoreEvidence::Malformed,
                        Some(Box::new(Error::protocol(format!(
                            "Ready echoed transaction {:?}, not {:?}",
                            reported.transaction, self.expected.transaction
                        )))),
                    );
                }
            }
            (
                MachineState::Connect
                | MachineState::ActivateQueued
                | MachineState::Activate
                | MachineState::Ready,
                RestoreInput::SessionLost,
            ) => {
                self.state = MachineState::Connect;
                self.action = Some(RestoreAction::Connect);
            }
            (
                _,
                RestoreInput::OperationFailed {
                    phase,
                    evidence,
                    source,
                },
            ) => {
                self.fail(phase, evidence, source);
            }
            (_, RestoreInput::Expired) => self.fail(phase, RestoreEvidence::Expired, None),
            (_, RestoreInput::Cancelled) => self.fail(phase, RestoreEvidence::Cancelled, None),
            (_, RestoreInput::Exited(_)) => self.fail(phase, RestoreEvidence::Exited, None),
            _ => self.fail(
                phase,
                RestoreEvidence::Malformed,
                Some(Box::new(Error::protocol(
                    "unexpected progress input during restore",
                ))),
            ),
        }
    }

    /// Takes the next action once, or returns None while awaiting a caller input.
    ///
    /// Taking SendActivate starts the pending Activate phase. ActivateSent advances to the Ready
    /// wait; a caller that combines the write and Ready read can report GuestReady directly.
    pub fn next_action(&mut self) -> Option<RestoreAction> {
        let action = self.action.take()?;
        if matches!(&action, RestoreAction::SendActivate(_)) {
            self.state = MachineState::Activate;
        }
        Some(action)
    }

    /// Queues the terminal failure and invalidates any pending action without cleanup.
    fn fail(
        &mut self,
        phase: RestorePhase,
        evidence: RestoreEvidence,
        source: Option<Box<dyn StdError + Send + Sync>>,
    ) {
        self.state = MachineState::Finished;
        self.action = Some(RestoreAction::Fail(RestoreError {
            phase,
            evidence,
            source,
        }));
    }
}

/// The listener of one restore, bound before its VM starts.
///
/// The first receive fixes the expected process identity. Every receive borrows its held
/// [`Process`], never merely a PID, and refuses an exited process. Dropping the listener closes
/// its handles; on Linux, the socket's directory entry remains for the caller to remove.
#[derive(Debug)]
pub struct ReadyListener {
    /// The endpoint passed to OpenVMM's `--restore-ready-path`.
    endpoint: String,
    /// The existing platform listener and its accepted connection, if any.
    listener: platform::ReadyListener,
    /// The identity supplied by the first receive, which later receives may not change.
    process: Option<ProcessRef>,
}

impl ReadyListener {
    /// Binds `restore-ready.sock` in the private VM directory on Linux, or a fresh, first-instance
    /// owner-only pipe on Windows. The Linux socket has mode 0600, whatever the umask.
    ///
    /// Replaces a previous Linux endpoint. Fails with [`ErrorKind::Io`](crate::ErrorKind::Io)
    /// if binding fails or the directory is no longer private, and with
    /// [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) on unsupported hosts.
    #[cfg_attr(
        not(all(test, any(target_os = "linux", windows))),
        expect(dead_code, reason = "SL2.16 binds a restore's endpoint (#1103)")
    )]
    pub(crate) fn bind(directory: &VmDir) -> Result<Self> {
        let directory = VmDir::open(directory.path())?;
        let endpoint = directory.restore_ready_endpoint()?;
        #[cfg(not(windows))]
        let (_, listener) = platform::listen_ready(Path::new(&endpoint)).map_err(endpoint_error)?;
        #[cfg(windows)]
        let listener = platform::ReadyListener::bind(&endpoint).map_err(endpoint_error)?;
        Ok(Self {
            endpoint,
            listener,
            process: None,
        })
    }

    /// Returns the endpoint for OpenVMM's `--restore-ready-path`.
    #[cfg_attr(
        not(all(test, any(target_os = "linux", windows))),
        expect(dead_code, reason = "SL2.16 renders the restore endpoint (#1103)")
    )]
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Appends at most `limit` total bytes to `received`, waiting at most `timeout`.
    ///
    /// Returns `false` at end of stream, or `true` when more may arrive, including after a
    /// timeout without data. The first client must match `process`; another client, or a later
    /// receive with a different process identity, fails with
    /// [`ErrorKind::Protocol`](crate::ErrorKind::Protocol). An exited held process fails with
    /// [`ErrorKind::Exited`](crate::ErrorKind::Exited), even if its PID has been reused.
    /// Linux authenticates the original socket peer through `SO_PEERPIDFD` (Linux 6.5 or a
    /// backport), failing with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable)
    /// if the host cannot provide it, rather than falling back to a numeric PID.
    #[cfg_attr(
        not(all(test, any(target_os = "linux", windows))),
        expect(
            dead_code,
            reason = "SL4.3's blocking driver receives the restore-ready event"
        )
    )]
    pub(crate) fn receive(
        &mut self,
        process: &Process,
        received: &mut Vec<u8>,
        limit: usize,
        timeout: Duration,
    ) -> Result<bool> {
        if self
            .process
            .as_ref()
            .is_some_and(|expected| expected != process.reference())
        {
            return Err(Error::protocol(
                "the restore-ready process identity changed",
            ));
        }
        check_running(process)?;
        self.process = Some(process.reference().clone());
        #[cfg(target_os = "linux")]
        let open =
            self.listener
                .receive_verified(process.reference().pid, received, limit, timeout);
        #[cfg(not(target_os = "linux"))]
        let open = self
            .listener
            .receive(process.reference().pid, received, limit, timeout);
        // A matching numeric PID is not enough if the held process exited during acceptance.
        check_running(process)?;
        open.map_err(listener_error)
    }

    /// Transfers the same nonblocking Unix listener, without reopening or duplicating it.
    ///
    /// Call before a receive accepts a client; otherwise fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). The caller must verify each
    /// accepted stream's original peer through `SO_PEERPIDFD`, refuse an exited peer, and match
    /// its credentials against the held VM process before reading its event. Opening a pidfd
    /// from the credentials' numeric PID does not authenticate an already-reaped peer.
    /// After matching the peer, check [`Process::has_exited`] and refuse an exited VM.
    #[cfg(target_os = "linux")]
    pub fn into_std(self) -> Result<std::os::unix::net::UnixListener> {
        self.listener.into_std().map_err(transfer_error)
    }

    /// Transfers the same overlapped first pipe instance, without reopening or duplicating it.
    ///
    /// Call before a receive accepts a client; otherwise fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). The caller completes
    /// `ConnectNamedPipe`, checks its client process ID against the held VM process before
    /// reading its event, and owns completion or cancellation of its overlapped operations.
    /// After matching the PID, check [`Process::has_exited`] and refuse an exited process.
    #[cfg(windows)]
    pub fn into_std(self) -> Result<std::os::windows::io::OwnedHandle> {
        self.listener.into_std().map_err(transfer_error)
    }
}

/// Refuses a stale process handle, so a reused PID cannot pass a peer check.
fn check_running(process: &Process) -> Result<()> {
    if process
        .has_exited()
        .map_err(|error| Error::io("cannot check the restore-ready process").with_source(error))?
    {
        return Err(Error::exited("the restore-ready VM process exited"));
    }
    Ok(())
}

/// Keeps the existing clone endpoint's peer mismatch distinct from host I/O failures.
fn listener_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::PermissionDenied {
        Error::protocol("another process connected to the restore-ready endpoint")
            .with_source(error)
    } else {
        endpoint_error(error)
    }
}

/// Reports endpoint I/O separately from an unsupported host.
fn endpoint_error(error: io::Error) -> Error {
    let message = "cannot use the restore-ready endpoint";
    if error.kind() == io::ErrorKind::Unsupported {
        Error::unavailable(message).with_source(error)
    } else {
        Error::io(message).with_source(error)
    }
}

/// Reports an accepted listener that can no longer be handed over as a fresh native listener.
#[cfg(any(target_os = "linux", windows))]
fn transfer_error(error: io::Error) -> Error {
    Error::unsupported("cannot transfer a restore-ready listener after acceptance")
        .with_source(error)
}

#[cfg(test)]
mod tests;
