//! The guest restore of a repaired clone, in the caller's process: [`GuestRestore`], the state
//! machine of the clone's restricted control session, through which the clone takes its
//! configuration from the host in one Activate, and [`apply`](fn@apply), which activates the
//! clone's root and workload with that configuration, as the [module](super#guest-restore)
//! describes.

pub(crate) mod apply;
#[cfg(test)]
mod tests;

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Repaired;
use crate::guest_restore::{
    ExpectedResources, GuestRestoreError, GuestRestoreReady, GuestRestoreRecord, TransactionId,
};

pub(crate) use self::apply::{Applied, ApplyError, RootTemplate, apply};
pub use self::apply::{MACHINE_ID_FILE, RESOLVER_FILE};

/// The guest restore of a repaired clone: the state machine, without I/O and independent of
/// any framing, of the clone's control session, which takes the clone's configuration from the
/// host in one Activate. It holds no admission fence and makes no system call; see the
/// [module](super#guest-restore).
///
/// The adapter passes it what it receives on the session, each a [`GuestRestoreInput`], through
/// [`input`](Self::input), and does what the [`GuestRestoreAction`] that it returns says. The
/// adapter applies the record of the Activate that the guest restore accepted, in its own
/// space, by the guest restore's [`deadline`](Self::deadline), passes the guest restore what
/// the session received while the application ran, which came before Ready, then reports the
/// result through [`complete`](Self::complete), whose Ready answers the Activate and each retry
/// that [joined](GuestRestoreAction::Join) it, and it reports the start of the sandbox, which
/// its own order takes once the guest restore is ready, through [`start`](Self::start). The
/// adapter's loop wakes by the deadline, and [ticks](Self::tick) the guest restore then,
/// whether input arrives or not, and whether its output drains or not.
///
/// It goes through the [`GuestRestoreState`]s in order, and each takes these inputs:
///
/// | State | Reset | Flow | Activate | Request |
/// | --- | --- | --- | --- | --- |
/// | [`Bootstrap`](GuestRestoreState::Bootstrap) | [`Acknowledge`](GuestRestoreAction::Acknowledge), restricted next | [`Continue`](GuestRestoreAction::Continue) | Fails | Fails |
/// | [`Restricted`](GuestRestoreState::Restricted) | Fails | [`Continue`](GuestRestoreAction::Continue) | [`Apply`](GuestRestoreAction::Apply), activating next, once its record is valid | Fails |
/// | [`Activating`](GuestRestoreState::Activating) | Fails | [`Continue`](GuestRestoreAction::Continue) | [`Join`](GuestRestoreAction::Join) if byte-equal | Fails |
/// | [`Ready`](GuestRestoreState::Ready), [`Started`](GuestRestoreState::Started) | [`Acknowledge`](GuestRestoreAction::Acknowledge), a later session | [`Continue`](GuestRestoreAction::Continue) | [`Ready`](GuestRestoreAction::Ready) if byte-equal | [`Serve`](GuestRestoreAction::Serve) |
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
///     /// The session's NVXS RESET.
///     Reset,
///     /// Bytes that complete no message, such as a part of a DATA record's payload.
///     Partial,
///     /// The host's Activate, with its record's bytes.
///     Activate(Vec<u8>),
///     /// The host's Activate, whose header announced this many bytes, more than a record has,
///     /// which the framing refused before it read any of them.
///     TooLong(usize),
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
///     /// Queues the acknowledgement of the session's reset, and abandons the earlier epoch's
///     /// work.
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
///     /// build.
///     fn ready(&mut self, ready: GuestRestoreReady);
///     /// Serves `message` in the agent's order, as after a cold boot, and returns whether the
///     /// host started the sandbox with it.
///     fn serve(&mut self, message: Vec<u8>) -> bool;
/// }
///
/// /// Serves the control session of the clone whose guest restore is `restore`, until it fails.
/// fn serve(agent: &mut impl Agent, mut restore: GuestRestore) -> Result<(), GuestRestoreFailure> {
///     loop {
///         let Some(received) = agent.receive(restore.deadline()) else {
///             // The deadline passed, unless the clone is ready, which ends the guest restore.
///             restore.tick(Instant::now())?;
///             continue;
///         };
///         match restore.input(Instant::now(), input(&received))? {
///             GuestRestoreAction::Acknowledge => agent.acknowledge(),
///             GuestRestoreAction::Continue | GuestRestoreAction::Join => {}
///             GuestRestoreAction::Apply(record) => {
///                 // The record is valid, and nothing happened before: the guest restore's side
///                 // effects start here. The application waits for the devices until the guest
///                 // restore's deadline at the latest, so that it ends by then.
///                 let applied = agent.activate(&record, restore.deadline());
///                 // What the host sent meanwhile came before Ready: a retry of the Activate,
///                 // byte for byte, joins the application, and anything else, a reset included,
///                 // fails the guest restore.
///                 let mut joined = 0;
///                 while let Some(received) = agent.receive(Some(Instant::now())) {
///                     let action = restore.input(Instant::now(), input(&received))?;
///                     if action == GuestRestoreAction::Join {
///                         joined += 1;
///                     }
///                 }
///                 // A failed application fails the guest restore.
///                 let ready = restore.complete(Instant::now(), applied)?;
///                 // Ready answers the Activate, and each retry that joined its application.
///                 for _ in 0..=joined {
///                     agent.ready(ready);
///                 }
///             }
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
/// fn input(received: &Received) -> GuestRestoreInput<'_> {
///     match received {
///         Received::Reset => GuestRestoreInput::Reset,
///         Received::Partial => GuestRestoreInput::Flow,
///         Received::Activate(record) => GuestRestoreInput::Activate(record),
///         Received::TooLong(length) => GuestRestoreInput::TooLong(*length),
///         Received::Other(_) => GuestRestoreInput::Request,
///     }
/// }
/// ```
pub struct GuestRestore {
    /// The resources that the clone has, as its repair reports them, which the record of an
    /// Activate must expect.
    resources: ExpectedResources,
    /// When the guest restore fails unless it is ready, or `None` if no instant is that late.
    deadline: Option<Instant>,
    /// Where the guest restore is.
    state: State,
}

