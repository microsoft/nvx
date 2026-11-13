//! The restore sequence of a clone, from its launch, a [`Restoring`], to its sandbox's start,
//! through the adapter's framing of the clone's control session, a [`CloneControl`]:
//! [`Restoring::activate`] activates the clone, [`Activated::start`] sends its mapping table, if
//! it has an export, then starts its sandbox, and [`Started::commit`] commits its launch, as the
//! documentation of [`Restoring::activate`] describes, while [`Activated::fail`] and
//! [`Started::fail`] fail the clone for a step of the caller's own between them.

use std::fmt;
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use super::failure::{Attribute, CloneFailure, Evidence, Fault, Progress, Stage, host_evidence};
use super::launch::Restoring;
use super::records::Tracker;
use crate::activation::{ActivationRecord, MAX_RECORD_BYTES, ReadyActivation};
use crate::error::{Error, Result};
use crate::host::platform::{ReadyListener, Transport};
use crate::host::process::ProcessRef;
use crate::host::sandbox::distro::CheckedDistro;
use crate::host::session::{self, SessionError};
use crate::host::store::Recovery;
use crate::host::store::regular::{open_regular, read_at_most};
use crate::host::supervisor::{Starting, Vmm};
use crate::protocol::{CAPABILITY_LEN, HostMap};

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
/// the clone's Ready its sandbox may start, which takes a few of the guest's answers, to the parts
/// of its mapping table and to the start, as the CI runs of [`RESTORE_TO_READY_TIMEOUT`] show.
/// It is the test adapter's deadline of a session's answers.
pub const READY_TO_START_TIMEOUT: Duration = Duration::from_secs(30);

/// The most sessions in which [`Restoring::activate`] sends a clone's Activate: its first session,
/// and those that replace a session that the adapter lost.
pub const MAX_ACTIVATION_SESSIONS: usize = 3;

/// How often the wait for the restore-ready event checks whether the VMM still runs.
const EXIT_CHECK_INTERVAL: Duration = Duration::from_millis(50);

