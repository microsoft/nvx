//! The host's documented workflows through the public API alone: a cold boot, a capture into a
//! snapshot, a restore from it, finding a VM again after the caller restarts, and a stop.
//!
//! The example only compiles: CI builds it with the `host` feature, and nothing calls its
//! functions, so it runs no VM. The `external-launch` example runs a cold boot end to end, or a
//! restore up to its restore-ready event, through a launcher of the caller's own.
//!
//! The library keeps no state. The caller creates and owns each VM's private directory, saves
//! what it needs to find a VM again, and keeps the directory and the runtime's files where they
//! are while the VM runs. It chooses its own deadlines, and speaks its own protocol with its
//! guest.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use nvx::ErrorKind;
use nvx::activation::{ActivationRecord, ReadyActivation, TransactionId};
use nvx::host::config::VmConfig;
use nvx::host::process::{Opened, Process};
use nvx::host::report;
use nvx::host::restore::{
    ReadyListener, RestoreAction, RestoreEvidence, RestoreInput, RestoreMachine, RestorePhase,
};
use nvx::host::runtime::Runtime;
use nvx::host::snapshot::{self, Snapshot};
use nvx::host::transport::{self, HostConnection, HostConnectionError};
use nvx::host::vm::{self, Launched, Prepared, Reattached, SavedVm, Stopped, VmDir, VmOutputs};
use nvx::protocol::nvxs::SessionError;

/// One byte past OpenVMM's 25-byte restore-ready event, so that a longer event fails to parse.
const READY_LIMIT: usize = 26;

/// How long a failed capture or restore waits for the exit of the VM that it ends.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a lost connection's VM has to show its exit.
const EXIT_PAUSE: Duration = Duration::from_millis(50);

fn main() {}

/// What the caller saves, durably and privately, to find its VM again after it restarts.
pub struct Saved {
    /// What [`vm::verify`] and [`vm::reattach`] take: the directory, the endpoint, the
    /// capability, the process and the runtime's identity.
    pub vm: SavedVm,
    /// The VM's output files and endpoints, whose outcome report [`vm::stop`] reads.
    pub outputs: VmOutputs,
}

/// A capture or restore that failed once its VM started, and whose VM didn't show its exit when
/// the spawn or the caller ended it. The caller takes it from the error with `downcast`, and
/// keeps the VM, to stop it again, with its directory, and a restore's view and lock of the
/// snapshot, until the VM exits, or, without the VM, until the host restarts.
#[derive(Debug)]
pub struct Unstopped {
    /// The VM, whose process may still run, or `None` after a spawn that could not confirm the
    /// exit of the process that it started, which the caller can then neither stop nor wait for.
    pub launched: Option<Launched>,
    /// Why the capture or restore failed, and why its VM didn't stop.
    pub reason: String,
}

impl fmt::Display for Unstopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl Error for Unstopped {}

/// Boots `config`, which has a `boot`, in `directory`, a new private directory, and returns the
/// running VM and what the caller saves before it relies on the VM.
///
/// The capability is secret: the library never writes it to a file, and the caller keeps it
/// private in its own records.
pub fn cold_boot(
    runtime: &Runtime,
    config: &VmConfig,
    directory: &Path,
) -> nvx::Result<(Launched, Saved)> {
    vm::create_private_dir(directory)?;
    let dir = VmDir::open(directory)?;
    // Creation checks the whole launch and creates its log, but starts nothing. A caller that
    // recovers interrupted launches records `prepared.capability()` and `prepared.outputs()`
    // here, before the spawn, and the process's reference after it.
    let prepared = vm::create(runtime, config, None, &dir)?;
    let launched = vm::spawn(prepared)?;
    let saved = Saved {
        vm: SavedVm {
            vm_dir: dir,
            endpoint: launched.outputs.endpoint.clone(),
            capability: *launched.capability.as_bytes(),
            process: launched.process.reference().clone(),
            runtime: runtime.identity.clone(),
        },
        outputs: launched.outputs.clone(),
    };
    Ok((launched, saved))
}

