//! The restore sequence of a clone, from its launch, a [`Restoring`], to its sandbox's start,
//! through the adapter's framing of the clone's control session, a [`CloneControl`]:
//! [`Restoring::activate`] activates the clone, [`Activated::start`] starts its sandbox, and
//! [`Started::commit`] commits its launch, as the documentation of [`Restoring::activate`]
//! describes.

use std::fmt;
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use super::launch::Restoring;
use crate::activation::{ActivationRecord, MAX_RECORD_BYTES, ReadyActivation};
use crate::error::{Error, Result};
use crate::host::platform::{ReadyListener, Transport};
use crate::host::sandbox::distro::CheckedDistro;
use crate::host::session::{self, SessionError};
use crate::host::store::Recovery;
use crate::host::store::regular::{open_regular, read_at_most};
use crate::host::supervisor::{Starting, Vmm};
use crate::protocol::CAPABILITY_LEN;

/// The restore-ready event that OpenVMM writes to a restore's `--restore-ready-path` once the
/// restored VM has started, which [`Restoring::activate`] waits for.
pub const RESTORE_READY_EVENT: &[u8] = b"OPENVMM_RESTORE_READY_V1\n";

/// The default restore-to-Ready deadline of a clone's restore sequence: how long after the clone's
/// launch its Ready may come.
///
/// It covers OpenVMM's restore, the guest's repair, the host's attachment, and the activation of
/// the clone's root, which assembles it as a cold boot does, with a wide margin on the CI runs of
/// the slowest backend, WHP, whose restores ran within a third of a second, and whose 25 VMs of the
/// test adapter, at once, assembled their layered roots, applied their mapping tables, and started
/// within 19 seconds in all. It is the test adapter's deadline of a cold launch.
pub const RESTORE_TO_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// The default Ready-to-Start deadline of a clone's restore sequence: how long after the check of
/// the clone's Ready its sandbox may start, which takes a few of the guest's answers, as the CI
/// runs of [`RESTORE_TO_READY_TIMEOUT`] show. It is the test adapter's deadline of a session's
/// answers.
pub const READY_TO_START_TIMEOUT: Duration = Duration::from_secs(30);

/// The most sessions in which [`Restoring::activate`] sends a clone's Activate: its first session,
/// and those that replace a session that the adapter lost.
pub const MAX_ACTIVATION_SESSIONS: usize = 3;

/// How often the wait for the restore-ready event checks whether the VMM still runs.
const EXIT_CHECK_INTERVAL: Duration = Duration::from_millis(50);

/// How long the sequence waits before a session that replaces a lost one, so that the VMM's
/// control console notices the end of the lost session, which frees the console's one attachment.
const SESSION_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// The deadlines of a clone's restore sequence, each a duration from the step at which it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestoreDeadlines {
    /// The restore-to-Ready deadline: how long after the clone's launch its Ready may come, which
    /// bounds the wait for the restore-ready event, the connections to the clone's control
    /// endpoint, the adapter's attachments, and the Activates.
    pub ready: Duration,
    /// The Ready-to-Start deadline: how long after the check of the clone's Ready its sandbox may
    /// start.
    pub start: Duration,
}

impl Default for RestoreDeadlines {
    /// Returns the default deadlines: [`RESTORE_TO_READY_TIMEOUT`] and [`READY_TO_START_TIMEOUT`].
    fn default() -> Self {
        Self {
            ready: RESTORE_TO_READY_TIMEOUT,
            start: READY_TO_START_TIMEOUT,
        }
    }
}