/// How long the wait for the restore-ready event waits for the VMM to exit once it closed the
/// event's endpoint without a byte of the event, as OpenVMM does as it fails to launch its VM
/// worker, which it does after it connected the endpoint, before it reports the error and exits.
const CLOSED_EXIT_WAIT: Duration = Duration::from_secs(10);

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

    /// Sends `table`, the clone's mapping table, which its record announces, over `session`, once
    /// the clone is ready, as the framing sends a mapping table: in order, in as many requests as
    /// it takes, each once the guest applied the one before; and waits until `deadline` for the
    /// guest to apply every mapping. The restore sequence calls it only for a clone with an export,
    /// whose table holds 1 to [`MAX_MAPPINGS`](crate::activation::MAX_MAPPINGS) mappings, as
    /// [`ClonePlan::table`](super::ClonePlan::table) returns them, and never again in a session.
    ///
    /// Fails once the guest refused or failed a mapping, after which a guest starts no sandbox, as
    /// does a guest that applied some of the table, and the framing sends no more of it, and once
    /// the session failed: the sequence then ends the launch, and never starts the sandbox.
    fn map(
        &mut self,
        session: &mut Self::Session,
        table: &[HostMap],
        deadline: Instant,
    ) -> Result<(), Self::Error>;

    /// Starts the clone's sandbox over `session`, once the clone is ready, and applied its mapping
    /// table if it has one: sends the start, and waits until `deadline` for its confirmation.
    fn start(&mut self, session: &mut Self::Session, deadline: Instant) -> Result<(), Self::Error>;

    /// Returns whether `error`, with which [`attach`](Self::attach) or
    /// [`activate`](Self::activate) failed, lost the session, as a reset of the session, the end
    /// of its stream, or a failure of its transport do, rather than another failure, such as the
    /// guest's refusal or the passing of the deadline: a clone that answers no more in a lost
    /// session may still answer in another.
    fn lost(&self, error: &Self::Error) -> bool;

    /// Returns whether `error`, with which [`map`](Self::map) failed, is the guest's refusal of
    /// the clone's mapping table: its answer that it would not, or could not, apply a part of the
    /// table, or one of its mappings, to the clone's root, which the request's mappings and image
    /// decide, rather than a failure of the session or of the framing. The restore sequence
    /// attributes such a failure to the request's mappings,
    /// [`Evidence::InvalidMappings`](super::Evidence::InvalidMappings), whenever it came, and any
    /// other to what the host saw of it, as [`Activated::start`] describes.
    ///
    /// No failure is, by default: a framing whose answers do not tell the guest's refusal of a
    /// table leaves the provenance of its failures unknown.
    fn refused(&self, error: &Self::Error) -> bool {
        let _ = error;
        false
    }
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
    /// The sequence records each step that the clone reaches in its progress log, as
    /// [`CloneRecords`](super::CloneRecords) describes: the event; its acknowledgment, which the
    /// attachment of a session shows, since OpenVMM reads none of the host's input, its
    /// authentication included, until the guest's repair acknowledged a restore that gates the
    /// input; the Activate; and the Ready.
    ///
    /// Returns the [`Activated`] clone, whose Ready-to-Start deadline, `deadlines.start`, starts
    /// at the check of its Ready, and whose sandbox [`Activated::start`] starts, once the clone
    /// applied its mapping table, if it has an export, after which [`Started::commit`] commits the
    /// launch, as the cold boot of a sandbox commits its launch once it checked its VM, with
    /// [`Starting::commit`].
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
    /// VMM through its verified identity, and ends the launch once nothing of it runs, and fails
    /// with a [`CloneFailure`], which the sandbox records, at [`Stage::Restore`] until the
    /// restore-ready event and at [`Stage::Activation`] after it, with the step's error and the
    /// evidence below, unless OpenVMM's outcome report of a VMM that exited gives a status of the
    /// guest's that is evidence, as [`status_evidence`](super::status_evidence) tells, which
    /// prevails:
    ///
    /// - [`ErrorKind::TimedOut`](crate::ErrorKind::TimedOut) once the restore-to-Ready deadline
    ///   passed, [`Evidence::Timeout`];
    /// - [`ErrorKind::Exited`](crate::ErrorKind::Exited) once the VMM exited before its event, or
    ///   before a connection, and once it closed the event's endpoint without a byte of the event,
    ///   and then exited, as OpenVMM does as it fails to launch its VM worker, within ten seconds
    ///   of the close, [`Evidence::LostStatus`], and, for a VMM that exited before its
    ///   event, the error that OpenVMM reported, as its log ends with it, after the error's
    ///   message, and [`Evidence::IncompatibleHost`] for an error whose time ABI code says that
    ///   the host cannot present the template's CPU profile, TSC, or LAPIC timer, as [the
    ///   attribution of evidence](Evidence#attribution) describes;
    /// - [`ErrorKind::Protocol`](crate::ErrorKind::Protocol) for a connection to the restore-ready
    ///   endpoint of another process than the VMM, bytes there that differ from the event, an
    ///   endpoint that the VMM closed after a part of the event, or without a byte of it and then
    ///   ran on for ten seconds, a control endpoint that another process serves, a failure of the
    ///   adapter's framing, and a Ready that reports another activation than the record's, or
    ///   none, [`Evidence::Protocol`], and for a session that the adapter lost in the last of the
    ///   sessions, [`Evidence::Reset`];
    /// - [`ErrorKind::Io`](crate::ErrorKind::Io) if the endpoints fail otherwise, if the sandbox's
    ///   activation record cannot be read, or is no longer the launch's, and if the progress log
    ///   cannot be appended to, the host's evidence, as [`ClonePlan::new`](super::ClonePlan::new)
    ///   describes it;
    /// - [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a deadline beyond the
    ///   host's clock, [`Evidence::HostFailure`].
    ///
    /// The failure's teardown says whether the launch could be ended, and the error says so if it
    /// could not: its journal then remains for
    /// [`LockedStore::recover`](crate::host::store::LockedStore::recover), and the lease with it,
    /// as [Crashes](crate::host::supervisor#crashes) describes.
    pub fn activate<C: CloneControl>(
        self,
        control: &mut C,
        deadlines: RestoreDeadlines,
    ) -> Result<Activated<'s, C::Session>, CloneFailure> {
        let Self {
            starting,
            launched,
            ready: _,
            mut listener,
            image,
            activation,
            record,
            capability,
            mut tracker,
            table,
        } = self;
        let Some(deadline) = launched.checked_add(deadlines.ready) else {
            let fault = beyond_clock("restore-to-Ready", deadlines.ready);
            return Err(tracker.fail(starting, Stage::Restore, fault));
        };
        if let Err(fault) = wait_event(&mut listener, &starting, deadline, deadlines.ready) {
            return Err(tracker.fail(starting, Stage::Restore, fault));
        }
        // OpenVMM sends one event, and the endpoint takes nothing more.
        drop(listener);
        if let Err(fault) = tracker.reach(Progress::Event) {
            return Err(tracker.fail(starting, Stage::Activation, fault));
        }
        let activation_session = ActivationSession {
            capability: &capability,
            record: &record,
            encoded: activation.encode(),
            expected: ReadyActivation::of(&activation),
            deadline,
            timeout: deadlines.ready,
            since: "its launch",
        };
        let endpoint = starting.endpoint().to_owned();
        let reference = starting.process().reference().clone();
        let activated =
            activation_session.activate(control, &endpoint, &reference, Some(&mut tracker));
        let session = match activated {
            Ok(session) => session,
            Err(fault) => return Err(tracker.fail(starting, Stage::Activation, fault)),
        };
        let Some(start) = Instant::now().checked_add(deadlines.start) else {
            drop(session);
            let fault = beyond_clock("Ready-to-Start", deadlines.start);
            return Err(tracker.fail(starting, Stage::Start, fault));
        };
        Ok(Activated {
            session,
            starting,
            deadline: start,
            image,
            activation,
            tracker,
            table,
        })
    }
}

