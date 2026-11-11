//! The blocking driver of a restore over NVX's control protocol, for NVX's own blocking callers.
//!
//! [`restore`] drives a [`RestoreMachine`] through a launched VM's restore-ready event and
//! control sessions until the guest's Ready, then sends the VM's mapping table and starts its
//! workload, and returns the live [`ControlSession`]. The restore-ready client and the control
//! server must be the VM's held process. Each session carries the same activate message: the
//! activation record and the workload's identity. A lost session is replaced, in at most
//! [`MAX_SESSIONS`] sessions in all.
//!
//! Every pending accept, read and write races the step's deadline, the caller's [`Cancel`] and
//! the VM's verified exit. The restore-ready endpoint and the control endpoint are waited for in
//! slices of at most [`POLL`], between which the restore checks both; while a control session
//! transfers, a watcher thread checks them and, once either happens, ends the session's pending
//! transfers. A failure leaves the VM's process, its outputs and its restore-ready listener to the
//! caller.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
#[cfg(any(target_os = "linux", windows))]
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", windows))]
use super::platform;
use super::process::Process;
use super::report;
use super::restore::{
    ReadyListener, RestoreAction, RestoreError, RestoreEvidence, RestoreInput, RestoreMachine,
    RestorePhase,
};
use super::session::{self, ControlSession, SessionError};
use super::transport::Transport;
use super::vm::{Capability, Launched, VmOutputs};
use crate::ErrorKind;
use crate::activation::ActivationRecord;
use crate::protocol::{self, APP_EXEC, HostMap, workload::WorkloadIdentity};

/// How long a restore may take, from the driver's start to the guest's Ready.
const READY_BUDGET: Duration = Duration::from_secs(60);
/// How long the mapping table and the workload's Start may take, from the guest's Ready.
const START_BUDGET: Duration = Duration::from_secs(30);
/// The most control sessions in which a restore sends its activate message.
const MAX_SESSIONS: usize = 3;
/// The longest that a wait goes without checking the caller's cancellation and the VM's exit,
/// and the pause after a lost session, in which the VMM's console notices the loss and an
/// exiting VM shows its exit.
const POLL: Duration = Duration::from_millis(50);
/// How long a VMM that closed its restore-ready endpoint without the event may take to exit, as
/// OpenVMM does when it cannot start its VM worker.
const CLOSED_EXIT_WAIT: Duration = Duration::from_secs(10);
/// OpenVMM's restore-ready event.
const READY_EVENT: &[u8] = b"OPENVMM_RESTORE_READY_V1\n";

/// The caller's cancellation of a restore, which the restore observes within [`POLL`] once set.
pub(crate) type Cancel = AtomicBool;

/// The workload that a restored VM runs, and the identity that it runs as.
pub(crate) struct Workload<'a> {
    /// The identity that the activate message carries, as which nvx-init sets up the workload.
    pub(crate) identity: WorkloadIdentity,
    /// The workload that Start runs.
    pub(crate) exec: protocol::Workload<'a>,
    /// The nonzero request ID of Start, which the workload's events on the returned session
    /// repeat.
    pub(crate) request_id: u64,
}

/// A failed restore. The VM's process, outputs and restore-ready listener remain the caller's.
#[derive(Debug)]
pub(crate) enum ControlRestoreError {
    /// The restore failed before the guest's Ready, with the machine's phase, evidence and
    /// source.
    Machine(RestoreError),
    /// The mapping table failed after the guest's Ready.
    Maps(SessionError),
    /// The workload's Start failed after the mapping table.
    Start(SessionError),
}

impl fmt::Display for ControlRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Machine(error) => error.fmt(formatter),
            Self::Maps(error) => write!(formatter, "the restored VM's mappings failed: {error}"),
            Self::Start(error) => write!(formatter, "the restored VM's Start failed: {error}"),
        }
    }
}

impl std::error::Error for ControlRestoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Machine(error) => Some(error),
            Self::Maps(error) | Self::Start(error) => Some(error),
        }
    }
}