/// The adapter's framing of a clone's control session, through which the clone's restore
/// sequence, [`Restoring::activate`], activates the clone and starts its sandbox, over the
/// session of [`nvxs`](crate::host::nvxs), say.
pub trait CloneControl {
    /// A session of the adapter's framing over one connection to the clone's control endpoint.
    type Session;
    /// A failure of the adapter's framing or of its session.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Attaches a session of the adapter's framing over `transport`, a fresh connection to the
    /// clone's control endpoint, with the launch's `capability`, and waits until `deadline` while
    /// the session's broker waits for the guest, as
    /// [`HostConnection::attach`](crate::host::nvxs::HostConnection::attach) does. Sends no
    /// request.
    fn attach(
        &mut self,
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> Result<Self::Session, Self::Error>;

    /// Sends `record`, the clone's activation record in its canonical encoding, in the framing's
    /// Activate, as the first request of `session`, and waits until `deadline` for the clone's
    /// Ready. Returns the activation that Ready reports, or `None` for a Ready that reports
    /// none, once the adapter checked its own identities in Ready, such as its build, and
    /// refused a Ready that does not match them.
    fn activate(
        &mut self,
        session: &mut Self::Session,
        record: &[u8],
        deadline: Instant,
    ) -> Result<Option<ReadyActivation>, Self::Error>;

    /// Starts the clone's sandbox over `session`, once the clone is ready: sends what the
    /// framing sends before the start, such as the mappings that the record announces, then the
    /// start, and waits until `deadline` for its confirmation.
    fn start(&mut self, session: &mut Self::Session, deadline: Instant) -> Result<(), Self::Error>;

    /// Returns whether `error`, with which [`attach`](Self::attach) or
    /// [`activate`](Self::activate) failed, lost the session, as a reset of the session, the end
    /// of its stream, or a failure of its transport do, rather than another failure, such as the
    /// guest's refusal or the passing of the deadline: a clone that answers no more in a lost
    /// session may still answer in another.
    fn lost(&self, error: &Self::Error) -> bool;
}

impl<'s> Restoring<'s> {
    /// Runs the clone's restore sequence until the clone is ready, through the adapter's framing
    /// of the clone's control session, `control`, under the restore-to-Ready deadline,
    /// `deadlines.ready` after the launch, [`ClonePlan::launch`](super::ClonePlan::launch), in
    /// order:
    ///
    /// 1. Waits for OpenVMM's restore-ready event, [`RESTORE_READY_EVENT`], at the endpoint at
    ///    which the launch listens, [`ready_endpoint`](Self::ready_endpoint), and reads it as soon
    ///    as it comes: OpenVMM connects to the endpoint before the restored VM runs, writes the
    ///    event once the restored VM has started, after which, on Windows, it waits until the
    ///    host has read the event, and then closes its end, which carries nothing else. The wait
    ///    refuses a connection of another process than the VMM, and bytes that differ from the
    ///    event, reads nothing past the event, and ends once the VMM has exited.
    /// 2. Reads the clone's activation record again where the launch wrote it,
    ///    [`CLONE_ACTIVATION_NAME`](crate::host::store::CLONE_ACTIVATION_NAME) in the sandbox,
    ///    which must still be, byte for byte, the record of the launch,
    ///    [`activation`](Self::activation), in its canonical encoding: the only record that the
    ///    clone's restore sends.
    /// 3. Connects to the clone's control endpoint, [`Starting::endpoint`], which the VMM serves,
    ///    as [`session::connect`] does, only after the event: the VMM's control console bounds the
    ///    host's authentication from its connection on, which the restore may hold up until its
    ///    event.
    /// 4. Attaches the adapter's session over the connection with the launch's capability,
    ///    [`capability`](Self::capability), which the VMM authenticates, and sends the record in
    ///    the adapter's Activate, as the session's first request.
    /// 5. Checks the activation that the clone's Ready reports, after the adapter checked its own
    ///    identities in it: the transaction ID of the record's restore section, as
    ///    [`ReadyActivation::of`] returns it.
    ///
    /// Returns the [`Activated`] clone, whose Ready-to-Start deadline, `deadlines.start`, starts
    /// at the check of its Ready, and whose sandbox [`Activated::start`] starts, after which
    /// [`Started::commit`] commits the launch, as the cold boot of a sandbox commits its launch
    /// once it checked its VM, with [`Starting::commit`].
    ///
    /// # Retries
    ///
    /// A clone answers an Activate of its record, byte for byte, with the same Ready, without
    /// activating again, also in a later session once it is ready, and refuses any other Activate
    /// for good, as does the reset of its session before it is ready. So once the adapter lost the
    /// session, as [`CloneControl::lost`] tells, while it attached it or awaited the clone's
    /// Ready, the sequence waits briefly, reads the record again, connects again, and attaches a
    /// new session, in which it sends the record again, byte for byte, and checks the Ready again:
    /// a clone that was ready answers it, and a clone that took the reset of the lost session
    /// before then has ended, so that the connection fails once its VMM has exited. It does so
    /// until the restore-to-Ready deadline, which no retry extends, in at most
    /// [`MAX_ACTIVATION_SESSIONS`] sessions in all, and never after another failure: a refusal, a
    /// Ready that does not match the record, a record that changed, and the passing of the
    /// deadline are final.
    ///
    /// # Failures
    ///
    /// A step that fails ends the launch, as [`abandon`](Self::abandon) does, which terminates the
    /// VMM, and ends the launch once nothing of it runs, and fails with the step's error:
    ///
    /// - [`ErrorKind::TimedOut`](crate::ErrorKind::TimedOut) once the restore-to-Ready deadline
    ///   passed;
    /// - [`ErrorKind::Exited`](crate::ErrorKind::Exited) once the VMM exited before its event, or
    ///   before a connection;
    /// - [`ErrorKind::Protocol`](crate::ErrorKind::Protocol) for a connection to the restore-ready
    ///   endpoint of another process than the VMM, bytes there that differ from the event, a
    ///   control endpoint that another process serves, a failure of the adapter's framing, a
    ///   session that it lost in the last of the sessions, and a Ready that reports another
    ///   activation than the record's, or none;
    /// - [`ErrorKind::Io`](crate::ErrorKind::Io) if the endpoints fail otherwise, and if the
    ///   sandbox's activation record cannot be read, or is no longer the launch's;
    /// - [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a deadline beyond the
    ///   host's clock.
    ///
    /// The error says so if the launch could not be ended, whose journal then remains for
    /// [`LockedStore::recover`](crate::host::store::LockedStore::recover), and the lease with it,
    /// as [Crashes](crate::host::supervisor#crashes) describes.
    pub fn activate<C: CloneControl>(
        self,
        control: &mut C,
        deadlines: RestoreDeadlines,
    ) -> Result<Activated<'s, C::Session>> {
        let Self {
            starting,
            launched,
            ready: _,
            mut listener,
            image,
            activation,
            record,
            capability,
        } = self;
        let Some(deadline) = launched.checked_add(deadlines.ready) else {
            let error = beyond_clock("restore-to-Ready", deadlines.ready);
            return Err(abandoned(starting, error));
        };
        if let Err(error) = wait_event(&mut listener, &starting, deadline, deadlines.ready) {
            return Err(abandoned(starting, error));
        }
        // OpenVMM sends one event, and the endpoint takes nothing more.
        drop(listener);
        let activation_session = ActivationSession {
            capability: &capability,
            record: &record,
            encoded: activation.encode(),
            expected: ReadyActivation::of(&activation),
            deadline,
            timeout: deadlines.ready,
        };
        let mut sessions = 0;
        let session = loop {
            sessions += 1;
            match activation_session.run(control, &starting) {
                Ok(session) => break session,
                Err(Attempt::Lost(_)) if sessions < MAX_ACTIVATION_SESSIONS => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    thread::sleep(SESSION_RETRY_INTERVAL.min(left));
                }
                Err(Attempt::Lost(error) | Attempt::Failed(error)) => {
                    return Err(abandoned(starting, error));
                }
            }
        };
        let Some(start) = Instant::now().checked_add(deadlines.start) else {
            drop(session);
            let error = beyond_clock("Ready-to-Start", deadlines.start);
            return Err(abandoned(starting, error));
        };
        Ok(Activated {
            session,
            starting,
            deadline: start,
            image,
            activation,
        })
    }
}

