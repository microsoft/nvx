//! The guest restore of a repaired clone, in the caller's process: [`GuestRestore`], the state
//! machine through which the clone takes its configuration from the activation record of its
//! restore packet and reports Ready on its control session, and [`apply`](fn@apply), which
//! activates the clone's root and workload with that configuration, as the
//! [module](super#guest-restore) describes.

pub(crate) mod apply;
#[cfg(test)]
mod tests;

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Repaired;
use crate::guest_restore::{
    ExpectedResources, GuestRestoreError, GuestRestoreReady, GuestRestoreRecord,
};

pub(crate) use self::apply::{Applied, ApplyError, RootTemplate, apply};
pub use self::apply::{MACHINE_ID_FILE, RESOLVER_FILE};

/// The guest restore of a repaired clone: the state machine, without I/O and independent of
/// any framing, through which the clone takes its configuration from the activation record that
/// its restore packet carried, and reports Ready on its control session. It holds no admission
/// fence and makes no system call; see the [module](super#guest-restore).
///
/// The adapter hands it the guest-restore record that the packet's activation record holds, as
/// soon as the clone runs, through [`accept`](Self::accept), which decodes and validates it,
/// applies the record in its own space by the guest restore's [`deadline`](Self::deadline), and
/// reports the result through [`complete`](Self::complete). It passes the guest restore what it
/// receives on the session, each a [`GuestRestoreInput`], through [`input`](Self::input), and does
/// what the [`GuestRestoreAction`] that it returns says: the host asks for the clone's Ready,
/// first in each of its sessions, and the adapter answers with Ready once the record applied.
/// It reports the start of the sandbox, which its own order takes once the guest restore is
/// ready, through [`start`](Self::start). The adapter's loop wakes by the deadline, and
/// [ticks](Self::tick) the guest restore then, whether input arrives or not, and whether its
/// output drains or not.
///
/// It goes through the [`GuestRestoreState`]s in order, and each takes these inputs:
///
/// | State | Reset | Flow | Ready | Request |
/// | --- | --- | --- | --- | --- |
/// | [`Pending`](GuestRestoreState::Pending), [`Activating`](GuestRestoreState::Activating) | [`Acknowledge`](GuestRestoreAction::Acknowledge) | [`Continue`](GuestRestoreAction::Continue) | [`Join`](GuestRestoreAction::Join) | Fails |
/// | [`Applied`](GuestRestoreState::Applied) | [`Acknowledge`](GuestRestoreAction::Acknowledge) | [`Continue`](GuestRestoreAction::Continue) | [`Ready`](GuestRestoreAction::Ready), ready next | Fails |
/// | [`Ready`](GuestRestoreState::Ready), [`Started`](GuestRestoreState::Started) | [`Acknowledge`](GuestRestoreAction::Acknowledge) | [`Continue`](GuestRestoreAction::Continue) | [`Ready`](GuestRestoreAction::Ready) | [`Serve`](GuestRestoreAction::Serve) |
///
/// Every failure is final, and leaves the guest restore [failed](GuestRestoreState::Failed): every
/// later call returns the same [`GuestRestoreFailure`]. The guest restore never starts a process or
/// powers the VM off: the adapter reports the failure and ends the VM.
///
/// # Example
///
/// ```no_run
/// use std::error::Error;
/// use std::time::Instant;
///
/// use nvx::guest_restore::{GuestRestoreRecord, GuestRestoreReady};
/// use nvx::guest::restore::{
///     GuestRestore, GuestRestoreAction, GuestRestoreFailure, GuestRestoreInput,
/// };
///
/// /// What the agent's control session received from the host, in the agent's own framing.
/// enum Received {
///     /// The session's reset.
///     Reset,
///     /// Bytes that complete no message, such as the start of one.
///     Partial,
///     /// The host's request for the clone's Ready.
///     Ready,
///     /// Any other message of the host, such as a part of the mapping table, or the start.
///     Other(Vec<u8>),
/// }
///
/// /// The agent of a clone, which serves its control session in its own framing, and queues what
/// /// it sends, so that a host that reads nothing holds up neither the agent nor the deadline.
/// trait Agent {
///     /// Waits until the host sends something, or until `deadline`, while it writes what the
///     /// agent queued, and returns what the host sent, or `None` once the deadline passed. A
///     /// deadline that passed already returns only what the host sent already.
///     fn receive(&mut self, deadline: Option<Instant>) -> Option<Received>;
///     /// Queues the acknowledgement of the session's reset, and abandons the earlier session's
///     /// work, the requests for Ready that joined the guest restore included.
///     fn acknowledge(&mut self);
///     /// Activates the clone's root and workload with the settings of `record` in the agent's
///     /// own space, whose waits for the devices end by `deadline`, and keeps them: the identity,
///     /// if any, in which it runs every execution, with the hostname of every execution, the
///     /// root, and the export's root, from which it maps the host's mapping table. nvx-init's
///     /// space, NVX's internal reference code, is private to `nvx`.
///     fn activate(
///         &mut self,
///         record: &GuestRestoreRecord,
///         deadline: Option<Instant>,
///     ) -> Result<(), Box<dyn Error + Send + Sync>>;
///     /// Queues Ready, which reports `ready` beside the agent's own identities, such as its
///     /// build, in answer to each request for Ready of the current session that joined the
///     /// guest restore, or to the one that the host just sent.
///     fn ready(&mut self, ready: GuestRestoreReady);
///     /// Serves `message` in the agent's order, as after a cold boot, and returns whether the
///     /// host started the sandbox with it.
///     fn serve(&mut self, message: Vec<u8>) -> bool;
/// }
///
/// /// Activates the clone whose guest restore is `restore` from `record`, the guest-restore
/// /// record that its restore packet's activation record holds, and serves its control session
/// /// until it fails.
/// fn serve(
///     agent: &mut impl Agent,
///     mut restore: GuestRestore,
///     record: Option<&[u8]>,
/// ) -> Result<(), GuestRestoreFailure> {
///     // The record is valid, and nothing happened before: the guest restore's side effects start
///     // here, without waiting for the host. The application waits for the devices until the
///     // guest restore's deadline at the latest, so that it ends by then.
///     let record = restore.accept(Instant::now(), record)?;
///     let applied = agent.activate(&record, restore.deadline());
///     // A failed application fails the guest restore. Without a request for Ready that joined,
///     // the guest restore is applied, and awaits the host's.
///     let ready = restore.complete(Instant::now(), applied)?;
///     if restore.established().is_some() {
///         agent.ready(ready);
///     }
///     loop {
///         let Some(received) = agent.receive(restore.deadline()) else {
///             // The deadline passed, unless the clone is ready, which ends the guest restore.
///             restore.tick(Instant::now())?;
///             continue;
///         };
///         match restore.input(Instant::now(), input(&received))? {
///             GuestRestoreAction::Acknowledge => agent.acknowledge(),
///             // The agent applies the record before it receives anything, so no request for
///             // Ready joins the guest restore.
///             GuestRestoreAction::Continue | GuestRestoreAction::Join => {}
///             GuestRestoreAction::Ready(ready) => agent.ready(ready),
///             // The guest restore serves only another message, once it is ready.
///             GuestRestoreAction::Serve => {
///                 if let Received::Other(message) = received
///                     && agent.serve(message)
///                 {
///                     restore.start()?;
///                 }
///             }
///         }
///     }
/// }
///
/// /// Returns the guest restore's input of `received`.
/// fn input(received: &Received) -> GuestRestoreInput {
///     match received {
///         Received::Reset => GuestRestoreInput::Reset,
///         Received::Partial => GuestRestoreInput::Flow,
///         Received::Ready => GuestRestoreInput::Ready,
///         Received::Other(_) => GuestRestoreInput::Request,
///     }
/// }
/// ```
pub struct GuestRestore {
    /// The resources that the clone has, as its repair reports them, which the record must
    /// expect.
    resources: ExpectedResources,
    /// When the guest restore fails unless it is ready, or `None` if no instant is that late.
    deadline: Option<Instant>,
    /// Whether a request for Ready of the session in progress joined the guest restore before
    /// the record applied, which [`complete`](Self::complete)'s Ready answers.
    asked: bool,
    /// Where the guest restore is.
    state: State,
}