impl GuestRestore {
    /// Returns the guest restore of the clone that `repaired` describes, in its
    /// [transport bootstrap](GuestRestoreState::Bootstrap), without holding or changing an
    /// admission fence.
    ///
    /// The guest restore must be ready by its deadline: `timeout`, which the caller chooses, after
    /// the repair acknowledged the restore, as [`Repaired::acknowledged_at`] tells, or after `now`
    /// for a restore that the host did not gate until an acknowledgement, whose repair
    /// acknowledged nothing. No input or retry extends it. A deadline that no instant can hold
    /// never passes.
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
            state: State::Bootstrap,
        }
    }

    /// Takes `input`, which the adapter received at `now` on the clone's control session, and
    /// returns what the adapter does next, as the [type](Self) tabulates it, or fails the
    /// guest restore.
    ///
    /// Until the guest restore is ready, it fails once `now` reaches its deadline, whatever the
    /// input, with [`GuestRestoreCause::Expired`]. In order, it then takes:
    ///
    /// 1. In the transport bootstrap, the session's reset, which the restore queued: the adapter
    ///    acknowledges it, and the session is restricted next. Before it, the host can send
    ///    nothing, so an Activate or another request fails with [`GuestRestoreCause::Request`].
    /// 2. While restricted, flow control, and one Activate. The guest restore decodes its record
    ///    with [`GuestRestoreRecord::decode`], which refuses it with [`GuestRestoreCause::Record`]
    ///    unless it is canonical and keeps every limit, checks that it is a clone's, with a restore
    ///    section, or refuses it with [`GuestRestoreCause::Cold`], and checks that it expects the
    ///    resources that the clone has, the CPU count and the memory size that its repair
    ///    reports, or refuses it with [`GuestRestoreCause::Resources`]. Only then does it accept
    ///    it, and hand the record to the adapter with [`GuestRestoreAction::Apply`]: nothing has a
    ///    side effect before. Any other request fails with [`GuestRestoreCause::Request`], and
    ///    another reset with [`GuestRestoreCause::Reset`].
    /// 3. While activating, flow control, and retries of the Activate: one of the same
    ///    transaction, with a byte-equal record, joins the guest restore that runs. Any other
    ///    Activate fails: one whose record does not decode, or is a cold boot's, with the cause
    ///    with which the first would have failed, one of another transaction with
    ///    [`GuestRestoreCause::Transaction`], and any other of the same transaction with
    ///    [`GuestRestoreCause::Changed`]. A request or a reset fails as while restricted.
    /// 4. Once ready, or started, everything: a reset starts a later session, which the adapter
    ///    acknowledges, a byte-equal retry of the Activate gets the cached result, what Ready
    ///    reports, without activating again, and any other Activate fails as while activating.
    ///    Every other request goes to the adapter's order, as after a cold boot.
    ///
    /// An Activate whose record the adapter's framing refused unread, as longer than a record can
    /// be, [`GuestRestoreInput::TooLong`], fails as one whose record has that many bytes: in the
    /// transport bootstrap with [`GuestRestoreCause::Request`], and from then on with
    /// [`GuestRestoreCause::Record`].
    pub fn input(
        &mut self,
        now: Instant,
        input: GuestRestoreInput<'_>,
    ) -> Result<GuestRestoreAction, GuestRestoreFailure> {
        self.check(now)?;
        match (self.state.name(), input) {
            (_, GuestRestoreInput::Flow) => Ok(GuestRestoreAction::Continue),
            (GuestRestoreState::Bootstrap, GuestRestoreInput::Reset) => {
                self.state = State::Restricted;
                Ok(GuestRestoreAction::Acknowledge)
            }
            (GuestRestoreState::Ready | GuestRestoreState::Started, GuestRestoreInput::Reset) => {
                Ok(GuestRestoreAction::Acknowledge)
            }
            (_, GuestRestoreInput::Reset) => Err(self.fail(GuestRestoreCause::Reset)),
            (GuestRestoreState::Restricted, GuestRestoreInput::Activate(record)) => {
                self.accept(record)
            }
            (
                GuestRestoreState::Activating
                | GuestRestoreState::Ready
                | GuestRestoreState::Started,
                GuestRestoreInput::Activate(record),
            ) => self.retry(record),
            // A record that long never decodes, so its Activate fails as one whose bytes the
            // adapter read would, whether the guest restore accepted another or not.
            (
                GuestRestoreState::Restricted
                | GuestRestoreState::Activating
                | GuestRestoreState::Ready
                | GuestRestoreState::Started,
                GuestRestoreInput::TooLong(length),
            ) => Err(
                self.fail(GuestRestoreCause::Record(GuestRestoreError::TooLong(
                    length,
                ))),
            ),
            (GuestRestoreState::Ready | GuestRestoreState::Started, GuestRestoreInput::Request) => {
                Ok(GuestRestoreAction::Serve)
            }
            (
                _,
                GuestRestoreInput::Activate(_)
                | GuestRestoreInput::TooLong(_)
                | GuestRestoreInput::Request,
            ) => Err(self.fail(GuestRestoreCause::Request)),
        }
    }

    /// Completes the guest restore of the accepted Activate's record at `now`, with the `result` of
    /// the adapter's application of the record, and returns what Ready reports: that the
    /// guest restore applied, with the record's transaction ID.
    ///
    /// A result that succeeded by the deadline makes the guest restore ready, and the adapter
    /// reports Ready. A failure, which may have
    /// applied part of the record, is final, with [`GuestRestoreCause::Failed`], so that no retry
    /// activates the record again. Past the deadline, the guest restore
    /// fails with [`GuestRestoreCause::Expired`] instead, whatever the result. Without a guest
    /// restore that runs, it fails with [`GuestRestoreCause::Completion`].
    pub fn complete(
        &mut self,
        now: Instant,
        result: Result<(), Box<dyn Error + Send + Sync>>,
    ) -> Result<GuestRestoreReady, GuestRestoreFailure> {
        self.check(now)?;
        let accepted = match &self.state {
            State::Activating(accepted) => accepted.clone(),
            _ => return Err(self.fail(GuestRestoreCause::Completion)),
        };
        if let Err(error) = result {
            return Err(self.fail(GuestRestoreCause::Failed(error)));
        }
        let ready = accepted.ready;
        self.state = State::Ready(accepted);
        Ok(ready)
    }

    /// Records that the adapter's order started the sandbox, once the guest restore is ready: the
    /// host's start, after the mapping table that the record announced, as after a cold boot.
    ///
    /// A start before the guest restore is ready, or a second one, fails with
    /// [`GuestRestoreCause::Start`].
    pub fn start(&mut self) -> Result<(), GuestRestoreFailure> {
        let accepted = match &self.state {
            State::Failed(failure) => return Err(failure.clone()),
            State::Ready(accepted) => accepted.clone(),
            _ => return Err(self.fail(GuestRestoreCause::Start)),
        };
        self.state = State::Started(accepted);
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
            State::Bootstrap | State::Restricted | State::Activating(_) => self.deadline,
            State::Ready(_) | State::Started(_) | State::Failed(_) => None,
        }
    }

    /// Returns where the guest restore is.
    pub fn state(&self) -> GuestRestoreState {
        self.state.name()
    }

    /// Returns what Ready reports about the established guest restore, once it is ready: that
    /// the guest restore applied, with the record's transaction ID, which a later session gets
    /// without activating again.
    pub fn established(&self) -> Option<GuestRestoreReady> {
        match &self.state {
            State::Ready(accepted) | State::Started(accepted) => Some(accepted.ready),
            _ => None,
        }
    }

    /// Accepts the host's first Activate, whose record's bytes are `record`, once it decoded and
    /// validated it: a clone's record, with a restore section, that expects the clone's resources.
    fn accept(&mut self, record: &[u8]) -> Result<GuestRestoreAction, GuestRestoreFailure> {
        let decoded = match GuestRestoreRecord::decode(record) {
            Ok(decoded) => decoded,
            Err(error) => return Err(self.fail(GuestRestoreCause::Record(error))),
        };
        let Some(restore) = decoded.restore() else {
            return Err(self.fail(GuestRestoreCause::Cold));
        };
        let expected = decoded.configuration().resources;
        if expected != self.resources {
            let actual = self.resources;
            return Err(self.fail(GuestRestoreCause::Resources { expected, actual }));
        }
        self.state = State::Activating(Accepted {
            record: record.into(),
            transaction: restore.transaction,
            ready: GuestRestoreReady::of(&decoded),
        });
        Ok(GuestRestoreAction::Apply(Box::new(decoded)))
    }

    /// Takes a retry of the accepted Activate, whose record's bytes are `record`: a byte-equal
    /// record joins the guest restore that runs, or gets its cached result once it is ready, and
    /// any other fails the guest restore.
    fn retry(&mut self, record: &[u8]) -> Result<GuestRestoreAction, GuestRestoreFailure> {
        let (accepted, ready) = match &self.state {
            State::Activating(accepted) => (accepted, false),
            State::Ready(accepted) | State::Started(accepted) => (accepted, true),
            _ => return Err(self.fail(GuestRestoreCause::Request)),
        };
        if *accepted.record == *record {
            return Ok(if ready {
                GuestRestoreAction::Ready(accepted.ready)
            } else {
                GuestRestoreAction::Join
            });
        }
        let cause = accepted.refusal(record);
        Err(self.fail(cause))
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
/// whatever its framing: the session's reset, flow control, the host's Activate, or another
/// request of the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestRestoreInput<'a> {
    /// The NVXS session's reset, a [`GuestEvent::Reset`](crate::protocol::nvxs::GuestEvent::Reset),
    /// which starts a new epoch: the one that the restore queued for the session that the guest
    /// attached before the capture, or a later one, such as once the host's attachment ended.
    Reset,
    /// Flow control, which carries no request: bytes of a message that they do not complete, such
    /// as a DATA record's payload, or a credit that the adapter returned to its receive window.
    Flow,
    /// The host's Activate, with the bytes of its guest-restore record, the canonical encoding of
    /// an [`GuestRestoreRecord`].
    Activate(&'a [u8]),
    /// The host's Activate, whose record the adapter's framing refused before it took any byte of
    /// it, since its header announced this many bytes, more than
    /// [`MAX_RECORD_BYTES`](crate::guest_restore::MAX_RECORD_BYTES): the guest restore takes it as
    /// an Activate whose record's bytes are that long, which [`GuestRestoreRecord::decode`] refuses
    /// with [`GuestRestoreError::TooLong`].
    TooLong(usize),
    /// Any other request of the host, such as a part of the mapping table, the start of the
    /// sandbox, an execution, or a probe of the adapter's framing.
    Request,
}

/// What the adapter does next, which [`GuestRestore::input`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestRestoreAction {
    /// The guest restore took the session's reset: the adapter acknowledges it with its receive
    /// window, unless its transport acknowledged it already, and abandons the work of the earlier
    /// epoch.
    Acknowledge,
    /// The guest restore took the input, which needs nothing more of the adapter.
    Continue,
    /// The guest restore accepted the host's Activate, whose record decoded and validated, and
    /// which nothing before had any side effect of: the adapter applies the record in its own
    /// space, which activates the root and the workload with its settings, passes
    /// the guest restore what the session received meanwhile, which came before Ready, then reports
    /// the result to [`GuestRestore::complete`].
    Apply(Box<GuestRestoreRecord>),
    /// A retry of the accepted Activate joined the guest restore that runs: what
    /// [`GuestRestore::complete`] returns answers it too, as the adapter's framing answers each
    /// request.
    Join,
    /// A retry of the accepted Activate, once the guest restore is ready: the adapter answers it
    /// with Ready, which reports this cached result, without activating again.
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
    /// The transport bootstrap: the clone awaits the reset of its NVXS session that the restore
    /// queued, which the adapter acknowledges.
    Bootstrap,
    /// The restricted session: it takes only flow control and one Activate.
    Restricted,
    /// The adapter applies the accepted Activate's record.
    Activating,
    /// Ready, with the mappings pending: the guest restore completed, and
    /// returned what Ready reports. The host's mapping table, which the record announced, and the
    /// start of the sandbox follow in the adapter's order, as after a cold boot.
    Ready,
    /// The adapter's order started the sandbox.
    Started,
    /// The guest restore failed, finally.
    Failed,
}

