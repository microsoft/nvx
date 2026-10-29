//! The activation of a repaired clone, in the caller's process: [`Activation`], the state machine
//! of the clone's restricted control session, through which the clone takes its configuration from
//! the host in one Activate, and [`apply`](fn@apply), which activates the clone's root and workload
//! with that configuration, as the [module](super#activation) describes.

mod apply;
#[cfg(test)]
mod tests;

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{AdmissionFence, Repaired};
use crate::activation::{
    ActivationError, ActivationRecord, ExpectedResources, ReadyActivation, TransactionId,
};

pub use self::apply::{
    Applied, AppliedTeardownError, ApplyError, MACHINE_ID_FILE, NetworkError, RESOLVER_FILE,
    RootTemplate, apply,
};

/// The activation of a repaired clone: the state machine, without I/O and independent of any
/// framing, of the clone's control session, which takes the clone's configuration from the host
/// in one Activate, behind the caller's fence of workload admission, which it opens once the
/// activation is ready; see the [module](super#activation).
///
/// The adapter passes it what it receives on the session, each an [`ActivationInput`], through
/// [`input`](Self::input), and does what the [`ActivationAction`] that it returns says. The
/// adapter applies the record of the Activate that the activation accepted, with
/// [`apply`](fn@apply), whose waits end by the activation's [`deadline`](Self::deadline), passes
/// the activation what the session received while the application ran, which came before Ready,
/// then reports the result through [`complete`](Self::complete), whose Ready answers the Activate
/// and each retry that [joined](ActivationAction::Join) it, and it reports the start of the
/// sandbox, which its own order takes once the activation is ready, through
/// [`start`](Self::start). The adapter's loop wakes by the deadline, and [ticks](Self::tick) the
/// activation then, whether input arrives or not, and whether its output drains or not.
///
/// It goes through the [`ActivationState`]s in order, and each takes these inputs:
///
/// | State | Reset | Flow | Activate | Request |
/// | --- | --- | --- | --- | --- |
/// | [`Bootstrap`](ActivationState::Bootstrap) | [`Acknowledge`](ActivationAction::Acknowledge), restricted next | [`Continue`](ActivationAction::Continue) | Fails | Fails |
/// | [`Restricted`](ActivationState::Restricted) | Fails | [`Continue`](ActivationAction::Continue) | [`Apply`](ActivationAction::Apply), activating next, once its record is valid | Fails |
/// | [`Activating`](ActivationState::Activating) | Fails | [`Continue`](ActivationAction::Continue) | [`Join`](ActivationAction::Join) if byte-equal | Fails |
/// | [`Ready`](ActivationState::Ready), [`Started`](ActivationState::Started) | [`Acknowledge`](ActivationAction::Acknowledge), a later session | [`Continue`](ActivationAction::Continue) | [`Ready`](ActivationAction::Ready) if byte-equal | [`Serve`](ActivationAction::Serve) |
///
/// Every failure is final, and leaves the activation [failed](ActivationState::Failed): every
/// later call returns the same [`ActivationFailure`]. A failure before the activation is ready
/// leaves the fence closed, and one after it leaves the fence open. The activation never powers
/// the VM off: the adapter reports the failure and ends the VM.
///
/// # Example
///
/// ```no_run
/// use std::time::Instant;
///
/// use nvx::activation::ReadyActivation;
/// use nvx::guest::clone::{
///     self, Activation, ActivationAction, ActivationFailure, ActivationInput, AdmissionFence,
///     Applied, RootTemplate,
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
///     /// Keeps the clone's activated root and workload: the identity, if any, in which it runs
///     /// every execution, with the hostname of every execution, the root, with the runtime's
///     /// binds on it, which `Applied::teardown` removes before it tears the root down, and the
///     /// export's root, which `Applied::take_export` hands over, from which it maps the host's
///     /// mapping table.
///     fn keep(&mut self, applied: Applied);
///     /// Queues Ready, which reports `activation` beside the agent's own identities, such as its
///     /// build.
///     fn ready(&mut self, activation: ReadyActivation);
///     /// Serves `message` in the agent's order, as after a cold boot, and returns whether the
///     /// host started the sandbox with it.
///     fn serve(&mut self, message: Vec<u8>) -> bool;
/// }
///
/// /// Serves the control session of the clone whose activation is `activation`, and whose
/// /// template captured `template`, the inputs of its root's assembly, until it fails.
/// fn serve<F: AdmissionFence>(
///     agent: &mut impl Agent,
///     template: &RootTemplate,
///     mut activation: Activation<F>,
/// ) -> Result<(), ActivationFailure> {
///     loop {
///         let Some(received) = agent.receive(activation.deadline()) else {
///             // The deadline passed, unless the clone is ready, which ends the activation.
///             activation.tick(Instant::now())?;
///             continue;
///         };
///         match activation.input(Instant::now(), input(&received))? {
///             ActivationAction::Acknowledge => agent.acknowledge(),
///             ActivationAction::Continue | ActivationAction::Join => {}
///             ActivationAction::Apply(record) => {
///                 // The record is valid, and nothing happened before: the activation's side
///                 // effects start here, and the fence opens once they all succeeded. The
///                 // application waits for the devices until the activation's deadline at the
///                 // latest, so that it ends by then.
///                 let applied = clone::apply(&record, template, activation.deadline());
///                 // What the host sent meanwhile came before Ready: a retry of the Activate,
///                 // byte for byte, joins the application, and anything else, a reset included,
///                 // fails the activation, which drops what the application assembled.
///                 let mut joined = 0;
///                 while let Some(received) = agent.receive(Some(Instant::now())) {
///                     let action = activation.input(Instant::now(), input(&received))?;
///                     if action == ActivationAction::Join {
///                         joined += 1;
///                     }
///                 }
///                 let ready = match applied {
///                     Ok(applied) => {
///                         agent.keep(applied);
///                         activation.complete(Instant::now(), Ok(()))?
///                     }
///                     // A failed application fails the activation: the fence stays closed.
///                     Err(error) => activation.complete(Instant::now(), Err(error.into()))?,
///                 };
///                 // Ready answers the Activate, and each retry that joined its application.
///                 for _ in 0..=joined {
///                     agent.ready(ready);
///                 }
///             }
///             ActivationAction::Ready(ready) => agent.ready(ready),
///             // The activation serves only another message, once it is ready.
///             ActivationAction::Serve => {
///                 if let Received::Other(message) = received
///                     && agent.serve(message)
///                 {
///                     activation.start()?;
///                 }
///             }
///         }
///     }
/// }
///
/// /// Returns the activation's input of `received`.
/// fn input(received: &Received) -> ActivationInput<'_> {
///     match received {
///         Received::Reset => ActivationInput::Reset,
///         Received::Partial => ActivationInput::Flow,
///         Received::Activate(record) => ActivationInput::Activate(record),
///         Received::TooLong(length) => ActivationInput::TooLong(*length),
///         Received::Other(_) => ActivationInput::Request,
///     }
/// }
/// ```
pub struct Activation<F> {
    /// The caller's fence of workload admission, closed until the activation is ready.
    fence: F,
    /// The resources that the clone has, as its repair reports them, which the record of an
    /// Activate must expect.
    resources: ExpectedResources,
    /// When the activation fails unless it is ready, or `None` if no instant is that late.
    deadline: Option<Instant>,
    /// Where the activation is.
    state: State,
}