/// A clone that its restore sequence activated, as [`Restoring::activate`] returns it: its guest
/// answered the Activate of its record with a Ready that reports the record's activation, and
/// awaits its mapping table, if it has an export, and the start of its sandbox, which
/// [`start`](Self::start) sends, by the Ready-to-Start deadline, [`deadline`](Self::deadline).
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
    /// How far the clone got, and how its failures are recorded.
    tracker: Tracker,
    /// The clone's mapping table, which [`start`](Self::start) sends before the start: empty for
    /// a clone without an export.
    table: Vec<HostMap>,
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
    /// clone, whose launch the caller commits. The progress log records that the start was sent,
    /// then that it was confirmed.
    ///
    /// A clone with an export first gets its mapping table, [`ClonePlan::table`], which its
    /// record announces: [`CloneControl::map`] sends it in order, and returns once the guest
    /// applied every mapping. Only then does the progress log record that the start was sent, and
    /// [`CloneControl::start`] send it, which a clone without an export gets at once. Neither is
    /// sent again: a guest that applied some mappings starts no sandbox, and a guest that started
    /// its sandbox refuses another start, so a session that fails once the table or the start was
    /// sent leaves the clone's state unknown.
    ///
    /// A failure of either, a partial or failed mapping table included, ends the launch, as
    /// [`abandon`](Self::abandon) does, and fails with a [`CloneFailure`] at [`Stage::Start`], as
    /// [`Restoring::activate`] does. A failure of the table leaves the clone's progress at
    /// [`Progress::Ready`], since the progress log records the start only once the table is
    /// applied, so that its record tells it from a failure of the start, whose progress is
    /// [`Progress::Starting`]. The failure's evidence is the guest's refusal of the table,
    /// [`Evidence::InvalidMappings`], once the adapter's framing tells it, as
    /// [`CloneControl::refused`] does; otherwise it fails with
    /// [`ErrorKind::TimedOut`](crate::ErrorKind::TimedOut) once the deadline passed,
    /// [`Evidence::Timeout`], and with [`ErrorKind::Protocol`](crate::ErrorKind::Protocol)
    /// before, [`Evidence::Reset`] for a session that the adapter lost and
    /// [`Evidence::Protocol`] otherwise, unless OpenVMM's outcome report of a VMM that exited
    /// gives evidence that prevails; and with the host's evidence if the progress log cannot be
    /// appended to. None of these is the template's, so no failure of the table or of the start
    /// quarantines the template's generation.
    ///
    /// [`ClonePlan::table`]: super::ClonePlan::table
    pub fn start<C: CloneControl<Session = S>>(
        self,
        control: &mut C,
    ) -> Result<Started<'s, S>, CloneFailure> {
        let Self {
            mut session,
            starting,
            deadline,
            image,
            activation,
            mut tracker,
            table,
        } = self;
        match order(control, &mut session, &mut tracker, &table, deadline) {
            Ok(()) => Ok(Started {
                session,
                starting,
                image,
                activation,
                tracker,
            }),
            // The one place where the order of a clone that is ready ends its launch: a mapping
            // table that the guest did not apply in whole, a start that failed, or a progress log
            // that could not record them.
            Err(fault) => {
                drop(session);
                Err(tracker.fail(starting, Stage::Start, fault))
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

    /// Fails the clone, once it is ready, for a step of the caller's own before the start of its
    /// sandbox, such as one that prepares the host's side of the clone, with `evidence`,
    /// which attributes the failure as [the attribution of evidence](Evidence#attribution)
    /// describes, and `error`, and returns the failure: once the adapter's session has ended, it
    /// ends the launch as [`abandon`](Self::abandon) does, and records the failure at
    /// [`Stage::Start`], before its teardown and once its teardown ended, as the restore sequence
    /// records its own failures, so that a status of the guest's that is evidence, which
    /// OpenVMM's outcome report gives once the VMM has exited, prevails over `evidence`, unless
    /// `evidence` is of the template, such as a defect of the generation that the caller
    /// diagnosed, whose record then holds the status beside it.
    pub fn fail(self, evidence: Evidence, error: Error) -> CloneFailure {
        let Self {
            session,
            starting,
            tracker,
            ..
        } = self;
        drop(session);
        tracker.fail(starting, Stage::Start, Fault::new(evidence, error))
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
            .field("table", &self.table)
            .finish_non_exhaustive()
    }
}

/// Runs the order of a clone that is ready, as [`Activated::start`] describes, over `session` of
/// `control`, by `deadline`: sends the mapping table, `table`, if it has a mapping, then records in
/// `tracker` that the start was sent, sends it, and records that it was confirmed. Returns the
/// fault of the step that failed, which ends the order.
fn order<C: CloneControl>(
    control: &mut C,
    session: &mut C::Session,
    tracker: &mut Tracker,
    table: &[HostMap],
    deadline: Instant,
) -> std::result::Result<(), Fault> {
    if !table.is_empty() {
        control
            .map(session, table, deadline)
            .map_err(|source| Step::Map.fault(control, source, deadline))?;
    }
    tracker.reach(Progress::Starting)?;
    control
        .start(session, deadline)
        .map_err(|source| Step::Start.fault(control, source, deadline))?;
    tracker.reach(Progress::Started)
}

/// A step of the order of a clone that is ready, which [`Activated::start`] takes in turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The mapping table, which [`CloneControl::map`] sends.
    Map,
    /// The start, which [`CloneControl::start`] sends.
    Start,
}