/// Restores the launched VM: drives its restore to the guest's Ready, in 60 seconds from the
/// call, then sends `maps` and starts `workload`, in 30 seconds from Ready, and returns the
/// session in which the workload's events arrive, no longer subject to `cancel`.
///
/// `maps` is the mapping table that `record` announces. A failure stops nothing: the caller keeps
/// the VM's process, its outputs and its restore-ready listener, and decides what to do with
/// them.
pub(crate) fn restore(
    launched: &mut Launched,
    record: &ActivationRecord,
    workload: &Workload<'_>,
    maps: &[HostMap],
    cancel: &Cancel,
) -> Result<ControlSession, ControlRestoreError> {
    let deadline = Instant::now() + READY_BUDGET;
    let Launched {
        process,
        outputs,
        capability,
        ready,
        ..
    } = launched;
    let watch = Watch {
        process,
        cancel,
        interrupt: OnceLock::new(),
        #[cfg(any(target_os = "linux", windows))]
        connection: Mutex::new(None),
    };
    let result = thread::scope(|scope| {
        // Dropping the sender, also as a panic unwinds, ends the watcher.
        let (_running, stopped) = mpsc::channel::<()>();
        scope.spawn(|| watch.run(stopped));
        let session = activate(
            &watch,
            ready.as_mut(),
            outputs,
            capability,
            record,
            workload,
            deadline,
        )?;
        start(&watch, session, record, workload, maps)
    });
    // The watcher may have ended the session's transfers after the last of them.
    match watch.interrupted() {
        Some(interrupt) if result.is_ok() => Err(ControlRestoreError::Start(interrupt.into())),
        _ => result,
    }
}

/// Drives the machine to the guest's Ready, and returns the session in which it came.
fn activate(
    watch: &Watch<'_>,
    mut listener: Option<&mut ReadyListener>,
    outputs: &VmOutputs,
    capability: &Capability,
    record: &ActivationRecord,
    workload: &Workload<'_>,
    deadline: Instant,
) -> Result<ControlSession, ControlRestoreError> {
    let mut machine = RestoreMachine::new(record);
    machine.input(RestoreInput::Spawned(watch.process.reference().clone()));
    let payload =
        protocol::encode_activate_payload(record, &workload.identity).unwrap_or_else(|error| {
            machine.input(RestoreInput::OperationFailed {
                phase: RestorePhase::Activate,
                evidence: RestoreEvidence::InvalidSettings,
                source: Some(Box::new(error)),
            });
            Vec::new()
        });
    let mut session = None;
    let mut sessions = 0;
    loop {
        let action = machine
            .next_action()
            .expect("each step gives the machine an input");
        let (phase, result) = match action {
            RestoreAction::Fail(mut error) => {
                // The VM ran before the driver did, whatever the machine saw of it.
                error.guest_may_have_run = true;
                return Err(ControlRestoreError::Machine(error));
            }
            RestoreAction::Done => return Ok(session.expect("Ready came in a session")),
            RestoreAction::AwaitReadyEvent => (
                RestorePhase::ReadyEvent,
                ready_event(watch, listener.as_deref_mut(), deadline),
            ),
            RestoreAction::Connect => {
                session = None;
                watch.release();
                let result = connect(watch, &outputs.endpoint, deadline).and_then(|transport| {
                    sessions += 1;
                    watch.register(&*transport)?;
                    let attached =
                        ControlSession::attach(transport, capability.as_bytes(), deadline)?;
                    session = Some(attached);
                    Ok(RestoreInput::Connected)
                });
                (RestorePhase::Connect, result)
            }
            RestoreAction::SendActivate(_) => {
                let attached = session.as_mut().expect("activation follows a connection");
                let result = attached.activate(&payload, deadline);
                (RestorePhase::Activate, result.map(RestoreInput::GuestReady))
            }
        };
        machine.input(match result {
            Ok(input) => input,
            Err(error) => watch.failed(&outputs.outcome, phase, error, sessions, deadline),
        });
    }
}