impl<F: AdmissionFence> Activation<F> {
    /// Returns the activation of the clone that `repaired` describes, in its
    /// [transport bootstrap](ActivationState::Bootstrap), behind the caller's `fence`, which
    /// [`Restored::into_parts`](super::Restored::into_parts) handed over closed, and which the
    /// activation opens once it is ready.
    ///
    /// The activation must be ready by its deadline: `timeout`, which the caller chooses, after
    /// the repair acknowledged the restore, as [`Repaired::acknowledged_at`] tells, or after `now`
    /// for a restore that the host did not gate until an acknowledgement, whose repair
    /// acknowledged nothing. No input or retry extends it. A deadline that no instant can hold
    /// never passes.
    pub fn new(repaired: &Repaired, fence: F, timeout: Duration, now: Instant) -> Self {
        let resources = ExpectedResources {
            cpus: repaired.cpus(),
            memory: repaired.memory(),
        };
        Self::with_resources(resources, repaired.acknowledged_at(), fence, timeout, now)
    }

    /// Returns the activation of a clone that has `resources`, and whose repair acknowledged its
    /// restore at `acknowledged_at`, if it did, as [`new`](Self::new) does with the resources and
    /// the acknowledgement of a [`Repaired`] clone: for a caller that has them otherwise, such as
    /// the tests of an adapter's framing, which run its session without a restore.
    pub fn with_resources(
        resources: ExpectedResources,
        acknowledged_at: Option<Instant>,
        fence: F,
        timeout: Duration,
        now: Instant,
    ) -> Self {
        let start = acknowledged_at.unwrap_or(now);
        Self {
            fence,
            resources,
            deadline: start.checked_add(timeout),
            state: State::Bootstrap,
        }
    }