/// A clone that its restore sequence activated, as [`Restoring::activate`] returns it: its guest
/// answered the Activate of its record with a Ready that reports the record's activation, and
/// awaits the start of its sandbox, which [`start`](Self::start) asks for, by the Ready-to-Start
/// deadline, [`deadline`](Self::deadline).
///
/// It holds the adapter's session in which the clone answered, and the launch, uncommitted,
/// through which it borrows the sandbox's locked store exclusively, as [`Starting`] does.
/// Dropping it abandons the launch, as [`abandon`](Self::abandon) does, ignoring failures; a crash
/// of the caller leaves the launch's journal instead, for
/// [`LockedStore::recover`](crate::host::store::LockedStore::recover).
pub struct Activated<'s, S> {
    /// The adapter's session, in which the clone answered its Activate.
    session: S,
    /// The launch, whose journal records the VMM.
    starting: Starting<'s>,
    /// The Ready-to-Start deadline.
    deadline: Instant,
    /// The clone's image, open for reading, which the plan checked.
    image: CheckedDistro,
    /// The clone's activation record.
    activation: ActivationRecord,
}

impl<'s, S> Activated<'s, S> {
    /// Returns the launch, whose VMM runs, and whose journal records its identity.
    pub fn starting(&self) -> &Starting<'s> {
        &self.starting
    }

    /// Returns the adapter's session, in which the clone answered its Activate.
    pub fn session(&mut self) -> &mut S {
        &mut self.session
    }

    /// Returns the Ready-to-Start deadline, by which the clone's sandbox must start.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Returns the clone's activation record, with which the clone is ready.
    pub fn activation(&self) -> &ActivationRecord {
        &self.activation
    }

    /// Returns the clone's image, open for reading, which the plan checked.
    pub fn image(&self) -> &CheckedDistro {
        &self.image
    }

    /// Starts the clone's sandbox through the adapter's framing, `control`, in the session in
    /// which the clone answered, by the Ready-to-Start deadline, and returns the [`Started`]
    /// clone, whose launch the caller commits.
    ///
    /// The start is not sent again: a guest that started its sandbox refuses another start, so a
    /// session that fails once the start was sent leaves the start unknown. A failure ends the
    /// launch, as [`abandon`](Self::abandon) does, and fails with
    /// [`ErrorKind::TimedOut`](crate::ErrorKind::TimedOut) once the deadline passed, and with
    /// [`ErrorKind::Protocol`](crate::ErrorKind::Protocol) before, and says so if the launch
    /// could not be ended, as [`Restoring::activate`] does.
    pub fn start<C: CloneControl<Session = S>>(self, control: &mut C) -> Result<Started<'s, S>> {
        let Self {
            mut session,
            starting,
            deadline,
            image,
            activation,
        } = self;
        match control.start(&mut session, deadline) {
            Ok(()) => Ok(Started {
                session,
                starting,
                image,
                activation,
            }),
            Err(source) => {
                drop(session);
                let error = if Instant::now() >= deadline {
                    Error::timed_out(
                        "the clone's sandbox did not start by its Ready-to-Start deadline",
                    )
                } else {
                    Error::protocol("cannot start the clone's sandbox")
                };
                Err(abandoned(starting, error.with_source(source)))
            }
        }
    }

    /// Ends the launch, as [`Starting::abandon`] does, which terminates its VMM and ends the
    /// launch once nothing of it runs, once the adapter's session has ended.
    pub fn abandon(self) -> Result<Recovery> {
        let Self {
            session, starting, ..
        } = self;
        drop(session);
        starting.abandon()
    }
}