impl Step {
    /// Returns the fault of the step's failure with `source`, which `control` returned, by the
    /// Ready-to-Start deadline `deadline`, as [`Activated::start`] attributes it: the guest's
    /// refusal of the mapping table is the request's mappings, whenever it came, and any other
    /// failure is what the host saw of it, the passing of the deadline, a lost session, or a
    /// broken protocol.
    fn fault<C: CloneControl>(self, control: &C, source: C::Error, deadline: Instant) -> Fault {
        let (evidence, error) = if self == Self::Map && control.refused(&source) {
            (
                Evidence::InvalidMappings,
                Error::protocol("the clone refused its mapping table"),
            )
        } else if Instant::now() >= deadline {
            (
                Evidence::Timeout,
                Error::timed_out(match self {
                    Self::Map => {
                        "the clone did not apply its mapping table by its Ready-to-Start deadline"
                    }
                    Self::Start => {
                        "the clone's sandbox did not start by its Ready-to-Start deadline"
                    }
                }),
            )
        } else if control.lost(&source) {
            (
                Evidence::Reset,
                Error::protocol(match self {
                    Self::Map => {
                        "the clone's control session was lost: the clone did not apply its \
                         mapping table"
                    }
                    Self::Start => {
                        "the clone's control session was lost: cannot start the clone's sandbox"
                    }
                }),
            )
        } else {
            (
                Evidence::Protocol,
                Error::protocol(match self {
                    Self::Map => "the clone did not apply its mapping table",
                    Self::Start => "cannot start the clone's sandbox",
                }),
            )
        };
        Fault::new(evidence, error.with_source(source))
    }
}

