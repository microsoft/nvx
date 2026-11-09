//! The activation of a repaired clone, in the caller's process: [`Activation`], the state machine
//! through which the clone takes its configuration from the activation record of its restore
//! packet, and through which its control session reports the activation to the host, and
//! [`apply`](fn@apply), which activates the clone's root and workload with that configuration, as
//! the [module](super#activation) describes.

mod apply;
#[cfg(test)]
mod tests;

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{AdmissionFence, Repaired};
use crate::activation::{ActivationError, ActivationRecord, ExpectedResources, ReadyActivation};

pub use self::apply::{
    Applied, AppliedTeardownError, ApplyError, MACHINE_ID_FILE, NetworkError, RESOLVER_FILE,
    RootTemplate, apply,
};

/// The activation of a repaired clone: the state machine, without I/O and independent of any
/// framing, through which the clone takes its configuration from the activation record that its
/// restore packet carried, behind the caller's fence of workload admission, which it opens once
/// the activation is ready, and through which the clone's control session reports the activation
/// to the host; see the [module](super#activation).
///
/// The adapter takes the record with [`accept`](Self::accept), which decodes and validates it,
/// as soon as the clone runs, without waiting for the host, applies it with `apply`, whose waits
/// end by the activation's [`deadline`](Self::deadline), passes the activation what the session
/// received while the application ran, which came before Ready, then reports the result through
/// [`complete`](Self::complete), whose Ready answers each of the host's requests for it that
/// [joined](ActivationAction::Join) the application, and it reports the start of the sandbox,
/// which its own order takes once the activation is ready, through [`start`](Self::start). It
/// passes the activation what it receives on the session, each an [`ActivationInput`], through
/// [`input`](Self::input), and does what the [`ActivationAction`] that it returns says. The
/// adapter's loop wakes by the deadline, and [ticks](Self::tick) the activation then, whether
/// input arrives or not, and whether its output drains or not.
///
/// The activation is ready once its application succeeded and the host asked for Ready, which
/// the host does first in each of its sessions, so that the activation is ready only once a Ready
/// answers the host: an application that completes before the host's request leaves the
/// activation [applied](ActivationState::Applied), and the host's next request for Ready makes it
/// ready. Until then, the deadline holds, so that a clone whose host never asks ends, and any
/// other request of the host fails the activation.
///
/// It goes through the [`ActivationState`]s in order, and each takes these inputs:
///
/// | State | Reset | Flow | Await | Request |
/// | --- | --- | --- | --- | --- |
/// | [`Pending`](ActivationState::Pending), [`Activating`](ActivationState::Activating) | [`Acknowledge`](ActivationAction::Acknowledge), which abandons the requests for Ready that joined | [`Continue`](ActivationAction::Continue) | [`Join`](ActivationAction::Join) | Fails |
/// | [`Applied`](ActivationState::Applied) | [`Acknowledge`](ActivationAction::Acknowledge) | [`Continue`](ActivationAction::Continue) | [`Ready`](ActivationAction::Ready), which makes the activation ready | Fails |
/// | [`Ready`](ActivationState::Ready), [`Started`](ActivationState::Started) | [`Acknowledge`](ActivationAction::Acknowledge), a later session | [`Continue`](ActivationAction::Continue) | [`Ready`](ActivationAction::Ready) | [`Serve`](ActivationAction::Serve) |
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
///     /// The host's request for the clone's Ready, the first message of each of its sessions.
///     Await,
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
/// /// Activates the clone whose activation is `activation`, and whose template captured
/// /// `template`, the inputs of its root's assembly, and serves its control session until it
/// /// fails.
/// fn serve<F: AdmissionFence>(
///     agent: &mut impl Agent,
///     template: &RootTemplate,
///     mut activation: Activation<F>,
/// ) -> Result<(), ActivationFailure> {
///     // The record of the restore packet is valid, and nothing happened before: the
///     // activation's side effects start here, and the fence opens once they all succeeded. The
///     // application waits for the devices until the activation's deadline at the latest, so
///     // that it ends by then.
///     let record = activation.accept(Instant::now())?;
///     let applied = clone::apply(&record, template, activation.deadline());
///     // What the host sent meanwhile came before Ready: a request for Ready joins the
///     // application, a reset abandons those of the earlier session, and any other request fails
///     // the activation, which drops what the application assembled.
///     let mut joined = 0;
///     while let Some(received) = agent.receive(Some(Instant::now())) {
///         match activation.input(Instant::now(), input(&received))? {
///             ActivationAction::Acknowledge => {
///                 agent.acknowledge();
///                 joined = 0;
///             }
///             ActivationAction::Join => joined += 1,
///             _ => {}
///         }
///     }
///     let ready = match applied {
///         Ok(applied) => {
///             agent.keep(applied);
///             activation.complete(Instant::now(), Ok(()))?
///         }
///         // A failed application fails the activation, which nothing retries.
///         Err(error) => activation.complete(Instant::now(), Err(error.into()))?,
///     };
///     // Ready answers each request for it that joined the application, which made the
///     // activation ready. Without one, the host's next request for Ready makes it ready.
///     for _ in 0..joined {
///         agent.ready(ready);
///     }
///     loop {
///         let Some(received) = agent.receive(activation.deadline()) else {
///             // Until the activation is ready, its deadline fails it.
///             activation.tick(Instant::now())?;
///             continue;
///         };
///         match activation.input(Instant::now(), input(&received))? {
///             ActivationAction::Acknowledge => agent.acknowledge(),
///             ActivationAction::Continue | ActivationAction::Join => {}
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
/// fn input(received: &Received) -> ActivationInput {
///     match received {
///         Received::Reset => ActivationInput::Reset,
///         Received::Partial => ActivationInput::Flow,
///         Received::Await => ActivationInput::Await,
///         Received::Other(_) => ActivationInput::Request,
///     }
/// }
/// ```
pub struct Activation<F> {
    /// The caller's fence of workload admission, closed until the activation is ready.
    fence: F,
    /// The resources that the clone has, as its repair reports them, which the record must
    /// expect.
    resources: ExpectedResources,
    /// The bytes of the activation record of the clone's restore packet, if it carried one, until
    /// the activation accepts it.
    record: Option<Box<[u8]>>,
    /// When the activation fails unless it is ready, or `None` if no instant is that late.
    deadline: Option<Instant>,
    /// Whether a request for Ready of the host's current session joined the activation, so that
    /// its completion makes it ready.
    awaited: bool,
    /// Where the activation is.
    state: State,
}

