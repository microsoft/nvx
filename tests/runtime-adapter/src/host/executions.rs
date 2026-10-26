//! The host program's driver of executions: the host's side of each, a [`HostExecution`], over one
//! attachment of a [`Client`] to the control session.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Instant;

use nvx::exec::{Key, Timers, Unit, Violation};
use nvx::host::nvxs::HostConnectionError;

use super::{Client, ClientError};
use crate::exec::{GuestMessage, HostExecution, MAX_EXECUTIONS, Start, name};
use crate::frame::{BodyError, Message};

/// The most frames of each output pool of an execution of [`Executions`], whose output windows
/// count frames: then the credits that the driver sends for the messages that the guest of
/// [`MAX_EXECUTIONS`] executions may send ahead of its own credit fit the guest's receive window,
/// `nvx::protocol::OUTER_MIN_RECEIVE_WINDOW`, several times over.
pub const MAX_OUTPUT_FRAMES: u32 = 32;

/// A failure of [`Executions`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ExecutionsError {
    /// The client failed.
    Client(ClientError),
    /// A start whose encoding does not decode, which the driver did not send.
    Start(BodyError),
    /// A start of an execution whose ID the driver started already, which it did not send.
    Duplicate(u64),
    /// A start that says that the host sends the program's input, which the driver does not
    /// send, and so did not send the start.
    Input,
    /// A start whose output windows count bytes, or more than [`MAX_OUTPUT_FRAMES`] frames,
    /// which the driver did not send.
    Window,
    /// A start beyond [`MAX_EXECUTIONS`] executions of the driver, which it did not send.
    Full,
    /// A start once the driver ran its executions, which it did not send.
    Ran,
    /// The guest sent a message about an execution whose body does not decode.
    Body(BodyError),
    /// The guest sent a message of this kind, which is about no execution.
    Unexpected(u8),
    /// The guest sent a message about an execution that the driver did not start.
    Unknown(Key),
    /// The host's side of the execution refused the guest's message about it.
    Violation(Key, Violation),
    /// The deadline passed before every execution finished.
    TimedOut,
}

impl fmt::Display for ExecutionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Start(error) => write!(formatter, "the host refuses to send the start: {error}"),
            Self::Duplicate(id) => write!(formatter, "the host starts execution {id} twice"),
            Self::Input => formatter.write_str("the host sends no execution's input"),
            Self::Window => write!(
                formatter,
                "the host takes output windows of at most {MAX_OUTPUT_FRAMES} frames"
            ),
            Self::Full => write!(
                formatter,
                "the host runs at most {MAX_EXECUTIONS} executions at once"
            ),
            Self::Ran => {
                formatter.write_str("the host starts no execution once it ran its executions")
            }
            Self::Body(error) => error.fmt(formatter),
            Self::Unexpected(kind) => write!(
                formatter,
                "the guest sent a message of kind {kind}, which is about no execution"
            ),
            Self::Unknown(key) => write!(
                formatter,
                "the guest sent a message about execution {}, which the host did not start",
                name(*key)
            ),
            Self::Violation(key, violation) => write!(
                formatter,
                "the guest's message about execution {} breaks the engine's rules: {violation}",
                name(*key)
            ),
            Self::TimedOut => formatter.write_str("the executions did not finish in time"),
        }
    }
}

impl std::error::Error for ExecutionsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::Start(error) | Self::Body(error) => Some(error),
            Self::Violation(_, violation) => Some(violation),
            Self::Duplicate(_)
            | Self::Input
            | Self::Window
            | Self::Full
            | Self::Ran
            | Self::Unexpected(_)
            | Self::Unknown(_)
            | Self::TimedOut => None,
        }
    }
}

impl From<ClientError> for ExecutionsError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

/// The host's side of the executions that the host program runs over one attachment of the control
/// session: a [`HostExecution`] for each, by ID, whose sink takes the program's output as it
/// arrives.
///
/// [`start`](Self::start) sends the start of an execution, and [`run`](Self::run) carries the
/// guest's messages to the executions, and theirs to the guest, until every execution finished.
/// A driver runs one batch: it starts every execution before it runs them, and none after. The
/// guest may still send messages about the executions of a run that gave up on one or failed, so
/// a later batch takes an attachment of its own, as each command of the host program does, and a
/// stop in the same attachment skips them, as [`Client::stop_skipping`] does with
/// [`is_about`](Self::is_about).
///
/// One thread runs them all: each receive waits at most until the earliest timer of the
/// executions, which so fires in time whatever the guest sends, and the guest's messages about one
/// execution never wait for another's. Each send waits, as the client's do, until the transport
/// takes the message or the run's deadline passes, and no timer fires meanwhile, so a guest that
/// stops reading the session holds the run until its deadline, when the send's timeout ends the
/// client.
///
/// Even a guest that reads can hold a send while the host does not read: the session's broker
/// passes the guest's credit for the host's records on only behind the guest's own records, and
/// takes no more of the host's records than that credit allows. So the driver sends little before
/// it reads: no program's input, which one thread could not send while it reads the programs'
/// output; the starts of at most [`MAX_EXECUTIONS`] executions, as many as the guest runs at once,
/// whose answers wait for the run, as no program's output flows before the run's credit does;
/// and, in the run, besides a cancellation for each execution, at most a credit for each message
/// that it reads, of output windows of at most [`MAX_OUTPUT_FRAMES`] frames, so that the guest's
/// messages ahead of its own credit stay few.
///
/// The guest ends its session on a second start of the same key, even in a later epoch of the
/// session, so the keys of the executions must differ from those of every execution that the guest
/// heard of before. A failure ends the run, and leaves the client as its own failures do.
#[derive(Debug, Default)]
pub struct Executions {
    /// The host's side of each execution, by ID.
    executions: BTreeMap<u64, HostExecution>,
    /// Whether the driver ran its executions.
    ran: bool,
}