/// A clone whose sandbox started, as [`Activated::start`] returns it, whose launch the caller
/// [commits](Self::commit), once it checked anything else that it requires of a launch, or
/// [abandons](Self::abandon), or [fails](Self::fail), if a check of its own failed.
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
    /// How far the clone got, and how its failures are recorded.
    tracker: Tracker,
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
    /// returns the VM, with the adapter's session. The clone's image closes, and the progress log
    /// records the commit, if it can: the runtime record holds it while the VM runs.
    ///
    /// Fails as [`Starting::commit`] fails, which ends the VMM through its handle and keeps the
    /// launch's journal, with a [`CloneFailure`] at [`Stage::Commit`], whose evidence is the
    /// host's, and whose teardown is [`Teardown::Unresolved`](super::Teardown::Unresolved) until
    /// a reattach ends the launch that the journal names, and releases the lease's hold.
    pub fn commit(self) -> Result<(Vmm, S), CloneFailure> {
        let Self {
            session,
            starting,
            mut tracker,
            ..
        } = self;
        let committed = starting.commit_reporting(|error| {
            // The launch's journal remains, whether or not the failure can be recorded.
            let _ = tracker.pending(Stage::Commit, host_evidence(error), error);
        });
        match committed {
            Ok(vmm) => {
                tracker.note(Progress::Committed);
                Ok((vmm, session))
            }
            Err(error) => {
                drop(session);
                let teardown = super::Teardown::Unresolved(
                    "the clone's VMM ended through its handle, and the launch's journal remains \
                     until a reattach ends the launch"
                        .to_owned(),
                );
                Err(tracker.record(Stage::Commit, Fault::host(error), teardown))
            }
        }
    }

    /// Ends the launch, as [`Activated::abandon`] does.
    pub fn abandon(self) -> Result<Recovery> {
        let Self {
            session, starting, ..
        } = self;
        drop(session);
        starting.abandon()
    }

    /// Fails the clone, once its sandbox started, for a check of the caller's own before the
    /// commit of its launch, with `evidence` and `error`, as [`Activated::fail`] does, at
    /// [`Stage::Commit`].
    pub fn fail(self, evidence: Evidence, error: Error) -> CloneFailure {
        let Self {
            session,
            starting,
            tracker,
            ..
        } = self;
        drop(session);
        tracker.fail(starting, Stage::Commit, Fault::new(evidence, error))
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
/// deadline, or by the deadline of a reattach.
pub(super) struct ActivationSession<'a> {
    /// The capability of the launch.
    pub(super) capability: &'a [u8; CAPABILITY_LEN],
    /// The path of the activation record in the sandbox.
    pub(super) record: &'a Path,
    /// The canonical encoding of the launch's activation record.
    pub(super) encoded: Vec<u8>,
    /// The activation that the clone's Ready must report.
    pub(super) expected: ReadyActivation,
    /// The deadline by which the clone must be ready.
    pub(super) deadline: Instant,
    /// The deadline's duration from its start.
    pub(super) timeout: Duration,
    /// What starts the deadline, as messages say: `its launch`, or `its reattach`.
    pub(super) since: &'static str,
}

/// How a session of a clone's activation failed.
enum Attempt {
    /// The adapter lost the session, after which another session may find the clone ready.
    Lost(Fault),
    /// Any other failure, which is final.
    Failed(Fault),
}