    /// Takes `input`, which the adapter received at `now` on the clone's control session, and
    /// returns what the adapter does next, as the [type](Self) tabulates it, or fails the
    /// activation.
    ///
    /// Until the activation is ready, it fails once `now` reaches its deadline, whatever the
    /// input, with [`ActivationCause::Expired`]. In order, it then takes:
    ///
    /// 1. In the transport bootstrap, the session's reset, which the restore queued: the adapter
    ///    acknowledges it, and the session is restricted next. Before it, the host can send
    ///    nothing, so an Activate or another request fails with [`ActivationCause::Request`].
    /// 2. While restricted, flow control, and one Activate. The activation decodes its record with
    ///    [`ActivationRecord::decode`], which refuses it with [`ActivationCause::Record`] unless it
    ///    is canonical and keeps every limit, checks that it is a clone's, with a restore
    ///    section, or refuses it with [`ActivationCause::Cold`], and checks that it expects the
    ///    resources that the clone has, the CPU count and the memory size that its repair
    ///    reports, or refuses it with [`ActivationCause::Resources`]. Only then does it accept it,
    ///    and hand the record to the adapter with [`ActivationAction::Apply`]: nothing has a side
    ///    effect before. Any other request fails with [`ActivationCause::Request`], and another
    ///    reset with [`ActivationCause::Reset`].
    /// 3. While activating, flow control, and retries of the Activate: one of the same
    ///    transaction, with a byte-equal record, joins the activation that runs. Any other
    ///    Activate fails: one whose record does not decode, or is a cold boot's, with the cause
    ///    with which the first would have failed, one of another transaction with
    ///    [`ActivationCause::Transaction`], and any other of the same transaction with
    ///    [`ActivationCause::Changed`]. A request or a reset fails as while restricted.
    /// 4. Once ready, or started, everything: a reset starts a later session, which the adapter
    ///    acknowledges, a byte-equal retry of the Activate gets the cached result, what Ready
    ///    reports, without activating again, and any other Activate fails as while activating.
    ///    Every other request goes to the adapter's order, as after a cold boot.
    ///
    /// An Activate whose record the adapter's framing refused unread, as longer than a record can
    /// be, [`ActivationInput::TooLong`], fails as one whose record has that many bytes: in the
    /// transport bootstrap with [`ActivationCause::Request`], and from then on with
    /// [`ActivationCause::Record`].
    pub fn input(
        &mut self,
        now: Instant,
        input: ActivationInput<'_>,
    ) -> Result<ActivationAction, ActivationFailure> {
        self.check(now)?;
        match (self.state.name(), input) {
            (_, ActivationInput::Flow) => Ok(ActivationAction::Continue),
            (ActivationState::Bootstrap, ActivationInput::Reset) => {
                self.state = State::Restricted;
                Ok(ActivationAction::Acknowledge)
            }
            (ActivationState::Ready | ActivationState::Started, ActivationInput::Reset) => {
                Ok(ActivationAction::Acknowledge)
            }
            (_, ActivationInput::Reset) => Err(self.fail(ActivationCause::Reset)),
            (ActivationState::Restricted, ActivationInput::Activate(record)) => self.accept(record),
            (
                ActivationState::Activating | ActivationState::Ready | ActivationState::Started,
                ActivationInput::Activate(record),
            ) => self.retry(record),
            // A record that long never decodes, so its Activate fails as one whose bytes the
            // adapter read would, whether the activation accepted another or not.
            (
                ActivationState::Restricted
                | ActivationState::Activating
                | ActivationState::Ready
                | ActivationState::Started,
                ActivationInput::TooLong(length),
            ) => Err(self.fail(ActivationCause::Record(ActivationError::TooLong(length)))),
            (ActivationState::Ready | ActivationState::Started, ActivationInput::Request) => {
                Ok(ActivationAction::Serve)
            }
            (
                _,
                ActivationInput::Activate(_)
                | ActivationInput::TooLong(_)
                | ActivationInput::Request,
            ) => Err(self.fail(ActivationCause::Request)),
        }
    }