impl GuestRestore {
    /// Returns the guest restore of the clone that `repaired` describes, which awaits the record
    /// of its restore packet, without holding or changing an admission fence.
    ///
    /// The guest restore must be ready by its deadline: `timeout`, which the caller chooses, after
    /// the repair acknowledged the restore, as [`Repaired::acknowledged_at`] tells, or after `now`
    /// for a restore that the host did not gate until an acknowledgement, whose repair
    /// acknowledged nothing. Nothing extends it. A deadline that no instant can hold never
    /// passes.
    pub fn new(repaired: &Repaired, timeout: Duration, now: Instant) -> Self {
        let resources = ExpectedResources {
            cpus: repaired.cpus(),
            memory: repaired.memory(),
        };
        Self::with_resources(resources, repaired.acknowledged_at(), timeout, now)
    }

    /// Returns the guest restore of a clone that has `resources`, and whose repair acknowledged its
    /// restore at `acknowledged_at`, if it did, as [`new`](Self::new) does with the resources and
    /// the acknowledgement of a [`Repaired`] clone: for a caller that has them otherwise, such as
    /// the tests of an adapter's framing, which run its session without a restore.
    pub fn with_resources(
        resources: ExpectedResources,
        acknowledged_at: Option<Instant>,
        timeout: Duration,
        now: Instant,
    ) -> Self {
        let start = acknowledged_at.unwrap_or(now);
        Self {
            resources,
            deadline: start.checked_add(timeout),
            asked: false,
            state: State::Pending,
        }
    }