impl<F: AdmissionFence> Activation<F> {
    /// Returns the activation of the clone that `repaired` describes, with the activation record
    /// of its restore packet, [`Repaired::activation_record`], [pending](ActivationState::Pending),
    /// behind the caller's `fence`, which [`Restored::into_parts`](super::Restored::into_parts)
    /// handed over closed, and which the activation opens once it is ready.
    ///
    /// The activation must be ready by its deadline: `timeout`, which the caller chooses, after
    /// the repair acknowledged the restore, as [`Repaired::acknowledged_at`] tells, or after `now`
    /// for a restore that the host did not gate until an acknowledgement, whose repair
    /// acknowledged nothing. Nothing extends it. A deadline that no instant can hold never passes.
    pub fn new(repaired: &Repaired, fence: F, timeout: Duration, now: Instant) -> Self {
        let resources = ExpectedResources {
            cpus: repaired.cpus(),
            memory: repaired.memory(),
        };
        Self::with_resources(
            resources,
            repaired.activation_record(),
            repaired.acknowledged_at(),
            fence,
            timeout,
            now,
        )
    }

    /// Returns the activation of a clone that has `resources`, whose restore packet carried the
    /// activation record `record`, if it carried one, and whose repair acknowledged its restore at
    /// `acknowledged_at`, if it did, as [`new`](Self::new) does with those of a [`Repaired`]
    /// clone: for a caller that has them otherwise, such as the tests of an adapter's framing,
    /// which run its session without a restore.
    pub fn with_resources(
        resources: ExpectedResources,
        record: Option<&[u8]>,
        acknowledged_at: Option<Instant>,
        fence: F,
        timeout: Duration,
        now: Instant,
    ) -> Self {
        let start = acknowledged_at.unwrap_or(now);
        Self {
            fence,
            resources,
            record: record.map(Box::from),
            deadline: start.checked_add(timeout),
            awaited: false,
            state: State::Pending,
        }
    }