    /// Completes the activation of the accepted Activate's record at `now`, with the `result` of
    /// the adapter's application of the record, and returns what Ready reports: that the
    /// activation applied, with the record's transaction ID.
    ///
    /// A result that succeeded by the deadline makes the activation ready: it opens the fence,
    /// which no input before opened, and the adapter reports Ready. A failure, which may have
    /// applied part of the record, is final, with [`ActivationCause::Failed`], so that no retry
    /// activates the record again, and the fence stays closed. Past the deadline, the activation
    /// fails with [`ActivationCause::Expired`] instead, whatever the result. Without an activation
    /// that runs, it fails with [`ActivationCause::Completion`].
    pub fn complete(
        &mut self,
        now: Instant,
        result: Result<(), Box<dyn Error + Send + Sync>>,
    ) -> Result<ReadyActivation, ActivationFailure> {
        self.check(now)?;
        let accepted = match &self.state {
            State::Activating(accepted) => accepted.clone(),
            _ => return Err(self.fail(ActivationCause::Completion)),
        };
        if let Err(error) = result {
            return Err(self.fail(ActivationCause::Failed(error)));
        }
        let ready = accepted.ready;
        self.state = State::Ready(accepted);
        self.fence.open();
        Ok(ready)
    }

    /// Records that the adapter's order started the sandbox, once the activation is ready: the
    /// host's start, after the mapping table that the record announced, as after a cold boot.
    ///
    /// A start before the activation is ready, or a second one, fails with
    /// [`ActivationCause::Start`].
    pub fn start(&mut self) -> Result<(), ActivationFailure> {
        let accepted = match &self.state {
            State::Failed(failure) => return Err(failure.clone()),
            State::Ready(accepted) => accepted.clone(),
            _ => return Err(self.fail(ActivationCause::Start)),
        };
        self.state = State::Started(accepted);
        Ok(())
    }

    /// Fails the activation with [`ActivationCause::Expired`] once `now` reaches its deadline,
    /// unless it is ready, as each input does, so that the adapter wakes by the
    /// [deadline](Self::deadline) and ticks the activation then, whether input arrives or not.
    pub fn tick(&mut self, now: Instant) -> Result<(), ActivationFailure> {
        self.check(now)
    }

    /// Returns when the activation fails unless it is ready, for the adapter to
    /// [tick](Self::tick) it then, or `None` once it is ready or failed, or if no instant is that
    /// late.
    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Bootstrap | State::Restricted | State::Activating(_) => self.deadline,
            State::Ready(_) | State::Started(_) | State::Failed(_) => None,
        }
    }

    /// Returns where the activation is.
    pub fn state(&self) -> ActivationState {
        self.state.name()
    }

    /// Returns what Ready reports about the established activation, once it is ready: that the
    /// activation applied, with the record's transaction ID, which a later session gets without
    /// activating again.
    pub fn established(&self) -> Option<ReadyActivation> {
        match &self.state {
            State::Ready(accepted) | State::Started(accepted) => Some(accepted.ready),
            _ => None,
        }
    }

    /// Accepts the host's first Activate, whose record's bytes are `record`, once it decoded and
    /// validated it: a clone's record, with a restore section, that expects the clone's resources.
    fn accept(&mut self, record: &[u8]) -> Result<ActivationAction, ActivationFailure> {
        let decoded = match ActivationRecord::decode(record) {
            Ok(decoded) => decoded,
            Err(error) => return Err(self.fail(ActivationCause::Record(error))),
        };
        let Some(restore) = decoded.restore() else {
            return Err(self.fail(ActivationCause::Cold));
        };
        let expected = decoded.configuration().resources;
        if expected != self.resources {
            let actual = self.resources;
            return Err(self.fail(ActivationCause::Resources { expected, actual }));
        }
        self.state = State::Activating(Accepted {
            record: record.into(),
            transaction: restore.transaction,
            ready: ReadyActivation::of(&decoded),
        });
        Ok(ActivationAction::Apply(Box::new(decoded)))
    }

    /// Takes a retry of the accepted Activate, whose record's bytes are `record`: a byte-equal
    /// record joins the activation that runs, or gets its cached result once it is ready, and
    /// any other fails the activation.
    fn retry(&mut self, record: &[u8]) -> Result<ActivationAction, ActivationFailure> {
        let (accepted, ready) = match &self.state {
            State::Activating(accepted) => (accepted, false),
            State::Ready(accepted) | State::Started(accepted) => (accepted, true),
            _ => return Err(self.fail(ActivationCause::Request)),
        };
        if *accepted.record == *record {
            return Ok(if ready {
                ActivationAction::Ready(accepted.ready)
            } else {
                ActivationAction::Join
            });
        }
        let cause = accepted.refusal(record);
        Err(self.fail(cause))
    }

    /// Fails the activation with the failure that it already had, or with
    /// [`ActivationCause::Expired`] once `now` reaches its deadline, unless it is ready.
    fn check(&mut self, now: Instant) -> Result<(), ActivationFailure> {
        if let State::Failed(failure) = &self.state {
            return Err(failure.clone());
        }
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            return Err(self.fail(ActivationCause::Expired));
        }
        Ok(())
    }

    /// Fails the activation in its state, because of `cause`, and returns the failure, unless it
    /// failed already, which it then returns.
    fn fail(&mut self, cause: ActivationCause) -> ActivationFailure {
        if let State::Failed(failure) = &self.state {
            return failure.clone();
        }
        let failure = ActivationFailure {
            state: self.state.name(),
            cause: Arc::new(cause),
        };
        self.state = State::Failed(failure.clone());
        failure
    }
}