impl fmt::Display for GuestRestoreState {
    /// Writes the state's name, such as `restricted`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bootstrap => "transport bootstrap",
            Self::Restricted => "restricted",
            Self::Activating => "activating",
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
    /// The session was reset after its bootstrap, before the guest restore was ready.
    Reset,
    /// The host sent a request that the state does not take, before the guest restore was ready:
    /// any before the session's bootstrap, and any but one Activate, and its retries, after it.
    Request,
    /// The Activate's record does not decode, or breaks a limit of `nvx::guest_restore`, which
    /// refused it with this error.
    Record(GuestRestoreError),
    /// The Activate's record is a cold boot's, without the restore section of a clone's.
    Cold,
    /// The Activate's record expects other resources than the clone has.
    Resources {
        /// The CPU count and the memory size that the record expects.
        expected: ExpectedResources,
        /// The CPU count and the memory size that the clone has, as its repair reports them: the
        /// number of its online CPUs, and the size of its online memory blocks.
        actual: ExpectedResources,
    },
    /// The Activate's record names another transaction than the accepted Activate's.
    Transaction {
        /// The transaction of the accepted Activate.
        accepted: TransactionId,
        /// The transaction that the retry names.
        offered: TransactionId,
    },
    /// The Activate's record names the accepted Activate's transaction, but is not byte-equal to
    /// its record.
    Changed,
    /// The adapter's application of the accepted record failed, wholly or partway, with this
    /// error.
    Failed(Box<dyn Error + Send + Sync>),
    /// The adapter completed a guest restore that did not run: before the guest restore accepted an
    /// Activate, or once it completed one.
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
            Self::Reset => {
                formatter.write_str("the session was reset again before the activation was ready")
            }
            Self::Request => {
                formatter.write_str("the host sent a request before the activation was ready")
            }
            Self::Record(error) => write!(formatter, "the Activate's record is refused: {error}"),
            Self::Cold => formatter
                .write_str("the Activate's record is a cold boot's, without a restore section"),
            Self::Resources { expected, actual } => write!(
                formatter,
                "the Activate's record expects {} CPUs and {} bytes of memory, and the clone has \
                 {} CPUs and {} bytes",
                expected.cpus, expected.memory, actual.cpus, actual.memory
            ),
            Self::Transaction { accepted, offered } => write!(
                formatter,
                "the Activate names the transaction {offered}, after an Activate of the \
                 transaction {accepted}"
            ),
            Self::Changed => formatter.write_str(
                "the Activate changes the record of the accepted Activate's transaction",
            ),
            Self::Failed(error) => write!(formatter, "the record's application failed: {error}"),
            Self::Completion => formatter.write_str("an activation that did not run completed"),
            Self::Start => {
                formatter.write_str("the sandbox started before the activation was ready, or again")
            }
        }
    }
}