    /// Accepts `record`, at `now`, the bytes of the guest-restore record that the clone's restore
    /// packet carried, which the adapter takes from the packet's activation record, as
    /// [`Repaired::activation_record`] returns it, or `None` for a packet that carried none, and
    /// returns the decoded record, for the adapter to apply in its own space.
    ///
    /// It decodes the record with [`GuestRestoreRecord::decode`], which refuses it with
    /// [`GuestRestoreCause::Record`] unless it is canonical and keeps every limit, checks that it
    /// is a clone's, with a restore section, or refuses it with [`GuestRestoreCause::Cold`], and
    /// checks that it expects the resources that the clone has, the CPU count and the memory size
    /// that its repair reports, or refuses it with [`GuestRestoreCause::Resources`]. Only then
    /// does it accept it: nothing has a side effect before. A packet without a record fails with
    /// [`GuestRestoreCause::Missing`], and a second record with [`GuestRestoreCause::Accept`].
    /// Past its deadline, the guest restore fails with [`GuestRestoreCause::Expired`] instead.
    pub fn accept(
        &mut self,
        now: Instant,
        record: Option<&[u8]>,
    ) -> Result<Box<GuestRestoreRecord>, GuestRestoreFailure> {
        self.check(now)?;
        if !matches!(self.state, State::Pending) {
            return Err(self.fail(GuestRestoreCause::Accept));
        }
        let Some(record) = record else {
            return Err(self.fail(GuestRestoreCause::Missing));
        };
        let decoded = match GuestRestoreRecord::decode(record) {
            Ok(decoded) => decoded,
            Err(error) => return Err(self.fail(GuestRestoreCause::Record(error))),
        };
        if decoded.restore().is_none() {
            return Err(self.fail(GuestRestoreCause::Cold));
        }
        let expected = decoded.configuration().resources;
        if expected != self.resources {
            let actual = self.resources;
            return Err(self.fail(GuestRestoreCause::Resources { expected, actual }));
        }
        self.state = State::Activating(GuestRestoreReady::of(&decoded));
        Ok(Box::new(decoded))
    }

    /// Takes `input`, which the adapter received at `now` on the clone's control session, and
    /// returns what the adapter does next, as the [type](Self) tabulates it, or fails the guest
    /// restore.
    ///
    /// Until the guest restore is ready, it fails once `now` reaches its deadline, whatever the
    /// input, with [`GuestRestoreCause::Expired`]. Otherwise it takes:
    ///
    /// - flow control, in every state;
    /// - the session's reset, in every state: the one that the restore queued for the session
    ///   that the guest attached before the capture, or a later one, such as once the host's
    ///   attachment ended. The adapter acknowledges it. A reset changes nothing of the record,
    ///   which came with the restore: it only abandons a request for Ready of the earlier session
    ///   that joined the guest restore, which the host asks again in the later one;
    /// - the host's request for Ready, which comes first in each of its sessions: before the
    ///   record applied, it joins the guest restore, whose completion answers it; once the record
    ///   applied, it makes the clone ready, and gets what Ready reports; and once the clone is
    ///   ready, it gets the established Ready, without activating again;
    /// - any other request, once the clone is ready, which goes to the adapter's order, as after a
    ///   cold boot. Before, it fails with [`GuestRestoreCause::Request`].
    pub fn input(
        &mut self,
        now: Instant,
        input: GuestRestoreInput,
    ) -> Result<GuestRestoreAction, GuestRestoreFailure> {
        self.check(now)?;
        match (&self.state, input) {
            (State::Failed(failure), _) => Err(failure.clone()),
            (_, GuestRestoreInput::Flow) => Ok(GuestRestoreAction::Continue),
            (_, GuestRestoreInput::Reset) => {
                self.asked = false;
                Ok(GuestRestoreAction::Acknowledge)
            }
            (State::Pending | State::Activating(_), GuestRestoreInput::Ready) => {
                self.asked = true;
                Ok(GuestRestoreAction::Join)
            }
            (State::Applied(ready), GuestRestoreInput::Ready) => {
                let ready = *ready;
                self.state = State::Ready(ready);
                Ok(GuestRestoreAction::Ready(ready))
            }
            (State::Ready(ready) | State::Started(ready), GuestRestoreInput::Ready) => {
                Ok(GuestRestoreAction::Ready(*ready))
            }
            (State::Ready(_) | State::Started(_), GuestRestoreInput::Request) => {
                Ok(GuestRestoreAction::Serve)
            }
            (_, GuestRestoreInput::Request) => Err(self.fail(GuestRestoreCause::Request)),
        }
    }