impl<F> fmt::Debug for Activation<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Activation")
            .field("state", &self.state.name())
            .field("resources", &self.resources)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// What the adapter received on a clone's control session, in the terms of its [`Activation`],
/// whatever its framing: the session's reset, flow control, the host's Activate, or another
/// request of the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationInput<'a> {
    /// The NVXS session's reset, a [`GuestEvent::Reset`](crate::protocol::nvxs::GuestEvent::Reset),
    /// which starts a new epoch: the one that the restore queued for the session that the guest
    /// attached before the capture, or a later one, such as once the host's attachment ended.
    Reset,
    /// Flow control, which carries no request: bytes of a message that they do not complete, such
    /// as a DATA record's payload, or a credit that the adapter returned to its receive window.
    Flow,
    /// The host's Activate, with the bytes of its activation record, the canonical encoding of an
    /// [`ActivationRecord`].
    Activate(&'a [u8]),
    /// The host's Activate, whose record the adapter's framing refused before it took any byte of
    /// it, since its header announced this many bytes, more than
    /// [`MAX_RECORD_BYTES`](crate::activation::MAX_RECORD_BYTES): the activation takes it as an
    /// Activate whose record's bytes are that long, which [`ActivationRecord::decode`] refuses
    /// with [`ActivationError::TooLong`].
    TooLong(usize),
    /// Any other request of the host, such as a part of the mapping table, the start of the
    /// sandbox, an execution, or a probe of the adapter's framing.
    Request,
}

/// What the adapter does next, which [`Activation::input`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationAction {
    /// The activation took the session's reset: the adapter acknowledges it with its receive
    /// window, unless its transport acknowledged it already, and abandons the work of the earlier
    /// epoch.
    Acknowledge,
    /// The activation took the input, which needs nothing more of the adapter.
    Continue,
    /// The activation accepted the host's Activate, whose record decoded and validated, and
    /// which nothing before had any side effect of: the adapter applies the record with
    /// [`apply`](fn@apply), which activates the root and the workload with its settings, passes
    /// the activation what the session received meanwhile, which came before Ready, then reports
    /// the result to [`Activation::complete`].
    Apply(Box<ActivationRecord>),
    /// A retry of the accepted Activate joined the activation that runs: what
    /// [`Activation::complete`] returns answers it too, as the adapter's framing answers each
    /// request.
    Join,
    /// A retry of the accepted Activate, once the activation is ready: the adapter answers it with
    /// Ready, which reports this cached result, without activating again.
    Ready(ReadyActivation),
    /// A request once the activation is ready: the adapter serves it in its order, as after a
    /// cold boot.
    Serve,
}