    /// Accepts at `now` the activation record that the clone's restore packet carried, once it
    /// decoded and validated it, and returns it, for the adapter to apply with `apply`: nothing
    /// has a side effect before. The activation is [activating](ActivationState::Activating)
    /// next.
    ///
    /// The packet must carry a record, or the activation fails with [`ActivationCause::Missing`]:
    /// a restore of a platform snapshot hands its clone the record that its launcher supplied,
    /// which NVX's launch of a clone always supplies. [`ActivationRecord::decode`] refuses a record
    /// with [`ActivationCause::Record`] unless it is canonical and keeps every limit; the record
    /// must be a clone's, with a restore section, or the activation fails with
    /// [`ActivationCause::Cold`]; and it must expect the resources that the clone has, the CPU
    /// count and the memory size that its repair reports, or the activation fails with
    /// [`ActivationCause::Resources`]. Past the deadline, the activation fails with
    /// [`ActivationCause::Expired`] instead, and once it accepted the record, another acceptance
    /// fails it with [`ActivationCause::Completion`]. The activation keeps nothing of the record
    /// but what Ready reports about it, its transaction ID.
    pub fn accept(&mut self, now: Instant) -> Result<Box<ActivationRecord>, ActivationFailure> {
        self.check(now)?;
        if !matches!(self.state, State::Pending) {
            return Err(self.fail(ActivationCause::Completion));
        }
        let Some(record) = self.record.take() else {
            return Err(self.fail(ActivationCause::Missing));
        };
        let decoded = match ActivationRecord::decode(&record) {
            Ok(decoded) => decoded,
            Err(error) => return Err(self.fail(ActivationCause::Record(error))),
        };
        if decoded.restore().is_none() {
            return Err(self.fail(ActivationCause::Cold));
        }
        let expected = decoded.configuration().resources;
        if expected != self.resources {
            let actual = self.resources;
            return Err(self.fail(ActivationCause::Resources { expected, actual }));
        }
        self.state = State::Activating(ReadyActivation::of(&decoded));
        Ok(Box::new(decoded))
    }

    /// Takes `input`, which the adapter received at `now` on the clone's control session, and
    /// returns what the adapter does next, as the [type](Self) tabulates it, or fails the
    /// activation.
    ///
    /// Until the activation is ready, it fails once `now` reaches its deadline, whatever the
    /// input, with [`ActivationCause::Expired`]. Otherwise, in any state, it takes flow control,
    /// and each reset of the session: the one that the restore queued, and any later one, such as
    /// once the host's attachment ended, which the adapter acknowledges, abandoning the work of
    /// the earlier epoch, the host's requests for Ready that joined the activation in it included.
    /// A reset changes nothing else of the activation, whose record came with the restore, before
    /// the host could send anything, so that each session gets the same Ready.
    ///
    /// The host's request for the clone's Ready, [`ActivationInput::Await`], comes first in each
    /// of its sessions: before the activation applied the record, it joins the activation, whose
    /// [completion](Self::complete) answers it, unless a reset abandoned it; once the activation
    /// applied the record, it makes the activation ready, which opens the fence, and gets what
    /// Ready reports; and once the activation is ready, or started, it gets the same, without
    /// activating again. Any other request before the activation is ready fails it with
    /// [`ActivationCause::Request`], since the host sends its mapping table and the start only
    /// once Ready came, and once the activation is ready, or started, every other request goes to
    /// the adapter's order, as after a cold boot.
    pub fn input(
        &mut self,
        now: Instant,
        input: ActivationInput,
    ) -> Result<ActivationAction, ActivationFailure> {
        self.check(now)?;
        Ok(match input {
            ActivationInput::Flow => ActivationAction::Continue,
            ActivationInput::Reset => {
                self.awaited = false;
                ActivationAction::Acknowledge
            }
            ActivationInput::Await => match self.state {
                State::Applied(ready) => {
                    self.state = State::Ready(ready);
                    self.fence.open();
                    ActivationAction::Ready(ready)
                }
                State::Ready(ready) | State::Started(ready) => ActivationAction::Ready(ready),
                _ => {
                    self.awaited = true;
                    ActivationAction::Join
                }
            },
            ActivationInput::Request => {
                if self.established().is_none() {
                    return Err(self.fail(ActivationCause::Request));
                }
                ActivationAction::Serve
            }
        })
    }