    /// Completes the guest restore of the accepted record at `now`, with the `result` of the
    /// adapter's application of the record, and returns what Ready reports: that the guest
    /// restore applied, with the record's transaction ID.
    ///
    /// A result that succeeded by the deadline makes the guest restore ready, if a request for
    /// Ready of the session in progress joined it, and the adapter answers that request with
    /// Ready; otherwise the guest restore is applied, and awaits the host's request, by the same
    /// deadline. A failure, which may have applied part of the record, is final, with
    /// [`GuestRestoreCause::Failed`], so that nothing applies the record again. Past the deadline,
    /// the guest restore fails with [`GuestRestoreCause::Expired`] instead, whatever the result.
    /// Without an accepted record whose application runs, it fails with
    /// [`GuestRestoreCause::Completion`].
    pub fn complete(
        &mut self,
        now: Instant,
        result: Result<(), Box<dyn Error + Send + Sync>>,
    ) -> Result<GuestRestoreReady, GuestRestoreFailure> {
        self.check(now)?;
        let ready = match &self.state {
            State::Activating(ready) => *ready,
            _ => return Err(self.fail(GuestRestoreCause::Completion)),
        };
        if let Err(error) = result {
            return Err(self.fail(GuestRestoreCause::Failed(error)));
        }
        self.state = if self.asked {
            State::Ready(ready)
        } else {
            State::Applied(ready)
        };
        Ok(ready)
    }

    /// Records that the adapter's order started the sandbox, once the guest restore is ready: the
    /// host's start, after the mapping table that the record announced, as after a cold boot.
    ///
    /// A start before the guest restore is ready, or a second one, fails with
    /// [`GuestRestoreCause::Start`].
    pub fn start(&mut self) -> Result<(), GuestRestoreFailure> {
        let ready = match &self.state {
            State::Failed(failure) => return Err(failure.clone()),
            State::Ready(ready) => *ready,
            _ => return Err(self.fail(GuestRestoreCause::Start)),
        };
        self.state = State::Started(ready);
        Ok(())
    }

    /// Fails the guest restore with [`GuestRestoreCause::Expired`] once `now` reaches its deadline,
    /// unless it is ready, as each input does, so that the adapter wakes by the
    /// [deadline](Self::deadline) and ticks the guest restore then, whether input arrives or not.
    pub fn tick(&mut self, now: Instant) -> Result<(), GuestRestoreFailure> {
        self.check(now)
    }