impl ActivationSession<'_> {
    /// Activates the clone whose VMM, `reference`, serves its control endpoint at `endpoint`, in
    /// sessions of `control`, as [`Restoring::activate`] describes: in a new session after a
    /// session that the adapter lost, in [`MAX_ACTIVATION_SESSIONS`] at most, and returns the
    /// session in which the clone answered with its Ready. A launch records the clone's progress
    /// in its `tracker`.
    pub(super) fn activate<C: CloneControl>(
        &self,
        control: &mut C,
        endpoint: &str,
        reference: &ProcessRef,
        mut tracker: Option<&mut Tracker>,
    ) -> std::result::Result<C::Session, Fault> {
        let mut sessions = 0;
        loop {
            sessions += 1;
            match self.run(control, endpoint, reference, tracker.as_deref_mut()) {
                Ok(session) => return Ok(session),
                Err(Attempt::Lost(_)) if sessions < MAX_ACTIVATION_SESSIONS => {
                    let left = self.deadline.saturating_duration_since(Instant::now());
                    thread::sleep(SESSION_RETRY_INTERVAL.min(left));
                }
                Err(Attempt::Lost(fault) | Attempt::Failed(fault)) => return Err(fault),
            }
        }
    }

    /// Activates the clone in a new session of `control`, as [`activate`](Self::activate)
    /// describes, records its progress in `tracker`, if it has one, and returns the session once
    /// the clone is ready.
    fn run<C: CloneControl>(
        &self,
        control: &mut C,
        endpoint: &str,
        reference: &ProcessRef,
        mut tracker: Option<&mut Tracker>,
    ) -> std::result::Result<C::Session, Attempt> {
        let record = self.read_record().map_err(Attempt::Failed)?;
        let transport = self.connect(endpoint, reference).map_err(Attempt::Failed)?;
        let attached = control.attach(transport, self.capability, self.deadline);
        let mut session =
            attached.map_err(|error| self.failed(control, error, "attach its control session"))?;
        if let Some(tracker) = &mut tracker {
            // OpenVMM authenticated the session, which it does only once the guest acknowledged
            // a restore that gates the host's input. The Activate follows at once, with which
            // the progress log records the acknowledgment.
            tracker.advance(Progress::Acknowledged);
            tracker
                .reach(Progress::Activating)
                .map_err(Attempt::Failed)?;
        }
        let reported = control
            .activate(&mut session, &record, self.deadline)
            .map_err(|error| self.failed(control, error, "activate it"))?;
        self.check(reported).map_err(Attempt::Failed)?;
        if let Some(tracker) = &mut tracker {
            tracker.reach(Progress::Ready).map_err(Attempt::Failed)?;
        }
        Ok(session)
    }

    /// Connects to the control endpoint `endpoint`, which the VMM `reference` serves, as
    /// [`session::connect`] does, by the deadline.
    fn connect(
        &self,
        endpoint: &str,
        reference: &ProcessRef,
    ) -> std::result::Result<Box<dyn Transport>, Fault> {
        session::connect(endpoint, reference.pid, reference.start_time, self.deadline).map_err(
            |error| {
                let message = format!("cannot connect to the clone's control endpoint {endpoint}");
                match error {
                    SessionError::TimedOut => Fault::new(
                        Evidence::Timeout,
                        Error::timed_out(format!(
                            "{message} within {:?} of {}",
                            self.timeout, self.since
                        )),
                    ),
                    SessionError::ProcessExited => Fault::new(
                        Evidence::LostStatus,
                        Error::exited(format!("{message}: its VMM exited")),
                    ),
                    SessionError::Io(source) => Fault::host(Error::io(message).with_source(source)),
                    error => Fault::new(
                        Evidence::Protocol,
                        Error::protocol(message).with_source(error),
                    ),
                }
            },
        )
    }

    /// Reads the activation record in the sandbox, which must be, byte for byte, the encoding of
    /// the launch's record, and returns it.
    fn read_record(&self) -> std::result::Result<Vec<u8>, Fault> {
        let path = self.record;
        let bytes = open_regular(path)
            .and_then(|file| read_at_most(file, MAX_RECORD_BYTES as u64, "an activation record"))
            .map_err(|error| {
                Fault::host(
                    Error::io(format!(
                        "cannot read the clone's activation record {}",
                        path.display()
                    ))
                    .with_source(error),
                )
            })?;
        if bytes != self.encoded {
            return Err(Fault::new(
                Evidence::HostFailure,
                Error::io(format!(
                    "{} no longer holds the activation record that the clone's launch wrote, the \
                     only record that its restore sends",
                    path.display()
                )),
            ));
        }
        Ok(bytes)
    }

    /// Checks `reported`, the activation that the clone's Ready reports, against the record's.
    fn check(&self, reported: Option<ReadyActivation>) -> std::result::Result<(), Fault> {
        check_ready(&self.expected, reported).attribute(Evidence::Protocol)
    }

    /// Returns how a session failed with `error`, which `control` returned as it tried to `what`.
    fn failed<C: CloneControl>(&self, control: &C, error: C::Error, what: &str) -> Attempt {
        if Instant::now() >= self.deadline {
            return Attempt::Failed(Fault::new(
                Evidence::Timeout,
                Error::timed_out(format!(
                    "the clone was not ready within {:?} of {}: cannot {what}",
                    self.timeout, self.since
                ))
                .with_source(error),
            ));
        }
        if control.lost(&error) {
            let error = Error::protocol(format!(
                "the clone's control session was lost: cannot {what}"
            ))
            .with_source(error);
            Attempt::Lost(Fault::new(Evidence::Reset, error))
        } else {
            let error = Error::protocol(format!("cannot {what}")).with_source(error);
            Attempt::Failed(Fault::new(Evidence::Protocol, error))
        }
    }
}