/// The state of an [`Activation`], in the order in which it goes through them, which an
/// [`ActivationFailure`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ActivationState {
    /// The transport bootstrap: the clone awaits the reset of its NVXS session that the restore
    /// queued, which the adapter acknowledges.
    Bootstrap,
    /// The restricted session: it takes only flow control and one Activate.
    Restricted,
    /// The adapter applies the accepted Activate's record.
    Activating,
    /// Ready, with the mappings pending: the activation completed, which opened the fence, and
    /// returned what Ready reports. The host's mapping table, which the record announced, and the
    /// start of the sandbox follow in the adapter's order, as after a cold boot.
    Ready,
    /// The adapter's order started the sandbox.
    Started,
    /// The activation failed, finally.
    Failed,
}

impl fmt::Display for ActivationState {
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

/// Why an [`Activation`] failed, which an [`ActivationFailure`] holds.
#[derive(Debug)]
#[non_exhaustive]
pub enum ActivationCause {
    /// The deadline passed before the activation was ready.
    Expired,
    /// The session was reset after its bootstrap, before the activation was ready.
    Reset,
    /// The host sent a request that the state does not take, before the activation was ready:
    /// any before the session's bootstrap, and any but one Activate, and its retries, after it.
    Request,
    /// The Activate's record does not decode, or breaks a limit of `nvx::activation`, which
    /// refused it with this error.
    Record(ActivationError),
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
    /// The adapter completed an activation that did not run: before the activation accepted an
    /// Activate, or once it completed one.
    Completion,
    /// The adapter's order started the sandbox before the activation was ready, or again.
    Start,
}

impl fmt::Display for ActivationCause {
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

/// Why an [`Activation`] failed, finally: the state in which it failed, and its cause.
///
/// Its `Display` form names both, such as `the activation failed in its restricted state: the
/// host sent a request before the activation was ready`. An activation keeps its failure, which
/// every later call returns again. The failure has no exit status: the adapter that ends the VM
/// documents its own.
#[derive(Clone, Debug)]
pub struct ActivationFailure {
    /// The state in which the activation failed.
    state: ActivationState,
    /// Why it failed.
    cause: Arc<ActivationCause>,
}

impl ActivationFailure {
    /// Returns the state in which the activation failed, which is never
    /// [`ActivationState::Failed`].
    pub fn state(&self) -> ActivationState {
        self.state
    }

    /// Returns why it failed.
    pub fn cause(&self) -> &ActivationCause {
        &self.cause
    }
}

impl fmt::Display for ActivationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the activation failed in its {} state: {}",
            self.state, self.cause
        )
    }
}

impl Error for ActivationFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self.cause.as_ref() {
            ActivationCause::Record(error) => Some(error),
            ActivationCause::Failed(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// Where an [`Activation`] is, with the Activate that it accepted, from the activating state on.
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
    /// The activation failed, finally, with this failure.
    Failed(ActivationFailure),
}

impl State {
    /// Returns the state's name.
    fn name(&self) -> ActivationState {
        match self {
            Self::Bootstrap => ActivationState::Bootstrap,
            Self::Restricted => ActivationState::Restricted,
            Self::Activating(_) => ActivationState::Activating,
            Self::Ready(_) => ActivationState::Ready,
            Self::Started(_) => ActivationState::Started,
            Self::Failed(_) => ActivationState::Failed,
        }
    }
}

/// The Activate that an [`Activation`] accepted, against which it checks each retry.
#[derive(Clone, Debug)]
struct Accepted {
    /// The bytes of its record, to which a retry's must be equal.
    record: Box<[u8]>,
    /// The transaction that its record names.
    transaction: TransactionId,
    /// What Ready reports about its activation.
    ready: ReadyActivation,
}

impl Accepted {
    /// Returns why a retry whose record's bytes are `record`, which are not the accepted record's,
    /// is refused: its record does not decode, or is a cold boot's, as the first Activate's would
    /// have been refused, or it names another transaction, or it changes the record of the same
    /// one.
    fn refusal(&self, record: &[u8]) -> ActivationCause {
        let decoded = match ActivationRecord::decode(record) {
            Ok(decoded) => decoded,
            Err(error) => return ActivationCause::Record(error),
        };
        match decoded.restore() {
            None => ActivationCause::Cold,
            Some(restore) if restore.transaction != self.transaction => {
                ActivationCause::Transaction {
                    accepted: self.transaction,
                    offered: restore.transaction,
                }
            }
            Some(_) => ActivationCause::Changed,
        }
    }
}