impl<S> fmt::Debug for Activated<'_, S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Activated")
            .field("starting", &self.starting)
            .field("deadline", &self.deadline)
            .field("image", &self.image)
            .field("activation", &self.activation)
            .finish_non_exhaustive()
    }
}

/// A clone whose sandbox started, as [`Activated::start`] returns it, whose launch the caller
/// [commits](Self::commit), once it checked anything else that it requires of a launch, or
/// [abandons](Self::abandon).
///
/// It holds the adapter's session, and the launch, as [`Activated`] does. Dropping it abandons the
/// launch, as [`abandon`](Self::abandon) does, ignoring failures; a crash of the caller leaves the
/// launch's journal instead, for
/// [`LockedStore::recover`](crate::host::store::LockedStore::recover).
pub struct Started<'s, S> {
    /// The adapter's session, in which the clone's sandbox started.
    session: S,
    /// The launch, whose journal records the VMM.
    starting: Starting<'s>,
    /// The clone's image, open for reading, which the plan checked.
    image: CheckedDistro,
    /// The clone's activation record.
    activation: ActivationRecord,
}

impl<'s, S> Started<'s, S> {
    /// Returns the launch, whose VMM runs, and whose journal records its identity.
    pub fn starting(&self) -> &Starting<'s> {
        &self.starting
    }

    /// Returns the adapter's session, in which the clone's sandbox started.
    pub fn session(&mut self) -> &mut S {
        &mut self.session
    }

    /// Returns the clone's activation record, with which the clone is ready.
    pub fn activation(&self) -> &ActivationRecord {
        &self.activation
    }

    /// Returns the clone's image, open for reading, which the plan checked.
    pub fn image(&self) -> &CheckedDistro {
        &self.image
    }

    /// Commits the launch, as [`Starting::commit`] does, which writes its runtime record, and
    /// returns the VM, with the adapter's session. The clone's image closes.
    ///
    /// Fails as [`Starting::commit`] fails, which ends the VMM.
    pub fn commit(self) -> Result<(Vmm, S)> {
        let Self {
            session, starting, ..
        } = self;
        let vmm = starting.commit()?;
        Ok((vmm, session))
    }

    /// Ends the launch, as [`Activated::abandon`] does.
    pub fn abandon(self) -> Result<Recovery> {
        let Self {
            session, starting, ..
        } = self;
        drop(session);
        starting.abandon()
    }
}