/// Captures a snapshot of `config`, whose boot has a `capture`, in `directory`, a new private
/// directory, moves it to `snapshot_dir`, another, which must be empty, and opens it there.
///
/// `request` asks the guest for a snapshot, in the caller's own protocol with its guest, on a
/// connection to the VM's control endpoint that only the VM's own process may serve, and returns
/// once the guest answered. The guest then captures the VM, as the guest example shows, and the
/// VM exits once the capture commits. The caller finishes the capture with the proof of that
/// exit, then moves the snapshot's three entries, and nothing else, out of the VM directory. It
/// keeps them private and immutable while it restores from them. When the request fails, or the
/// VM doesn't exit by `deadline`, the caller finishes no snapshot: it ends the VM, or keeps it in
/// an [`Unstopped`] error.
pub fn capture<R>(
    runtime: &Runtime,
    config: &VmConfig,
    directory: &Path,
    snapshot_dir: &Path,
    request: R,
    deadline: Instant,
) -> Result<Snapshot, Box<dyn Error>>
where
    R: FnOnce(&mut HostConnection, Instant) -> nvx::Result<()>,
{
    vm::create_private_dir(snapshot_dir)?;
    if fs::read_dir(snapshot_dir)?.next().is_some() {
        return Err("the snapshot's directory is not empty".into());
    }
    vm::create_private_dir(directory)?;
    let dir = VmDir::open(directory)?;
    let prepared = vm::create(runtime, config, None, &dir)?;
    // The caller may record the context with the launch, to finish the capture after a restart.
    let context = prepared
        .capture_context()
        .cloned()
        .ok_or("the configuration's boot has no capture")?;
    let launched = start(prepared)?;
    let requested = transport::connect_verified(
        &launched.outputs.endpoint,
        launched.process.reference(),
        deadline,
    )
    .and_then(|stream| HostConnection::attach(stream, launched.capability.as_bytes(), deadline))
    .and_then(|mut connection| request(&mut connection, deadline));
    if let Err(error) = requested {
        return Err(end(launched, error.into()));
    }
    let exited = match launched.process.wait(deadline) {
        Ok(Some(exited)) => exited,
        Ok(None) => {
            let error = "the capture's VM did not exit by the deadline".into();
            return Err(end(launched, error));
        }
        Err(error) => return Err(end(launched, error.into())),
    };
    snapshot::finish_capture(&dir, &context, &exited)?;
    for name in [
        snapshot::KEY_FILE,
        snapshot::DESCRIPTION_FILE,
        snapshot::OPENVMM_DIR,
    ] {
        fs::rename(dir.path().join(name), snapshot_dir.join(name))?;
    }
    Ok(Snapshot::open(snapshot_dir)?)
}

/// Restores `snapshot` with `config`, which has no `boot`, in `directory`, a new private
/// directory, and returns the running VM and its connection to the guest once the guest is Ready.
///
/// Creation hard-links the snapshot's OpenVMM files into the VM directory, a view that the
/// caller removes once the VM exited. On Linux, the caller holds its own lock of the snapshot,
/// which every caller of the snapshot takes before it opens it, through the restore-ready event
/// or the VM's verified exit, and around the view's removal. NVX takes no lock, and Windows
/// needs none. The caller holds it through this call, which ends the VM when the restore fails,
/// or fails with an [`Unstopped`] error, after which the caller keeps holding it.
///
/// `activate` sends the activation record's bytes as the first request of the caller's own
/// protocol with its guest, and returns the activation that the guest's Ready reports. The
/// caller then sends the guest its mappings and workload in that protocol.
pub fn restore<A>(
    runtime: &Runtime,
    config: &VmConfig,
    snapshot: &Snapshot,
    directory: &Path,
    transaction: TransactionId,
    activate: A,
    deadline: Instant,
) -> Result<(Launched, HostConnection), Box<dyn Error>>
where
    A: FnMut(&mut HostConnection, &[u8], Instant) -> nvx::Result<ReadyActivation>,
{
    vm::create_private_dir(directory)?;
    let dir = VmDir::open(directory)?;
    let prepared = vm::create(runtime, config, Some(snapshot), &dir)?;
    let record = prepared.activation_record(transaction)?;
    let mut launched = start(prepared)?;
    match drive(&mut launched, &record, activate, deadline) {
        Ok(connection) => Ok((launched, connection)),
        Err(error) => Err(end(launched, error)),
    }
}