/// Checks `reported`, the activation that a clone's Ready reports, against `expected`, its
/// record's: the transaction ID of the record's restore section.
fn check_ready(expected: &ReadyActivation, reported: Option<ReadyActivation>) -> Result<()> {
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

/// Waits until `deadline`, `timeout` after the launch, for the restore-ready event of the VMM of
/// `starting` at `listener`, as [`Restoring::activate`] describes.
fn wait_event(
    listener: &mut ReadyListener,
    starting: &Starting<'_>,
    deadline: Instant,
    timeout: Duration,
) -> std::result::Result<(), Fault> {
    let process = starting.process();
    let pid = process.reference().pid;
    // OpenVMM writes the event once, and then closes its end, so the wait reads the event's bytes
    // and nothing past them.
    let length = RESTORE_READY_EVENT.len();
    let mut received = Vec::with_capacity(length);
    while received.len() < length {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Fault::new(
                Evidence::Timeout,
                Error::timed_out(format!(
                    "the clone's VMM sent no restore-ready event within {timeout:?} of its launch"
                )),
            ));
        }
        let before = received.len();
        let open = listener
            .receive(pid, &mut received, length, left.min(EXIT_CHECK_INTERVAL))
            .map_err(|error| {
                let message = "cannot receive the restore-ready event of the clone's VMM";
                if error.kind() == io::ErrorKind::PermissionDenied {
                    Fault::new(
                        Evidence::Protocol,
                        Error::protocol(message).with_source(error),
                    )
                } else {
                    Fault::host(Error::io(message).with_source(error))
                }
            })?;
        if !RESTORE_READY_EVENT.starts_with(&received) {
            return Err(Fault::new(
                Evidence::Protocol,
                Error::protocol(format!(
                    "the clone's VMM sent {:?} at its restore-ready endpoint, which is not \
                     OpenVMM's restore-ready event",
                    String::from_utf8_lossy(&received)
                )),
            ));
        }
        if received.len() == length {
            break;
        }
        if !open {
            if received.is_empty() {
                let wait = deadline.min(Instant::now() + CLOSED_EXIT_WAIT);
                let exited = process.wait(wait).map_err(|error| {
                    Fault::host(
                        Error::io("cannot tell whether the clone's VMM still runs")
                            .with_source(error),
                    )
                })?;
                if exited {
                    return Err(Fault::new(
                        Evidence::LostStatus,
                        Error::exited(
                            "the clone's VMM closed its restore-ready endpoint without the event, \
                             and exited, as OpenVMM does once it cannot restore the snapshot",
                        ),
                    ));
                }
            }
            return Err(Fault::new(
                Evidence::Protocol,
                Error::protocol(format!(
                    "the clone's VMM closed its restore-ready endpoint after {} bytes of the event",
                    received.len()
                )),
            ));
        }
        if received.len() == before {
            let exited = process.has_exited().map_err(|error| {
                Fault::host(
                    Error::io("cannot tell whether the clone's VMM still runs").with_source(error),
                )
            })?;
            if exited {
                return Err(Fault::new(
                    Evidence::LostStatus,
                    Error::exited(
                        "the clone's VMM exited before its restore-ready event, as OpenVMM does \
                         once it cannot restore the snapshot",
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Returns the fault of the deadline `name`, `timeout` after its start, which this host's clock
/// cannot represent.
fn beyond_clock(name: &str, timeout: Duration) -> Fault {
    Fault::new(
        Evidence::HostFailure,
        Error::unsupported(format!(
            "the {name} deadline of a clone's restore, {timeout:?} after its start, is beyond \
             this host's clock"
        )),
    )
}