impl<S> fmt::Debug for Started<'_, S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Started")
            .field("starting", &self.starting)
            .field("image", &self.image)
            .field("activation", &self.activation)
            .finish_non_exhaustive()
    }
}

/// What every session of a clone's activation takes: the launch's capability, the sandbox's
/// activation record, and the activation that its Ready must report, by the restore-to-Ready
/// deadline.
struct ActivationSession<'a> {
    /// The capability of the launch.
    capability: &'a [u8; CAPABILITY_LEN],
    /// The path of the activation record in the sandbox.
    record: &'a Path,
    /// The canonical encoding of the launch's activation record.
    encoded: Vec<u8>,
    /// The activation that the clone's Ready must report.
    expected: ReadyActivation,
    /// The restore-to-Ready deadline.
    deadline: Instant,
    /// The restore-to-Ready deadline's duration from the launch.
    timeout: Duration,
}

/// How a session of a clone's activation failed.
enum Attempt {
    /// The adapter lost the session, after which another session may find the clone ready.
    Lost(Error),
    /// Any other failure, which is final.
    Failed(Error),
}

impl ActivationSession<'_> {
    /// Activates the clone of `starting` in a new session of `control`, as
    /// [`Restoring::activate`] describes, and returns the session once the clone is ready.
    fn run<C: CloneControl>(
        &self,
        control: &mut C,
        starting: &Starting<'_>,
    ) -> std::result::Result<C::Session, Attempt> {
        let record = self.read_record().map_err(Attempt::Failed)?;
        let transport = self.connect(starting).map_err(Attempt::Failed)?;
        let attached = control.attach(transport, self.capability, self.deadline);
        let mut session =
            attached.map_err(|error| self.failed(control, error, "attach its control session"))?;
        let reported = control
            .activate(&mut session, &record, self.deadline)
            .map_err(|error| self.failed(control, error, "activate it"))?;
        self.check(reported).map_err(Attempt::Failed)?;
        Ok(session)
    }

    /// Connects to the control endpoint of the VMM of `starting`, as [`session::connect`] does,
    /// by the deadline.
    fn connect(&self, starting: &Starting<'_>) -> Result<Box<dyn Transport>> {
        let reference = starting.process().reference();
        let endpoint = starting.endpoint();
        session::connect(endpoint, reference.pid, reference.start_time, self.deadline).map_err(
            |error| {
                let message = format!("cannot connect to the clone's control endpoint {endpoint}");
                match error {
                    SessionError::TimedOut => Error::timed_out(format!(
                        "{message} within {:?} of its launch",
                        self.timeout
                    )),
                    SessionError::ProcessExited => {
                        Error::exited(format!("{message}: its VMM exited"))
                    }
                    SessionError::Io(source) => Error::io(message).with_source(source),
                    error => Error::protocol(message).with_source(error),
                }
            },
        )
    }

    /// Reads the activation record in the sandbox, which must be, byte for byte, the encoding of
    /// the launch's record, and returns it.
    fn read_record(&self) -> Result<Vec<u8>> {
        let path = self.record;
        let bytes = open_regular(path)
            .and_then(|file| read_at_most(file, MAX_RECORD_BYTES as u64, "an activation record"))
            .map_err(|error| {
                Error::io(format!(
                    "cannot read the clone's activation record {}",
                    path.display()
                ))
                .with_source(error)
            })?;
        if bytes != self.encoded {
            return Err(Error::io(format!(
                "{} no longer holds the activation record that the clone's launch wrote, the only \
                 record that its restore sends",
                path.display()
            )));
        }
        Ok(bytes)
    }

    /// Checks `reported`, the activation that the clone's Ready reports, against the record's.
    fn check(&self, reported: Option<ReadyActivation>) -> Result<()> {
        let expected = &self.expected;
        let Some(reported) = reported else {
            return Err(Error::protocol(
                "the clone's Ready reports no activation, as a VM that boots cold reports none",
            ));
        };
        if reported == *expected {
            return Ok(());
        }
        let transaction = |activation: &ReadyActivation| {
            activation
                .transaction
                .map_or_else(|| "none".to_owned(), |transaction| transaction.to_string())
        };
        Err(Error::protocol(format!(
            "the clone's Ready reports the transaction {}, not its record's transaction {}",
            transaction(&reported),
            transaction(expected)
        )))
    }

    /// Returns how a session failed with `error`, which `control` returned as it tried to `what`.
    fn failed<C: CloneControl>(&self, control: &C, error: C::Error, what: &str) -> Attempt {
        if Instant::now() >= self.deadline {
            return Attempt::Failed(
                Error::timed_out(format!(
                    "the clone was not ready within {:?} of its launch: cannot {what}",
                    self.timeout
                ))
                .with_source(error),
            );
        }
        let lost = control.lost(&error);
        let message = if lost {
            format!("the clone's control session was lost: cannot {what}")
        } else {
            format!("cannot {what}")
        };
        let error = Error::protocol(message).with_source(error);
        if lost {
            Attempt::Lost(error)
        } else {
            Attempt::Failed(error)
        }
    }
}