    /// Completes the activation of the accepted record at `now`, with the `result` of the
    /// adapter's application of the record, and returns what Ready reports: that the activation
    /// applied, with the record's transaction ID, which answers each of the host's requests for
    /// Ready that joined the activation in the current session.
    ///
    /// A result that succeeded by the deadline makes the activation ready once a request for
    /// Ready joined it in the current session: it opens the fence, which no input before opened.
    /// Without one, it leaves the activation [applied](ActivationState::Applied), with the fence
    /// closed and the deadline holding, until the host's next request for Ready, which makes it
    /// ready. A failure, which may have applied part of the record, is final, with
    /// [`ActivationCause::Failed`], so that nothing activates the record again, and the fence
    /// stays closed. Past the deadline, the activation fails with [`ActivationCause::Expired`]
    /// instead, whatever the result. Without an activation that runs, it fails with
    /// [`ActivationCause::Completion`].
    pub fn complete(
        &mut self,
        now: Instant,
        result: Result<(), Box<dyn Error + Send + Sync>>,
    ) -> Result<ReadyActivation, ActivationFailure> {
        self.check(now)?;
        let State::Activating(ready) = self.state else {
            return Err(self.fail(ActivationCause::Completion));
        };
        if let Err(error) = result {
            return Err(self.fail(ActivationCause::Failed(error)));
        }
        if self.awaited {
            self.state = State::Ready(ready);
            self.fence.open();
        } else {
            self.state = State::Applied(ready);
        }
        Ok(ready)
    }

    /// Records that the adapter's order started the sandbox, once the activation is ready: the
    /// host's start, after the mapping table that the record announced, as after a cold boot.
    ///
    /// A start before the activation is ready, or a second one, fails with
    /// [`ActivationCause::Start`].
    pub fn start(&mut self) -> Result<(), ActivationFailure> {
        let ready = match &self.state {
            State::Failed(failure) => return Err(failure.clone()),
            State::Ready(ready) => *ready,
            _ => return Err(self.fail(ActivationCause::Start)),
        };
        self.state = State::Started(ready);
        Ok(())
    }

    /// Fails the activation with [`ActivationCause::Expired`] once `now` reaches its deadline,
    /// unless it is ready, as each input does, so that the adapter wakes by the
    /// [deadline](Self::deadline) and ticks the activation then, whether input arrives or not.
    pub fn tick(&mut self, now: Instant) -> Result<(), ActivationFailure> {
        self.check(now)
    }

    /// Returns when the activation fails unless it is ready, for the adapter to
    /// [tick](Self::tick) it then, or `None` once it is ready, or failed, or if no instant is that
    /// late.
    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Pending | State::Activating(_) | State::Applied(_) => self.deadline,
            State::Ready(_) | State::Started(_) | State::Failed(_) => None,
        }
    }

    /// Returns where the activation is.
    pub fn state(&self) -> ActivationState {
        self.state.name()
    }

    /// Returns what Ready reports about the established activation, once it is ready: that the
    /// activation applied, with the record's transaction ID, which every later session gets
    /// without activating again.
    pub fn established(&self) -> Option<ReadyActivation> {
        match &self.state {
            State::Ready(ready) | State::Started(ready) => Some(*ready),
            _ => None,
        }
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
/// whatever its framing: the session's reset, flow control, the host's request for the clone's
/// Ready, or another request of the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActivationInput {
    /// The NVXS session's reset, a [`GuestEvent::Reset`](crate::protocol::nvxs::GuestEvent::Reset),
    /// which starts a new epoch: the one that the restore queued for the session that the guest
    /// attached before the capture, or a later one, such as once the host's attachment ended.
    Reset,
    /// Flow control, which carries no request: bytes of a message that they do not complete, such
    /// as a DATA record's payload, or a credit that the adapter returned to its receive window.
    Flow,
    /// The host's request for the clone's Ready, the first request of each of the host's
    /// sessions: OpenVMM's broker refuses the guest's records while no host is attached, which the
    /// guest cannot tell, so the host asks for Ready rather than the guest sending it unasked, as
    /// the host asks a guest agent for its readiness with NVXC's `PING`.
    Await,
    /// Any other request of the host, such as a part of the mapping table, the start of the
    /// sandbox, an execution, or a probe of the adapter's framing.
    Request,
}