/// Why a [`GuestRestore`] failed, finally: the state in which it failed, and its cause.
///
/// Its `Display` form names both, such as `the activation failed in its restricted state: the
/// host sent a request before the activation was ready`. A guest restore keeps its failure,
/// which every later call returns again. The failure has no exit status: the adapter that ends
/// the VM documents its own.
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

/// Where a [`GuestRestore`] is, with the Activate that it accepted, from the activating state on.
#[derive(Clone, Debug)]
enum State {
    /// The transport bootstrap.
    Bootstrap,
    /// The restricted session.
    Restricted,
    /// The adapter applies the accepted Activate's record.
    Activating(Accepted),
    /// Ready, with the mappings pending.
    Ready(Accepted),
    /// The adapter's order started the sandbox.
    Started(Accepted),
    /// The guest restore failed, finally, with this failure.
    Failed(GuestRestoreFailure),
}

impl State {
    /// Returns the state's name.
    fn name(&self) -> GuestRestoreState {
        match self {
            Self::Bootstrap => GuestRestoreState::Bootstrap,
            Self::Restricted => GuestRestoreState::Restricted,
            Self::Activating(_) => GuestRestoreState::Activating,
            Self::Ready(_) => GuestRestoreState::Ready,
            Self::Started(_) => GuestRestoreState::Started,
            Self::Failed(_) => GuestRestoreState::Failed,
        }
    }
}

/// The Activate that a [`GuestRestore`] accepted, against which it checks each retry.
#[derive(Clone, Debug)]
struct Accepted {
    /// The bytes of its record, to which a retry's must be equal.
    record: Box<[u8]>,
    /// The transaction that its record names.
    transaction: TransactionId,
    /// What Ready reports about its guest restore.
    ready: GuestRestoreReady,
}

impl Accepted {
    /// Returns why a retry whose record's bytes are `record`, which are not the accepted record's,
    /// is refused: its record does not decode, or is a cold boot's, as the first Activate's would
    /// have been refused, or it names another transaction, or it changes the record of the same
    /// one.
    fn refusal(&self, record: &[u8]) -> GuestRestoreCause {
        let decoded = match GuestRestoreRecord::decode(record) {
            Ok(decoded) => decoded,
            Err(error) => return GuestRestoreCause::Record(error),
        };
        match decoded.restore() {
            None => GuestRestoreCause::Cold,
            Some(restore) if restore.transaction != self.transaction => {
                GuestRestoreCause::Transaction {
                    accepted: self.transaction,
                    offered: restore.transaction,
                }
            }
            Some(_) => GuestRestoreCause::Changed,
        }
    }
}