/// Waits until `deadline`, `timeout` after the launch, for the restore-ready event of the VMM of
/// `starting` at `listener`, as [`Restoring::activate`] describes.
fn wait_event(
    listener: &mut ReadyListener,
    starting: &Starting<'_>,
    deadline: Instant,
    timeout: Duration,
) -> Result<()> {
    let process = starting.process();
    let pid = process.reference().pid;
    // OpenVMM writes the event once, and then closes its end, so the wait reads the event's bytes
    // and nothing past them.
    let length = RESTORE_READY_EVENT.len();
    let mut received = Vec::with_capacity(length);
    while received.len() < length {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::timed_out(format!(
                "the clone's VMM sent no restore-ready event within {timeout:?} of its launch"
            )));
        }
        let before = received.len();
        let open = listener
            .receive(pid, &mut received, length, left.min(EXIT_CHECK_INTERVAL))
            .map_err(|error| {
                let message = "cannot receive the restore-ready event of the clone's VMM";
                if error.kind() == io::ErrorKind::PermissionDenied {
                    Error::protocol(message).with_source(error)
                } else {
                    Error::io(message).with_source(error)
                }
            })?;
        if !RESTORE_READY_EVENT.starts_with(&received) {
            return Err(Error::protocol(format!(
                "the clone's VMM sent {:?} at its restore-ready endpoint, which is not OpenVMM's \
                 restore-ready event",
                String::from_utf8_lossy(&received)
            )));
        }
        if received.len() == length {
            break;
        }
        if !open {
            return Err(Error::protocol(format!(
                "the clone's VMM closed its restore-ready endpoint after {} bytes of the event",
                received.len()
            )));
        }
        if received.len() == before {
            let exited = process.has_exited().map_err(|error| {
                Error::io("cannot tell whether the clone's VMM still runs").with_source(error)
            })?;
            if exited {
                return Err(Error::exited(
                    "the clone's VMM exited before its restore-ready event, as OpenVMM does once \
                     it cannot restore the snapshot",
                ));
            }
        }
    }
    Ok(())
}

/// Returns the error of the deadline `name`, `timeout` after its start, which this host's clock
/// cannot represent.
fn beyond_clock(name: &str, timeout: Duration) -> Error {
    Error::unsupported(format!(
        "the {name} deadline of a clone's restore, {timeout:?} after its start, is beyond this \
         host's clock"
    ))
}

/// Ends `starting`, a launch whose restore sequence failed with `error`, as
/// [`Starting::abandon`] does, and returns `error`, which says why the launch could not be ended
/// if it could not: its journal then remains for recovery.
fn abandoned(starting: Starting<'_>, error: Error) -> Error {
    let reason = match starting.abandon() {
        Ok(Recovery::Indeterminate(reason)) => reason,
        Ok(_) => return error,
        Err(failure) => failure.to_string(),
    };
    let code = error.code();
    let (_, message, source) = error.into_parts();
    let error = Error::new(
        code,
        format!("{message}; the launch could not be ended, and remains for recovery: {reason}"),
    );
    match source {
        Some(source) => error.with_source(source),
        None => error,
    }
}