    /// Returns when the guest restore fails unless it is ready, for the adapter to
    /// [tick](Self::tick) it then, or `None` once it is ready or failed, or if no instant is that
    /// late.
    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Pending | State::Activating(_) | State::Applied(_) => self.deadline,
            State::Ready(_) | State::Started(_) | State::Failed(_) => None,
        }
    }

    /// Returns where the guest restore is.
    pub fn state(&self) -> GuestRestoreState {
        self.state.name()
    }

    /// Returns what Ready reports about the established guest restore, once it is ready: that
    /// the guest restore applied, with the record's transaction ID, which a later request for
    /// Ready gets without activating again.
    pub fn established(&self) -> Option<GuestRestoreReady> {
        match &self.state {
            State::Ready(ready) | State::Started(ready) => Some(*ready),
            _ => None,
        }
    }

    /// Fails the guest restore with the failure that it already had, or with
    /// [`GuestRestoreCause::Expired`] once `now` reaches its deadline, unless it is ready.
    fn check(&mut self, now: Instant) -> Result<(), GuestRestoreFailure> {
        if let State::Failed(failure) = &self.state {
            return Err(failure.clone());
        }
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            return Err(self.fail(GuestRestoreCause::Expired));
        }
        Ok(())
    }

    /// Fails the guest restore in its state, because of `cause`, and returns the failure, unless it
    /// failed already, which it then returns.
    fn fail(&mut self, cause: GuestRestoreCause) -> GuestRestoreFailure {
        if let State::Failed(failure) = &self.state {
            return failure.clone();
        }
        let failure = GuestRestoreFailure {
            state: self.state.name(),
            cause: Arc::new(cause),
        };
        self.state = State::Failed(failure.clone());
        failure
    }
}

impl fmt::Debug for GuestRestore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuestRestore")
            .field("state", &self.state.name())
            .field("resources", &self.resources)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// What the adapter received on a clone's control session, in the terms of its [`GuestRestore`],
/// whatever its framing: the session's reset, flow control, the host's request for Ready, or
/// another request of the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestRestoreInput {
    /// The control session's reset, an [`Event::Reset`](crate::guest::control::Event::Reset),
    /// which starts the session over: the one that the restore queued for the session that the
    /// guest attached before the capture, or a later one, such as once the host's attachment
    /// ended.
    Reset,
    /// Flow control, which carries no request: bytes of a message that they do not complete, or
    /// a credit that the adapter returned to its transport's receive window, if it has one.
    Flow,
    /// The host's request for the clone's Ready, which the host sends first in each of its
    /// sessions: OpenVMM's broker refuses the guest's records while no host is attached, which the
    /// guest cannot tell, so the guest answers the host rather than sending Ready unasked.
    Ready,
    /// Any other request of the host, such as a part of the mapping table, the start of the
    /// sandbox, an execution, or a probe of the adapter's framing.
    Request,
}

/// What the adapter does next, which [`GuestRestore::input`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestRestoreAction {
    /// The guest restore took the session's reset: the adapter abandons the work of the earlier
    /// session, a request for Ready that joined the guest restore included, then acknowledges the
    /// reset, as [`Connection::acknowledge`](crate::guest::control::Connection::acknowledge)
    /// does, unless its transport acknowledged it already.
    Acknowledge,
    /// The guest restore took the input, which needs nothing more of the adapter.
    Continue,
    /// The host's request for Ready came before the record applied, and joined the guest
    /// restore: what [`GuestRestore::complete`] returns answers it, as the adapter's framing
    /// answers each request.
    Join,
    /// The host's request for Ready, once the record applied: the adapter answers it with Ready,
    /// which reports this result.
    Ready(GuestRestoreReady),
    /// A request once the guest restore is ready: the adapter serves it in its order, as after a
    /// cold boot.
    Serve,
}

/// The state of a [`GuestRestore`], in the order in which it goes through them, which an
/// [`GuestRestoreFailure`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GuestRestoreState {
    /// The guest restore awaits the record of the clone's restore packet.
    Pending,
    /// The adapter applies the accepted record.
    Activating,
    /// The record applied, and the guest restore awaits the host's request for Ready.
    Applied,
    /// Ready, with the mappings pending: the guest restore completed, and Ready answered the
    /// host's request. The host's mapping table, which the record announced, and the start of the
    /// sandbox follow in the adapter's order, as after a cold boot.
    Ready,
    /// The adapter's order started the sandbox.
    Started,
    /// The guest restore failed, finally.
    Failed,
}

impl fmt::Display for GuestRestoreState {
    /// Writes the state's name, such as `pending`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pending => "pending",
            Self::Activating => "activating",
            Self::Applied => "applied",
            Self::Ready => "ready",
            Self::Started => "started",
            Self::Failed => "failed",
        })
    }
}