impl Executions {
    /// Returns a driver without executions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the execution of `start` over `client`, whose peer runs with `timers`: sends the
    /// start by `deadline`. The timeout of `timers` runs from then.
    ///
    /// These starts fail before anything is sent, and leave the client usable, as a message that
    /// the framing refuses does: one of an ID that the driver started already, or whose encoding
    /// does not decode, as one with a relative working directory or an environment entry without
    /// a name, on either of which the guest would end its session; and, as [`Executions`]
    /// describes, one that says that the host sends the program's input, one whose output windows
    /// count bytes or more than [`MAX_OUTPUT_FRAMES`] frames, one beyond [`MAX_EXECUTIONS`]
    /// executions, and any once the driver ran.
    pub fn start(
        &mut self,
        client: &mut Client,
        start: Start,
        timers: Timers,
        deadline: Instant,
    ) -> Result<(), ExecutionsError> {
        if self.ran {
            return Err(ExecutionsError::Ran);
        }
        let id = start.key.id();
        if self.executions.contains_key(&id) {
            return Err(ExecutionsError::Duplicate(id));
        }
        if start.input {
            return Err(ExecutionsError::Input);
        }
        let output = start.limits.output();
        if !matches!(output.unit(), Unit::Frames) || output.size() > MAX_OUTPUT_FRAMES {
            return Err(ExecutionsError::Window);
        }
        if self.executions.len() >= MAX_EXECUTIONS {
            return Err(ExecutionsError::Full);
        }
        let body = start.encode();
        Start::decode(&body).map_err(ExecutionsError::Start)?;
        let execution = HostExecution::new(start, timers, Vec::new(), Instant::now());
        client.send(&Message::ExecStart(body), Some(deadline))?;
        self.executions.insert(id, execution);
        Ok(())
    }

    /// Carries the guest's messages to the executions, and theirs to the guest, over `client`,
    /// until every execution finished, and fails with [`ExecutionsError::TimedOut`] once
    /// `deadline` passed first: in a receive, which leaves the client usable, or in a send, which
    /// ends it.
    pub fn run(&mut self, client: &mut Client, deadline: Instant) -> Result<(), ExecutionsError> {
        self.ran = true;
        loop {
            let now = Instant::now();
            for execution in self.executions.values_mut() {
                for message in execution.serve(now) {
                    client.send(&message, Some(deadline)).map_err(failed)?;
                }
            }
            if self.is_finished() {
                return Ok(());
            }
            if now >= deadline {
                return Err(ExecutionsError::TimedOut);
            }
            let wake = self
                .executions
                .values()
                .filter_map(HostExecution::deadline)
                .fold(deadline, Instant::min);
            match client.receive(Some(wake)) {
                Ok(message) => self.dispatch(&message, Instant::now())?,
                Err(ClientError::Session(HostConnectionError::TimedOut)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Returns the host's side of the execution `id`, if the driver started it.
    pub fn get(&self, id: u64) -> Option<&HostExecution> {
        self.executions.get(&id)
    }

    /// Returns the host's side of the execution `id`, if the driver started it, with which the
    /// caller may end it.
    pub fn get_mut(&mut self, id: u64) -> Option<&mut HostExecution> {
        self.executions.get_mut(&id)
    }

    /// Returns the host's side of each execution, in the order of their IDs.
    pub fn iter(&self) -> impl Iterator<Item = &HostExecution> {
        self.executions.values()
    }

    /// Returns whether every execution finished.
    pub fn is_finished(&self) -> bool {
        self.executions
            .values()
            .all(|execution| execution.outcome().is_some())
    }

    /// Returns whether `message` is one of the guest's messages about an execution that the driver
    /// started, of its key, whose body decodes: one that the guest may still send once the driver
    /// finished the execution without it, as when it gave up waiting for the execution's outcome,
    /// or once its run failed.
    pub fn is_about(&self, message: &Message) -> bool {
        let Some(Ok(decoded)) = GuestMessage::decode(message) else {
            return false;
        };
        let key = decoded.key();
        self.executions
            .get(&key.id())
            .is_some_and(|execution| execution.start().key == key)
    }

    /// Hands the guest's `message` to the host's side of its execution at `now`.
    fn dispatch(&mut self, message: &Message, now: Instant) -> Result<(), ExecutionsError> {
        let decoded = GuestMessage::decode(message)
            .ok_or(ExecutionsError::Unexpected(message.kind()))?
            .map_err(ExecutionsError::Body)?;
        let key = decoded.key();
        let execution = self
            .executions
            .get_mut(&key.id())
            .ok_or(ExecutionsError::Unknown(key))?;
        execution
            .deliver(&decoded, now)
            .map_err(|violation| ExecutionsError::Violation(key, violation))?;
        Ok(())
    }
}

/// Returns the failure of a run whose send failed with `error`: [`ExecutionsError::TimedOut`]
/// once the send's deadline, the run's, passed.
fn failed(error: ClientError) -> ExecutionsError {
    match error {
        ClientError::Session(HostConnectionError::TimedOut) => ExecutionsError::TimedOut,
        error => ExecutionsError::Client(error),
    }
}