/// Drives a launched restore to its guest's Ready, and returns the connection in which it came.
///
/// The restore's machine says what to do next and checks what the caller observed: the
/// restore-ready event, which only the VM's own process may send, a connection to the VM's
/// control endpoint, which only it may serve, and the guest's Ready, which must echo the record's
/// transaction. A lost connection is replaced, with the same record, until the deadline.
fn drive<A>(
    launched: &mut Launched,
    record: &ActivationRecord,
    mut activate: A,
    deadline: Instant,
) -> Result<HostConnection, Box<dyn Error>>
where
    A: FnMut(&mut HostConnection, &[u8], Instant) -> nvx::Result<ReadyActivation>,
{
    let mut machine = RestoreMachine::new(record);
    machine.input(RestoreInput::Spawned(launched.process.reference().clone()));
    let mut current = None;
    while let Some(action) = machine.next_action() {
        let input = match action {
            RestoreAction::Done => {
                return current.ok_or_else(|| "a restore that is Ready has its connection".into());
            }
            RestoreAction::Fail(error) => return Err(error.into()),
            _ if Instant::now() >= deadline => RestoreInput::Expired,
            RestoreAction::AwaitReadyEvent => {
                let listener = launched
                    .ready
                    .as_mut()
                    .ok_or("a restore has its listener")?;
                match ready_event(listener, &launched.process, deadline) {
                    Ok(Some(event)) => RestoreInput::ReadyEvent(event),
                    Ok(None) => RestoreInput::Expired,
                    Err(error) if error.kind() == ErrorKind::Exited => {
                        exited(&launched.outputs, RestorePhase::ReadyEvent)
                    }
                    Err(error) => RestoreInput::OperationFailed {
                        phase: RestorePhase::ReadyEvent,
                        evidence: if error.kind() == ErrorKind::Protocol {
                            RestoreEvidence::PeerMismatch
                        } else {
                            RestoreEvidence::Io
                        },
                        source: Some(error.into()),
                    },
                }
            }
            RestoreAction::Connect => {
                // A connection that was lost closes before another replaces it.
                current = None;
                let process = launched.process.reference();
                let endpoint = &launched.outputs.endpoint;
                let capability = launched.capability.as_bytes();
                match transport::connect_verified(endpoint, process, deadline)
                    .and_then(|stream| HostConnection::attach(stream, capability, deadline))
                {
                    Ok(connection) => {
                        current = Some(connection);
                        RestoreInput::Connected
                    }
                    Err(error) => failed(launched, RestorePhase::Connect, error),
                }
            }
            RestoreAction::SendActivate(bytes) => {
                let connection = current
                    .as_mut()
                    .ok_or("a restore activates in its connection")?;
                match activate(connection, &bytes, deadline) {
                    Ok(ready) => RestoreInput::GuestReady(ready),
                    Err(error) => failed(launched, RestorePhase::Activate, error),
                }
            }
            _ => return Err("the restore asks for an action that this caller doesn't know".into()),
        };
        machine.input(input);
    }
    Err("the restore ended without its result".into())
}

/// Receives OpenVMM's restore-ready event from the VM's own process, or returns `None` once
/// `deadline` passed first.
fn ready_event(
    listener: &mut ReadyListener,
    process: &Process,
    deadline: Instant,
) -> nvx::Result<Option<Vec<u8>>> {
    let mut event = Vec::new();
    let mut open = true;
    while open && event.len() < READY_LIMIT {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Ok(None);
        }
        open = listener.receive(process, &mut event, READY_LIMIT, timeout)?;
    }
    Ok(Some(event))
}