/// What the adapter does next, which [`Activation::input`] returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActivationAction {
    /// The activation took the session's reset: the adapter acknowledges it with its receive
    /// window, unless its transport acknowledged it already, and abandons the work of the earlier
    /// epoch, the host's requests for Ready that joined the activation in it included, since the
    /// host reads no answer of an earlier session.
    Acknowledge,
    /// The activation took the input, which needs nothing more of the adapter.
    Continue,
    /// The host's request for Ready came before the activation was ready, and joined it: what
    /// [`Activation::complete`] returns answers it, as the adapter's framing answers each request,
    /// unless a reset of the session abandons it first.
    Join,
    /// The host's request for Ready, once the activation applied the record: the adapter answers
    /// it with Ready, which reports this result, without activating again. The first, once the
    /// activation is applied, makes it ready.
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
    /// The activation record of the clone's restore packet awaits its acceptance, which decodes
    /// and validates it.
    Pending,
    /// The adapter applies the accepted record.
    Activating,
    /// The adapter applied the accepted record before the host asked for Ready in its current
    /// session: the activation awaits the host's request for Ready, which makes it ready.
    Applied,
    /// Ready, with the mappings pending: the activation completed, and the host asked for Ready,
    /// which opened the fence, and got what Ready reports. The host's mapping table, which the
    /// record announced, and the start of the sandbox follow in the adapter's order, as after a
    /// cold boot.
    Ready,
    /// The adapter's order started the sandbox.
    Started,
    /// The activation failed, finally.
    Failed,
}

impl fmt::Display for ActivationState {
    /// Writes the state's name, such as `activating`.
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

/// Why an [`Activation`] failed, which an [`ActivationFailure`] holds.
#[derive(Debug)]
#[non_exhaustive]
pub enum ActivationCause {
    /// The deadline passed before the activation was ready.
    Expired,
    /// The clone's restore packet carried no activation record.
    Missing,
    /// The host sent a request before the activation was ready, other than its request for
    /// Ready.
    Request,
    /// The record does not decode, or breaks a limit of `nvx::activation`, which refused it with
    /// this error.
    Record(ActivationError),
    /// The record is a cold boot's, without the restore section of a clone's.
    Cold,
    /// The record expects other resources than the clone has.
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
    /// The adapter accepted the record of an activation that had accepted it already, or
    /// completed an activation that did not run: before it accepted the record, or once it
    /// completed.
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
            Self::Missing => {
                formatter.write_str("the clone's restore packet carried no activation record")
            }
            Self::Request => {
                formatter.write_str("the host sent a request before the activation was ready")
            }
            Self::Record(error) => {
                write!(
                    formatter,
                    "the restore packet's activation record is refused: {error}"
                )
            }
            Self::Cold => formatter.write_str(
                "the restore packet's activation record is a cold boot's, without a restore \
                 section",
            ),
            Self::Resources { expected, actual } => write!(
                formatter,
                "the activation record expects {} CPUs and {} bytes of memory, and the clone has \
                 {} CPUs and {} bytes",
                expected.cpus, expected.memory, actual.cpus, actual.memory
            ),
            Self::Failed(error) => write!(formatter, "the record's application failed: {error}"),
            Self::Completion => formatter.write_str(
                "the activation's record was accepted again, or an activation that did not run \
                 completed",
            ),
            Self::Start => {
                formatter.write_str("the sandbox started before the activation was ready, or again")
            }
        }
    }
}

/// Why an [`Activation`] failed, finally: the state in which it failed, and its cause.
///
/// Its `Display` form names both, such as `the activation failed in its activating state: the
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

/// Where an [`Activation`] is, with what Ready reports about its record, from the activating state
/// on.
#[derive(Clone, Debug)]
enum State {
    /// The record awaits its acceptance.
    Pending,
    /// The adapter applies the accepted record.
    Activating(ReadyActivation),
    /// The adapter applied the accepted record, which awaits the host's request for Ready.
    Applied(ReadyActivation),
    /// Ready, with the mappings pending.
    Ready(ReadyActivation),
    /// The adapter's order started the sandbox.
    Started(ReadyActivation),
    /// The activation failed, finally, with this failure.
    Failed(ActivationFailure),
}

impl State {
    /// Returns the state's name.
    fn name(&self) -> ActivationState {
        match self {
            Self::Pending => ActivationState::Pending,
            Self::Activating(_) => ActivationState::Activating,
            Self::Applied(_) => ActivationState::Applied,
            Self::Ready(_) => ActivationState::Ready,
            Self::Started(_) => ActivationState::Started,
            Self::Failed(_) => ActivationState::Failed,
        }
    }
}