/// Why a [`GuestRestore`] failed, which a [`GuestRestoreFailure`] holds.
#[derive(Debug)]
#[non_exhaustive]
pub enum GuestRestoreCause {
    /// The deadline passed before the guest restore was ready.
    Expired,
    /// The host sent a request other than its request for Ready before the guest restore was
    /// ready.
    Request,
    /// The clone's restore packet carried no record.
    Missing,
    /// The packet's record does not decode, or breaks a limit of `nvx::guest_restore`, which
    /// refused it with this error.
    Record(GuestRestoreError),
    /// The packet's record is a cold boot's, without the restore section of a clone's.
    Cold,
    /// The packet's record expects other resources than the clone has.
    Resources {
        /// The CPU count and the memory size that the record expects.
        expected: ExpectedResources,
        /// The CPU count and the memory size that the clone has, as its repair reports them: the
        /// number of its online CPUs, and the size of its online memory blocks.
        actual: ExpectedResources,
    },
    /// The adapter's application of the accepted record failed, wholly or partway, with this
    /// error.
    Failed(Box<dyn Error + Send + Sync>),
    /// The adapter handed the guest restore a record once it had accepted one.
    Accept,
    /// The adapter completed a guest restore that did not run: before the guest restore accepted a
    /// record, or once it completed one.
    Completion,
    /// The adapter's order started the sandbox before the guest restore was ready, or again.
    Start,
}

impl fmt::Display for GuestRestoreCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expired => {
                formatter.write_str("the deadline passed before the activation was ready")
            }
            Self::Request => {
                formatter.write_str("the host sent a request before the activation was ready")
            }
            Self::Missing => formatter.write_str("the restore packet carried no record"),
            Self::Record(error) => {
                write!(formatter, "the restore packet's record is refused: {error}")
            }
            Self::Cold => formatter.write_str(
                "the restore packet's record is a cold boot's, without a restore section",
            ),
            Self::Resources { expected, actual } => write!(
                formatter,
                "the restore packet's record expects {} CPUs and {} bytes of memory, and the clone \
                 has {} CPUs and {} bytes",
                expected.cpus, expected.memory, actual.cpus, actual.memory
            ),
            Self::Failed(error) => write!(formatter, "the record's application failed: {error}"),
            Self::Accept => formatter.write_str("the activation took a record again"),
            Self::Completion => formatter.write_str("an activation that did not run completed"),
            Self::Start => {
                formatter.write_str("the sandbox started before the activation was ready, or again")
            }
        }
    }
}

/// Why a [`GuestRestore`] failed, finally: the state in which it failed, and its cause.
///
/// Its `Display` form names both, such as `the activation failed in its pending state: the
/// restore packet carried no record`. A guest restore keeps its failure, which every later call
/// returns again. The failure has no exit status: the adapter that ends the VM documents its own.
#[derive(Clone, Debug)]
pub struct GuestRestoreFailure {
    /// The state in which the guest restore failed.
    state: GuestRestoreState,
    /// Why it failed.
    cause: Arc<GuestRestoreCause>,
}

impl GuestRestoreFailure {
    /// Returns the state in which the guest restore failed, which is never
    /// [`GuestRestoreState::Failed`].
    pub fn state(&self) -> GuestRestoreState {
        self.state
    }

    /// Returns why it failed.
    pub fn cause(&self) -> &GuestRestoreCause {
        &self.cause
    }
}

impl fmt::Display for GuestRestoreFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the activation failed in its {} state: {}",
            self.state, self.cause
        )
    }
}

impl Error for GuestRestoreFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self.cause.as_ref() {
            GuestRestoreCause::Record(error) => Some(error),
            GuestRestoreCause::Failed(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// Where a [`GuestRestore`] is, with what Ready reports about its accepted record, from the
/// activating state on.
#[derive(Clone, Debug)]
enum State {
    /// The guest restore awaits the packet's record.
    Pending,
    /// The adapter applies the accepted record.
    Activating(GuestRestoreReady),
    /// The record applied, and the guest restore awaits the host's request for Ready.
    Applied(GuestRestoreReady),
    /// Ready, with the mappings pending.
    Ready(GuestRestoreReady),
    /// The adapter's order started the sandbox.
    Started(GuestRestoreReady),
    /// The guest restore failed, finally, with this failure.
    Failed(GuestRestoreFailure),
}

impl State {
    /// Returns the state's name.
    fn name(&self) -> GuestRestoreState {
        match self {
            Self::Pending => GuestRestoreState::Pending,
            Self::Activating(_) => GuestRestoreState::Activating,
            Self::Applied(_) => GuestRestoreState::Applied,
            Self::Ready(_) => GuestRestoreState::Ready,
            Self::Started(_) => GuestRestoreState::Started,
            Self::Failed(_) => GuestRestoreState::Failed,
        }
    }
}