/// What a failure of the connection to the guest in `phase` tells the restore: the VM's exit, a
/// lost connection, which another may replace, an expired deadline, or a final failure.
///
/// A VM that exits ends its connections before its exit shows, so a lost connection's VM has a
/// moment to show its exit first.
fn failed(launched: &Launched, phase: RestorePhase, error: nvx::Error) -> RestoreInput {
    // The broker closed or reset the session, or the stream ended inside a record.
    let closed = error.source().is_some_and(|cause| {
        matches!(
            cause.downcast_ref(),
            Some(HostConnectionError::Closed | HostConnectionError::Reset(_))
        ) || matches!(cause.downcast_ref(), Some(SessionError::Truncated { .. }))
    });
    let lost = closed
        || io_cause(&error).is_some_and(|cause| {
            matches!(
                cause.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::WriteZero
            )
        });
    let gone = lost
        && launched
            .process
            .wait(Instant::now() + EXIT_PAUSE)
            .is_ok_and(|exit| exit.is_some());
    let evidence = match error.kind() {
        ErrorKind::Exited => return exited(&launched.outputs, phase),
        _ if gone => return exited(&launched.outputs, phase),
        _ if lost => return RestoreInput::SessionLost,
        ErrorKind::TimedOut => return RestoreInput::Expired,
        // `transport::connect_verified` refuses an endpoint that another process serves.
        ErrorKind::Protocol
            if io_cause(&error)
                .is_some_and(|cause| cause.kind() == io::ErrorKind::PermissionDenied) =>
        {
            RestoreEvidence::PeerMismatch
        }
        ErrorKind::Protocol => RestoreEvidence::Malformed,
        _ => RestoreEvidence::Io,
    };
    RestoreInput::OperationFailed {
        phase,
        evidence,
        source: Some(error.into()),
    }
}

/// The I/O error that caused `error`, if one did.
fn io_cause(error: &nvx::Error) -> Option<&io::Error> {
    error.source()?.downcast_ref()
}

/// The VM's exit, with the outcome report that tells what the failure is attributed to.
fn exited(outputs: &VmOutputs, phase: RestorePhase) -> RestoreInput {
    match report::read(&outputs.outcome) {
        Ok(report) => RestoreInput::Exited(report),
        Err(error) => RestoreInput::OperationFailed {
            phase,
            evidence: RestoreEvidence::ReportUnreadable,
            source: Some(error.into()),
        },
    }
}

/// Finds a VM again from what the caller saved at its launch, after the caller restarted, or
/// stops it and returns `None` if another runtime launched it.
///
/// [`vm::reattach`] checks the runtime's identity before it opens anything, then the endpoint's
/// owner, which must be the VM's own process, before it presents the capability. Its failures
/// neither signal the VM nor change the saved values, which the caller keeps to try again.
pub fn reattach(
    saved: &Saved,
    runtime: &Runtime,
    deadline: Instant,
) -> Result<Option<Reattached>, Box<dyn Error>> {
    match vm::reattach(&saved.vm, runtime, deadline) {
        Ok(reattached) => Ok(Some(reattached)),
        // The caller drives no VM of another runtime: it terminates it, through its process.
        Err(error) if error.kind() == ErrorKind::RuntimeMismatch => {
            if let Opened::Running(process) = Process::open(&saved.vm.process)? {
                stop(
                    &process,
                    &saved.outputs,
                    |_| false,
                    Duration::ZERO,
                    deadline,
                )?;
            }
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

/// Stops a VM: `request` asks its guest to end the VM in the caller's own protocol, and returns
/// whether it sent the request, which is no proof of an exit. Once `grace` passed without an
/// exit, the VM's process is terminated. Returns once the process exited, with its outcome
/// report, or fails at `deadline`, which leaves the caller its process.
pub fn stop(
    process: &Process,
    outputs: &VmOutputs,
    request: impl FnOnce(Instant) -> bool,
    grace: Duration,
    deadline: Instant,
) -> nvx::Result<Stopped> {
    vm::stop(process, outputs, request, Instant::now() + grace, deadline)
}

/// Starts a capture's or restore's VM. A spawn that fails ends the process that it started; one
/// that cannot confirm that exit fails here with an [`Unstopped`] error without the VM.
fn start(prepared: Prepared) -> Result<Launched, Box<dyn Error>> {
    vm::spawn(prepared).map_err(|error| match error.kind() {
        ErrorKind::StillRunning => Box::new(Unstopped {
            reason: format!("{error}, and the process that it started may still run"),
            launched: None,
        }) as Box<dyn Error>,
        _ => error.into(),
    })
}

/// Ends a VM that its caller gives up on after `error`, at once, and returns `error` once the
/// VM's exit is verified, or an [`Unstopped`] error that returns the VM to the caller.
fn end(launched: Launched, error: Box<dyn Error>) -> Box<dyn Error> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    match stop(
        &launched.process,
        &launched.outputs,
        |_| false,
        Duration::ZERO,
        deadline,
    ) {
        Ok(_) => error,
        Err(failure) => Box::new(Unstopped {
            reason: format!("{error}, and its VM did not stop: {failure}"),
            launched: Some(launched),
        }),
    }
}