/// Sends the mapping table and starts the workload in the session in which the guest was ready.
fn start(
    watch: &Watch<'_>,
    mut session: ControlSession,
    record: &ActivationRecord,
    workload: &Workload<'_>,
    maps: &[HostMap],
) -> Result<ControlSession, ControlRestoreError> {
    let deadline = Instant::now() + START_BUDGET;
    let announced = record.configuration().mappings;
    if maps.len() != announced {
        return Err(ControlRestoreError::Maps(SessionError::Protocol(format!(
            "the activation record announces {announced} mappings, not {}",
            maps.len()
        ))));
    }
    if !maps.is_empty() {
        watch
            .check()
            .and_then(|()| session.map_host_paths(maps, deadline))
            .map_err(|error| ControlRestoreError::Maps(watch.cause(error, deadline)))?;
    }
    let payload = protocol::encode_exec_payload(&workload.exec)
        .map_err(|error| ControlRestoreError::Start(error.into()))?;
    watch
        .check()
        .and_then(|()| session.send_request(APP_EXEC, workload.request_id, &payload, deadline))
        .map_err(|error| ControlRestoreError::Start(watch.cause(error, deadline)))?;
    Ok(session)
}

/// Receives the restore-ready event from the VM's held process.
fn ready_event(
    watch: &Watch<'_>,
    listener: Option<&mut ReadyListener>,
    deadline: Instant,
) -> Result<RestoreInput, SessionError> {
    let listener = listener.ok_or_else(|| {
        SessionError::Protocol("the launch has no restore-ready listener".to_owned())
    })?;
    let mut event = Vec::new();
    while event.len() < READY_EVENT.len() {
        let slice = watch.slice(deadline)?;
        let open = listener
            .receive(watch.process, &mut event, READY_EVENT.len() + 1, slice)
            .map_err(|error| match error.kind() {
                ErrorKind::Exited => SessionError::ProcessExited,
                ErrorKind::Protocol => SessionError::Refused(io::Error::other(error)),
                _ => SessionError::Io(io::Error::other(error)),
            })?;
        if !open {
            watch.pause(deadline.min(Instant::now() + CLOSED_EXIT_WAIT))?;
            break;
        }
    }
    Ok(RestoreInput::ReadyEvent(event))
}

/// Connects to the VM's control endpoint, which its held process must serve.
fn connect(
    watch: &Watch<'_>,
    endpoint: &str,
    deadline: Instant,
) -> Result<Box<dyn Transport>, SessionError> {
    let process = watch.process.reference();
    loop {
        let slice = watch.slice(deadline)?;
        match session::connect(
            endpoint,
            process.pid,
            process.start_time,
            Instant::now() + slice,
        ) {
            Err(SessionError::TimedOut) => {}
            result => return result,
        }
    }
}

/// Why a restore stopped waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Interrupt {
    /// The caller cancelled the restore.
    Cancelled,
    /// The VM's held process exited.
    Exited,
}

impl From<Interrupt> for SessionError {
    fn from(interrupt: Interrupt) -> Self {
        match interrupt {
            Interrupt::Cancelled => Self::Io(io::Error::new(
                io::ErrorKind::Interrupted,
                "the caller cancelled the restore",
            )),
            Interrupt::Exited => Self::ProcessExited,
        }
    }
}

/// Races a restore's pending operations against the caller's cancellation and the VM's exit.
struct Watch<'a> {
    /// The VM's held process.
    process: &'a Process,
    /// The caller's cancellation.
    cancel: &'a Cancel,
    /// The first interrupt observed, which stands.
    interrupt: OnceLock<Interrupt>,
    /// A duplicate of the current session's connection, whose transfers an interrupt ends.
    #[cfg(any(target_os = "linux", windows))]
    connection: Mutex<Option<platform::TransportHandle>>,
}

impl Watch<'_> {
    /// Returns the restore's interrupt, once the caller cancelled it or its VM verifiably exited.
    fn poll(&self) -> Option<Interrupt> {
        if self.cancel.load(Ordering::Acquire) {
            let _ = self.interrupt.set(Interrupt::Cancelled);
        } else if self.process.has_exited().unwrap_or(false) {
            let _ = self.interrupt.set(Interrupt::Exited);
        }
        self.interrupted()
    }

    /// Returns the interrupt observed so far, without checking for another.
    fn interrupted(&self) -> Option<Interrupt> {
        self.interrupt.get().copied()
    }

    /// Fails once the restore was interrupted.
    fn check(&self) -> Result<(), SessionError> {
        self.poll()
            .map_or(Ok(()), |interrupt| Err(interrupt.into()))
    }

    /// Returns the interrupt that ended a step with `error`, if any, after giving a lost session's
    /// VM until [`POLL`] or `deadline` to show its exit, since a VM that exits ends its sessions
    /// before its exit shows.
    fn interrupt_behind(&self, error: &SessionError, deadline: Instant) -> Option<Interrupt> {
        if session_lost(error) {
            let _ = self.pause(deadline.min(Instant::now() + POLL));
        }
        self.poll()
    }

    /// Returns the interrupt that ended a step with `error` as its failure, or `error`.
    fn cause(&self, error: SessionError, deadline: Instant) -> SessionError {
        self.interrupt_behind(&error, deadline)
            .map_or(error, SessionError::from)
    }

    /// Returns how long the next wait until `deadline` may take, or why the step ends.
    fn slice(&self, deadline: Instant) -> Result<Duration, SessionError> {
        self.check()?;
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .map(|left| left.min(POLL))
            .ok_or(SessionError::TimedOut)
    }

    /// Waits until `until`, unless the restore is interrupted first.
    fn pause(&self, until: Instant) -> Result<(), SessionError> {
        loop {
            match self.slice(until) {
                Ok(slice) => thread::sleep(slice),
                Err(SessionError::TimedOut) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    /// Lets an interrupt end `transport`'s pending transfers, in place of the previous session's.
    fn register(&self, transport: &dyn Transport) -> Result<(), SessionError> {
        #[cfg(any(target_os = "linux", windows))]
        {
            let handle = transport.try_clone_handle()?;
            *self
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = handle;
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = transport;
        Ok(())
    }

    /// Forgets the previous session's connection, so that it closes with its session.
    fn release(&self) {
        #[cfg(any(target_os = "linux", windows))]
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    /// Checks every [`POLL`] until `stopped` disconnects, and once the restore is interrupted, ends
    /// the session's pending transfers, at each check, since a Windows transfer that starts later
    /// would not end.
    fn run(&self, stopped: mpsc::Receiver<()>) {
        while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(POLL) {
            #[cfg(any(target_os = "linux", windows))]
            if self.poll().is_some()
                && let Some(connection) = &*self
                    .connection
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            {
                // A failure leaves the transfer to its deadline.
                let _ = platform::interrupt_transfers(connection);
            }
        }
    }

    /// Turns a step's failure into the machine's input: the caller's cancellation, the VM's
    /// verified exit with its outcome report, a lost session that another may replace, or a final
    /// failure with its evidence.
    fn failed(
        &self,
        outcome: &Path,
        phase: RestorePhase,
        error: SessionError,
        sessions: usize,
        deadline: Instant,
    ) -> RestoreInput {
        let lost = phase != RestorePhase::ReadyEvent && session_lost(&error);
        match (self.interrupt_behind(&error, deadline), error) {
            (Some(Interrupt::Cancelled), _) => RestoreInput::Cancelled,
            (Some(Interrupt::Exited), _) | (None, SessionError::ProcessExited) => {
                match report::read(outcome) {
                    Ok(report) => RestoreInput::Exited(report),
                    Err(error) => RestoreInput::OperationFailed {
                        phase,
                        evidence: RestoreEvidence::ReportUnreadable,
                        source: Some(Box::new(error)),
                    },
                }
            }
            (None, _) if lost && sessions < MAX_SESSIONS => RestoreInput::SessionLost,
            (None, SessionError::TimedOut) => RestoreInput::Expired,
            (None, error) => RestoreInput::OperationFailed {
                phase,
                evidence: match error {
                    SessionError::Refused(_) => RestoreEvidence::PeerMismatch,
                    SessionError::Protocol(_) | SessionError::AuthenticationFailed => {
                        RestoreEvidence::Malformed
                    }
                    _ if lost => RestoreEvidence::Reset,
                    _ => RestoreEvidence::Io,
                },
                source: Some(Box::new(error)),
            },
        }
    }
}

/// Returns whether `error` lost its session: a reset, or the end of its stream or transport.
fn session_lost(error: &SessionError) -> bool {
    match error {
        SessionError::Closed | SessionError::Reset => true,
        SessionError::Io(error) => matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::WriteZero
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
