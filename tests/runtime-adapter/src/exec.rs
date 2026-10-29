//! The adapter's messages about executions: programs that the guest runs for the host, whose
//! standard streams and outcome the execution engine of `nvx::exec` carries. The host's side of an
//! execution is the engine's [`Peer`], and the guest's side its [`Runner`](nvx::exec::Runner),
//! which the guest agent's worker binds to the program through `nvx::guest::exec::binding`. A
//! [`HostExecution`] holds the peer with a sink of the program's output and a source of its input,
//! as the host program's driver, `host::Executions`, and the in-process tests of the worker run it.
//!
//! Each message of the engine travels in a frame of its own kind. Its body starts with the
//! execution's [`Key`]: the execution's ID, then its generation, each a `u64`:
//!
//! | Kind | From | Message | Body after the key |
//! | --- | --- | --- | --- |
//! | 12 | host | `ExecStart`: starts the execution | the rest of its [`Start`] |
//! | 13 | host | `ExecInput`: bytes of the program's standard input | the bytes |
//! | 14 | host | `ExecInputEnd`: the end of the program's standard input | none |
//! | 15 | host | `ExecCancel`: ends the execution for a cause | the cause |
//! | 16 | both | `ExecCredit`: credit of an output pool, from the host, or of the input, from the guest | the pool, then the units, a `u32` |
//! | 17 | guest | `ExecStarted`: the program runs | none |
//! | 18 | guest | `ExecOutput`: bytes of the program's standard output or error | the stream, then the bytes |
//! | 19 | guest | `ExecEnd`: the end of a stream, after its last byte | the stream |
//! | 20 | guest | `ExecInputClosed`: the program's input closed before the host ended it | none |
//! | 21 | guest | `ExecTerminal`: the execution's outcome, the guest's last message about it | the [`Outcome`] |
//!
//! Numbers are little-endian. A pool is 0 for the input, 1 for standard output, 2 for standard
//! error, and 3 for the output pool that both streams share; a stream is 1 for standard output and
//! 2 for standard error; a cause is 1 for a cancellation, 2 for a drop, 3 for a timeout, 4 for a
//! reset, and 5 for a disconnection.
//!
//! A start, after the key, holds the [`Limits`]: the chunk, a `u32` of at most [`MAX_CHUNK`]
//! bytes, the input's window, then the output's, each a unit, 0 for bytes or 1 for frames, then
//! its size, a `u32`, and whether the streams share one output pool, 0 or 1. It then holds the
//! runner's [`Timers`], in milliseconds, each a `u64`: the grace, the settlement time, and
//! whether a timeout follows, 0 or 1, then the timeout, 0 if none. Then come whether the host sends
//! the program's standard input, 0 or 1, and the program's arguments: their count, a `u16`, then
//! each argument's length, a `u32`, and its bytes. The first argument is the program's path, and
//! no argument holds a NUL byte.
//!
//! The start ends with how the program enters the guest's root:
//!
//! - whether it names the program's [`Identity`], 0 or 1, and if it does, the user ID and the
//!   group ID, each a `u32`, the [`AccountMode`], 0 for an existing account or 1 for numeric IDs,
//!   the capabilities, a `u64` whose bit N stands for the capability numbered N, then the
//!   supplementary groups: their count, a `u16`, then each group, a `u32`;
//! - whether it names the program's working directory, 0 or 1, and if it does, the directory's
//!   length, a `u32`, and its bytes: an absolute path without a NUL byte. Otherwise the program
//!   starts in the root's `/`;
//! - the program's whole environment: the count of its entries, a `u16`, then each entry's
//!   length, a `u32`, and its bytes, `NAME=VALUE`, with a name that is not empty, and without a
//!   NUL byte.
//!
//! An outcome, after the key, holds the program's status: 0 for none, 1 for an exit, or 2 for a
//! signal, then the exit's code or the signal's number, an `i32`, or 0; the cause, or 0 for none;
//! whether the launch failed, 0 or 1, then its stage, a `u32`, and its `errno`, an `i32`, or 0 for
//! each; and flags: 1 if standard output lost bytes, 2 if standard error did, and 4 if the
//! execution is reusable.
//!
//! [`HostMessage`] and [`GuestMessage`] encode and decode these messages, and turn the engine's
//! messages into them. A body that breaks these rules decodes as a [`BodyError`].

use std::time::{Duration, Instant};

use nvx::exec::{
    Cause, Key, LaunchFailure, Limits, Outcome, Peer, Pool, Receipt, Status, Stream, Timers, Unit,
    Violation, Window, peer, runner,
};

use crate::frame::{BodyError, MAX_BODY, Message};

/// Bytes of an execution's key, at the start of each body.
pub const KEY_LEN: usize = 16;

/// The most bytes of a chunk, the data of one `ExecInput` or `ExecOutput`: what the framing's
/// longest body holds after the key and the stream.
pub const MAX_CHUNK: u32 = (MAX_BODY - KEY_LEN - 1) as u32;

/// The most executions that the guest's worker runs at once, beyond which the host's driver
/// starts none while those that it started have not finished.
pub const MAX_EXECUTIONS: usize = 8;

/// The most bytes of each output stream of an execution that a [`HostExecution`] keeps: it
/// cancels an execution whose stream would exceed them, so that its host holds no more.
pub const MAX_CAPTURE: usize = 16 * 1024 * 1024;

/// The code of a cause that the framing does not know, which no side decodes.
const UNKNOWN_CAUSE: u8 = 0xff;

/// The flag of an outcome whose standard output lost bytes.
const STDOUT_TRUNCATED: u8 = 1;
/// The flag of an outcome whose standard error lost bytes.
const STDERR_TRUNCATED: u8 = 2;
/// The flag of a reusable outcome.
const REUSABLE: u8 = 4;

/// The start of an execution, which the host sends in an `ExecStart`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Start {
    /// The execution's key.
    pub key: Key,
    /// The limits on which both sides agree, with a chunk of at most [`MAX_CHUNK`] bytes.
    pub limits: Limits,
    /// The timers of the guest's side, in whole milliseconds.
    pub timers: Timers,
    /// Whether the host sends the program's standard input. Otherwise the program reads the null
    /// device, and its input is closed from the start.
    pub input: bool,
    /// The program's path, which is also its first argument, then its other arguments.
    pub arguments: Vec<Vec<u8>>,
    /// The identity as which the program runs, if the start names one. Exactly one source names
    /// it: the start, or the workload identity of the guest's whole sandbox.
    pub identity: Option<Identity>,
    /// The program's working directory, an absolute path in the guest's root, if the program
    /// does not start in the root's `/`.
    pub working_directory: Option<Vec<u8>>,
    /// The program's whole environment, as `NAME=VALUE` entries.
    pub environment: Vec<Vec<u8>>,
}

impl Start {
    /// Returns the start of the execution `key`, with `limits` and the guest's `timers`, of the
    /// program that `arguments` names, without standard input, without an identity, in the
    /// root's `/`, and with an empty environment.
    pub fn new<A: Into<Vec<u8>>>(
        key: Key,
        limits: Limits,
        timers: Timers,
        arguments: impl IntoIterator<Item = A>,
    ) -> Self {
        Self {
            key,
            limits,
            timers,
            input: false,
            arguments: arguments.into_iter().map(Into::into).collect(),
            identity: None,
            working_directory: None,
            environment: Vec::new(),
        }
    }

    /// Makes the host send the program's standard input.
    pub fn with_input(mut self) -> Self {
        self.input = true;
        self
    }

    /// Names the identity as which the program runs.
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Names the program's working directory.
    pub fn with_working_directory(mut self, directory: impl Into<Vec<u8>>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }

    /// Gives the program the environment of the `NAME=VALUE` entries `environment`.
    pub fn with_environment<E: Into<Vec<u8>>>(
        mut self,
        environment: impl IntoIterator<Item = E>,
    ) -> Self {
        self.environment = environment.into_iter().map(Into::into).collect();
        self
    }

    /// Returns the start's encoding, the body of an `ExecStart`.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = key_body(self.key);
        let limits = self.limits;
        // A chunk that exceeds `u32` exceeds `MAX_CHUNK` too, which the guest refuses.
        put_u32(&mut body, u32::try_from(limits.chunk()).unwrap_or(u32::MAX));
        for window in [limits.input(), limits.output()] {
            body.push(match window.unit() {
                Unit::Bytes => 0,
                Unit::Frames => 1,
            });
            put_u32(&mut body, window.size());
        }
        body.push(u8::from(limits.is_shared()));
        put_u64(&mut body, milliseconds(self.timers.grace()));
        put_u64(&mut body, milliseconds(self.timers.settlement()));
        let timeout = self.timers.timeout_duration();
        body.push(u8::from(timeout.is_some()));
        put_u64(&mut body, timeout.map_or(0, milliseconds));
        body.push(u8::from(self.input));
        // A start with more arguments than `u16` counts exceeds the framing's limit too.
        put_count(&mut body, self.arguments.len());
        for argument in &self.arguments {
            put_bytes(&mut body, argument);
        }
        match &self.identity {
            None => body.push(0),
            Some(identity) => {
                body.push(1);
                put_u32(&mut body, identity.uid);
                put_u32(&mut body, identity.gid);
                body.push(identity.account.code());
                put_u64(&mut body, identity.capabilities);
                put_count(&mut body, identity.groups.len());
                for group in &identity.groups {
                    put_u32(&mut body, *group);
                }
            }
        }
        match &self.working_directory {
            None => body.push(0),
            Some(directory) => {
                body.push(1);
                put_bytes(&mut body, directory);
            }
        }
        put_count(&mut body, self.environment.len());
        for entry in &self.environment {
            put_bytes(&mut body, entry);
        }
        body
    }

    /// Decodes the body of an `ExecStart`, and checks it: its limits, of which none is empty and
    /// whose chunk is at most [`MAX_CHUNK`] bytes, its flags, its arguments, of which there is
    /// at least one, the program's path, which is not empty, and none of which holds a NUL byte,
    /// its identity's account mode, its working directory, which is absolute and holds no NUL
    /// byte, and its environment, whose entries are `NAME=VALUE`, with a name that is not empty,
    /// and hold no NUL byte.
    ///
    /// Fails with a [`BodyError`] that names the first problem.
    pub fn decode(body: &[u8]) -> Result<Self, BodyError> {
        let refused = |problem: String| BodyError::new(format!("an execution's start {problem}"));
        let mut reader = Reader::new(body, "an execution's start");
        let key = reader.key()?;
        let chunk = reader.u32()?;
        let input = window(reader.u8()?, reader.u32()?).map_err(refused)?;
        let output = window(reader.u8()?, reader.u32()?).map_err(refused)?;
        let shared = flag(reader.u8()?).map_err(refused)?;
        let grace = Duration::from_millis(reader.u64()?);
        let settlement = Duration::from_millis(reader.u64()?);
        let timed = flag(reader.u8()?).map_err(refused)?;
        let timeout = reader.u64()?;
        let with_input = flag(reader.u8()?).map_err(refused)?;
        let count = reader.u16()?;
        let mut arguments = Vec::with_capacity(usize::from(count).min(body.len()));
        for _ in 0..count {
            let argument = reader.counted()?;
            if argument.contains(&0) {
                return Err(refused("has an argument with a NUL byte".to_owned()));
            }
            arguments.push(argument.to_vec());
        }
        let identity = if flag(reader.u8()?).map_err(refused)? {
            let uid = reader.u32()?;
            let gid = reader.u32()?;
            let account = AccountMode::from_code(reader.u8()?).map_err(refused)?;
            let capabilities = reader.u64()?;
            let count = reader.u16()?;
            let mut groups = Vec::with_capacity(usize::from(count).min(body.len()));
            for _ in 0..count {
                groups.push(reader.u32()?);
            }
            Some(Identity {
                uid,
                gid,
                groups,
                capabilities,
                account,
            })
        } else {
            None
        };
        let working_directory = if flag(reader.u8()?).map_err(refused)? {
            let directory = reader.counted()?;
            if directory.first() != Some(&b'/') {
                return Err(refused(
                    "names a working directory that is not absolute".to_owned(),
                ));
            }
            if directory.contains(&0) {
                return Err(refused(
                    "has a working directory with a NUL byte".to_owned(),
                ));
            }
            Some(directory.to_vec())
        } else {
            None
        };
        let count = reader.u16()?;
        let mut environment = Vec::with_capacity(usize::from(count).min(body.len()));
        for _ in 0..count {
            let entry = reader.counted()?;
            if entry.contains(&0) {
                return Err(refused(
                    "has an environment entry with a NUL byte".to_owned(),
                ));
            }
            if entry.iter().position(|&byte| byte == b'=').unwrap_or(0) == 0 {
                return Err(refused(
                    "has an environment entry that is not NAME=VALUE".to_owned(),
                ));
            }
            environment.push(entry.to_vec());
        }
        reader.finish()?;
        if chunk > MAX_CHUNK {
            return Err(refused(format!(
                "has a chunk of {chunk} bytes, more than {MAX_CHUNK}"
            )));
        }
        let mut limits = Limits::new(chunk, input, output)
            .ok_or_else(|| refused("has an empty chunk or window".to_owned()))?;
        if shared {
            limits = limits.shared_output();
        }
        let mut timers = Timers::new(grace, settlement);
        if timed {
            timers = timers.timeout(Duration::from_millis(timeout));
        } else if timeout != 0 {
            return Err(refused("has a timeout without its flag".to_owned()));
        }
        match arguments.first() {
            None => return Err(refused("names no program".to_owned())),
            Some(program) if program.is_empty() => {
                return Err(refused("names an empty program".to_owned()));
            }
            Some(_) => {}
        }
        Ok(Self {
            key,
            limits,
            timers,
            input: with_input,
            arguments,
            identity,
            working_directory,
            environment,
        })
    }
}

/// The identity as which an execution's program runs, which a start names unless the guest's
/// sandbox has a workload identity: its IDs, its capabilities, and how the guest checks its
/// account, as the guest's entry into its root takes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The user ID.
    pub uid: u32,
    /// The group ID.
    pub gid: u32,
    /// The supplementary groups.
    pub groups: Vec<u32>,
    /// The capabilities, which only user ID 0 may hold: bit N for the capability numbered N.
    pub capabilities: u64,
    /// How the guest checks the identity's account.
    pub account: AccountMode,
}

impl Identity {
    /// Returns the identity of user ID `uid` and group ID `gid`, without supplementary groups or
    /// capabilities, whose account the guest checks in [existing](AccountMode::Existing) mode.
    pub fn new(uid: u32, gid: u32) -> Self {
        Self {
            uid,
            gid,
            groups: Vec::new(),
            capabilities: 0,
            account: AccountMode::Existing,
        }
    }
}

/// How the guest checks the account of an execution's [`Identity`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccountMode {
    /// The root's `/etc/passwd` holds exactly one entry for the user ID, whose primary group is
    /// the group ID, which the root's `/etc/group` holds.
    #[default]
    Existing,
    /// The IDs are used as given, without an account.
    Numeric,
}

impl AccountMode {
    /// Returns the mode's code in a start: 0 for an existing account, 1 for numeric IDs.
    fn code(self) -> u8 {
        match self {
            Self::Existing => 0,
            Self::Numeric => 1,
        }
    }

    /// Returns the mode that `code` encodes, or the problem.
    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            0 => Ok(Self::Existing),
            1 => Ok(Self::Numeric),
            _ => Err(format!("names the unknown account mode {code}")),
        }
    }
}

/// A message of the host's side of an execution, its [`Peer`], to the guest's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostMessage<'a> {
    /// Starts the execution.
    Start(Start),
    /// Bytes of the program's standard input, at most a chunk.
    Input(Key, &'a [u8]),
    /// The end of the program's standard input, after its last byte.
    InputEnd(Key),
    /// Ends the execution for the cause.
    Cancel(Key, Cause),
    /// Credit of an output pool: [`Pool::Output`] if the streams share one, otherwise
    /// [`Pool::Stdout`] or [`Pool::Stderr`].
    Credit(Key, Pool, u32),
}

impl<'a> HostMessage<'a> {
    /// Returns the peer's `message` about the execution `key`, whose input, if any, is at the
    /// start of `buffer`, as [`Peer::next_message`] returns it, or `None` for a message that the
    /// framing does not know.
    pub fn from_peer(key: Key, message: peer::Message, buffer: &'a [u8]) -> Option<Self> {
        Some(match message {
            peer::Message::Cancel(cause) => Self::Cancel(key, cause),
            peer::Message::Credit { pool, units } => Self::Credit(key, pool, units),
            peer::Message::Input { length } => Self::Input(key, &buffer[..length]),
            peer::Message::InputEnd => Self::InputEnd(key),
            _ => return None,
        })
    }

    /// Returns the execution's key.
    pub fn key(&self) -> Key {
        match self {
            Self::Start(start) => start.key,
            Self::Input(key, _) | Self::InputEnd(key) | Self::Cancel(key, _) => *key,
            Self::Credit(key, ..) => *key,
        }
    }

    /// Returns the frame that carries the message.
    pub fn encode(&self) -> Message {
        match self {
            Self::Start(start) => Message::ExecStart(start.encode()),
            Self::Input(key, bytes) => {
                let mut body = key_body(*key);
                body.extend_from_slice(bytes);
                Message::ExecInput(body)
            }
            Self::InputEnd(key) => Message::ExecInputEnd(key_body(*key)),
            Self::Cancel(key, cause) => {
                let mut body = key_body(*key);
                body.push(cause_code(*cause));
                Message::ExecCancel(body)
            }
            Self::Credit(key, pool, units) => Message::ExecCredit(credit_body(*key, *pool, *units)),
        }
    }

    /// Decodes `message` if it is one of the host's messages about an execution: `None` for
    /// another message, and a [`BodyError`] for a body that breaks the rules, such as a credit of
    /// the input, which only the guest grants, or a cause other than a cancellation, a drop, or a
    /// timeout.
    pub fn decode(message: &'a Message) -> Option<Result<Self, BodyError>> {
        Some(match message {
            Message::ExecStart(body) => Start::decode(body).map(Self::Start),
            Message::ExecInput(body) => decode_input(body),
            Message::ExecInputEnd(body) => Reader::new(body, "an end of input")
                .key_only()
                .map(Self::InputEnd),
            Message::ExecCancel(body) => decode_cancel(body),
            Message::ExecCredit(body) => match decode_credit(body) {
                Ok((key, Pool::Input, _)) => Err(BodyError::new(format!(
                    "the host credits the input of execution {}",
                    name(key)
                ))),
                Ok((key, pool, units)) => Ok(Self::Credit(key, pool, units)),
                Err(error) => Err(error),
            },
            _ => return None,
        })
    }
}

/// A message of the guest's side of an execution, its [`Runner`](nvx::exec::Runner), to the host's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestMessage<'a> {
    /// Credit of the program's standard input.
    Credit(Key, u32),
    /// The program runs.
    Started(Key),
    /// Bytes of the stream, at most a chunk.
    Output(Key, Stream, &'a [u8]),
    /// The end of the stream, after its last byte.
    End(Key, Stream),
    /// The program's standard input closed before the host ended it.
    InputClosed(Key),
    /// The execution's outcome: the guest's last message about it.
    Terminal(Key, Outcome),
}

impl<'a> GuestMessage<'a> {
    /// Returns the runner's `message` about the execution `key`, whose output, if any, is at the
    /// start of `buffer`, as [`Runner::next_message`](nvx::exec::Runner::next_message) returns
    /// it, or `None` for a message that the framing does not know.
    pub fn from_runner(key: Key, message: runner::Message, buffer: &'a [u8]) -> Option<Self> {
        Some(match message {
            runner::Message::Credit { units } => Self::Credit(key, units),
            runner::Message::Started => Self::Started(key),
            runner::Message::Output { stream, length } => {
                Self::Output(key, stream, &buffer[..length])
            }
            runner::Message::End(stream) => Self::End(key, stream),
            runner::Message::InputClosed => Self::InputClosed(key),
            runner::Message::Terminal(outcome) => Self::Terminal(key, outcome),
            _ => return None,
        })
    }

    /// Returns the execution's key.
    pub fn key(&self) -> Key {
        match self {
            Self::Credit(key, _) | Self::Started(key) | Self::InputClosed(key) => *key,
            Self::Output(key, ..) | Self::End(key, _) | Self::Terminal(key, _) => *key,
        }
    }

    /// Returns the frame that carries the message.
    pub fn encode(&self) -> Message {
        match self {
            Self::Credit(key, units) => Message::ExecCredit(credit_body(*key, Pool::Input, *units)),
            Self::Started(key) => Message::ExecStarted(key_body(*key)),
            Self::Output(key, stream, bytes) => {
                let mut body = key_body(*key);
                body.push(stream_code(*stream));
                body.extend_from_slice(bytes);
                Message::ExecOutput(body)
            }
            Self::End(key, stream) => {
                let mut body = key_body(*key);
                body.push(stream_code(*stream));
                Message::ExecEnd(body)
            }
            Self::InputClosed(key) => Message::ExecInputClosed(key_body(*key)),
            Self::Terminal(key, outcome) => Message::ExecTerminal(terminal_body(*key, outcome)),
        }
    }

    /// Decodes `message` if it is one of the guest's messages about an execution: `None` for
    /// another message, and a [`BodyError`] for a body that breaks the rules, such as a credit of
    /// an output pool, which only the host grants.
    pub fn decode(message: &'a Message) -> Option<Result<Self, BodyError>> {
        Some(match message {
            Message::ExecCredit(body) => match decode_credit(body) {
                Ok((key, Pool::Input, units)) => Ok(Self::Credit(key, units)),
                Ok((key, pool, _)) => Err(BodyError::new(format!(
                    "the guest credits the {} pool of execution {}",
                    pool_name(pool),
                    name(key)
                ))),
                Err(error) => Err(error),
            },
            Message::ExecStarted(body) => Reader::new(body, "a start's confirmation")
                .key_only()
                .map(Self::Started),
            Message::ExecOutput(body) => decode_output(body),
            Message::ExecEnd(body) => decode_end(body),
            Message::ExecInputClosed(body) => Reader::new(body, "an input's close")
                .key_only()
                .map(Self::InputClosed),
            Message::ExecTerminal(body) => decode_terminal(body),
            _ => return None,
        })
    }

    /// Hands the message to `peer`, the host's side of its execution, at `now`, and returns what
    /// the peer returns.
    pub fn deliver(&self, peer: &mut Peer, now: Instant) -> Result<Receipt, Violation> {
        match *self {
            Self::Credit(key, units) => peer.credit(key, units),
            Self::Started(key) => peer.started(key),
            Self::Output(key, stream, bytes) => peer.output(key, stream, bytes),
            Self::End(key, stream) => peer.end_output(key, stream),
            Self::InputClosed(key) => peer.input_closed(key),
            Self::Terminal(key, outcome) => peer.terminal(now, key, outcome),
        }
    }
}

/// The host's side of an execution: its [`Peer`], with a sink that takes the program's output as
/// it arrives, unless its caller stalls it, and a source that hands over the program's input, if
/// the start says that the host sends it, then ends it.
///
/// The sink keeps at most [`MAX_CAPTURE`] bytes of each stream. Once more of a stream arrives, the
/// host's side cancels the execution, and its peer discards the rest once the grace ends, so that
/// the outcome marks the stream truncated, with the cause [`Cause::Cancelled`].
///
/// Its caller sends the execution's [`Start`], hands it each of the guest's messages about the
/// execution with [`deliver`](Self::deliver), and sends the frames that [`serve`](Self::serve)
/// returns, until the peer [finished](Self::outcome). The host program's driver,
/// `host::Executions`, runs several at once over the session.
#[derive(Debug)]
pub struct HostExecution {
    /// The execution's start.
    start: Start,
    /// The host's side of the execution.
    peer: Peer,
    /// Whether the sink takes nothing.
    stalled: bool,
    /// The bytes of each stream that the sink took.
    output: [Vec<u8>; 2],
    /// The input that the source hands over, if the start says that the host sends it.
    input: Vec<u8>,
    /// How many bytes of the input the source handed over.
    sent: usize,
    /// Whether the source ended the input.
    ended: bool,
    /// The peer's events, in order.
    events: Vec<peer::Event>,
    /// Whether a stream outgrew [`MAX_CAPTURE`], so that the host's side cancelled the execution.
    overflowed: bool,
}

impl HostExecution {
    /// Returns the host's side of the execution of `start`, created at `now`, from which the
    /// timeout of its peer's `timers` runs, with a source of `input`.
    pub fn new(start: Start, timers: Timers, input: Vec<u8>, now: Instant) -> Self {
        let peer = Peer::new(start.key, start.limits, timers, now);
        Self {
            start,
            peer,
            stalled: false,
            output: [Vec::new(), Vec::new()],
            input,
            sent: 0,
            ended: false,
            events: Vec::new(),
            overflowed: false,
        }
    }

    /// Returns the execution's start.
    pub fn start(&self) -> &Start {
        &self.start
    }

    /// Returns the host's side of the execution.
    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    /// Returns the host's side of the execution, with which the caller ends it, or tells it that
    /// the session was reset or lost.
    pub fn peer_mut(&mut self) -> &mut Peer {
        &mut self.peer
    }

    /// Makes the sink take nothing while `stalled` holds, so that the program's output waits in
    /// the peer, which grants the guest no credit for it.
    pub fn stall(&mut self, stalled: bool) {
        self.stalled = stalled;
    }

    /// Returns the bytes of `stream` that the sink took, at most [`MAX_CAPTURE`].
    pub fn output(&self, stream: Stream) -> &[u8] {
        &self.output[position(stream)]
    }

    /// Returns the peer's events, in order.
    pub fn events(&self) -> &[peer::Event] {
        &self.events
    }

    /// Returns the execution's outcome, once the peer finished.
    pub fn outcome(&self) -> Option<Outcome> {
        self.events.iter().find_map(|event| match event {
            peer::Event::Finished(outcome) => Some(*outcome),
            _ => None,
        })
    }

    /// Returns when the peer's next timer fires, for the caller to [serve](Self::serve) the
    /// execution then, or `None` while no timer runs.
    pub fn deadline(&self) -> Option<Instant> {
        self.peer.deadline()
    }

    /// Hands the guest's `message` about the execution to the peer at `now`, and returns what the
    /// peer returns.
    pub fn deliver(
        &mut self,
        message: &GuestMessage<'_>,
        now: Instant,
    ) -> Result<Receipt, Violation> {
        message.deliver(&mut self.peer, now)
    }

    /// Takes every step that the source, the sink, and the peer can take at `now`, and returns
    /// the peer's messages for the guest, as frames, in order, the cancellation of an execution
    /// that the peer gave up on in this step included.
    pub fn serve(&mut self, now: Instant) -> Vec<Message> {
        if self.start.input && !self.ended {
            self.sent += self.peer.input(&self.input[self.sent..]);
            if self.sent == self.input.len() {
                self.peer.end_input();
                self.ended = true;
            }
        }
        if !self.stalled {
            for stream in Stream::ALL {
                let output = &mut self.output[position(stream)];
                loop {
                    let pending = self.peer.pending_output(stream);
                    if pending.is_empty() {
                        break;
                    }
                    let room = MAX_CAPTURE - output.len();
                    if room == 0 {
                        if !self.overflowed {
                            self.overflowed = true;
                            self.peer.cancel(now);
                        }
                        break;
                    }
                    let length = pending.len().min(room);
                    output.extend_from_slice(&pending[..length]);
                    self.peer.consumed(stream, length);
                }
            }
        }
        self.peer.tick(now);
        // The messages go first: a peer without grace or settlement time gives up on an
        // execution in the step that ends it, and its last event, once taken, discards the
        // cancellation that waits to be sent.
        let mut buffer = vec![0; self.start.limits.chunk()];
        let mut messages = Vec::new();
        while let Some(message) = self.peer.next_message(&mut buffer) {
            if let Some(message) = HostMessage::from_peer(self.start.key, message, &buffer) {
                messages.push(message.encode());
            }
        }
        while let Some(event) = self.peer.next_event() {
            self.events.push(event);
        }
        messages
    }
}

/// Returns the position of `stream` among [`Stream::ALL`].
fn position(stream: Stream) -> usize {
    match stream {
        Stream::Stdout => 0,
        Stream::Stderr => 1,
    }
}

/// Returns `key` as a name for problems: its ID, then its generation.
pub fn name(key: Key) -> String {
    format!("{}.{}", key.id(), key.generation())
}

/// Reads the numbers of a body in order, and fails with a [`BodyError`] that names its message
/// if the body ends early or has bytes left.
struct Reader<'a> {
    /// The rest of the body.
    rest: &'a [u8],
    /// The message, for problems.
    what: &'static str,
}

impl<'a> Reader<'a> {
    /// Returns a reader of `body`, the body of `what`.
    fn new(body: &'a [u8], what: &'static str) -> Self {
        Self { rest: body, what }
    }

    /// Takes the next `length` bytes.
    fn bytes(&mut self, length: usize) -> Result<&'a [u8], BodyError> {
        if self.rest.len() < length {
            return Err(BodyError::new(format!("{} ends early", self.what)));
        }
        let (taken, rest) = self.rest.split_at(length);
        self.rest = rest;
        Ok(taken)
    }

    /// Takes the next bytes, after their length, a `u32`.
    fn counted(&mut self) -> Result<&'a [u8], BodyError> {
        let length = self.u32()?;
        self.bytes(usize::try_from(length).unwrap_or(usize::MAX))
    }

    /// Takes the next `N` bytes.
    fn array<const N: usize>(&mut self) -> Result<[u8; N], BodyError> {
        Ok(self.bytes(N)?.try_into().expect("N bytes"))
    }

    /// Takes a byte.
    fn u8(&mut self) -> Result<u8, BodyError> {
        Ok(self.array::<1>()?[0])
    }

    /// Takes a `u16`.
    fn u16(&mut self) -> Result<u16, BodyError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    /// Takes a `u32`.
    fn u32(&mut self) -> Result<u32, BodyError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// Takes an `i32`.
    fn i32(&mut self) -> Result<i32, BodyError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    /// Takes a `u64`.
    fn u64(&mut self) -> Result<u64, BodyError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// Takes a key.
    fn key(&mut self) -> Result<Key, BodyError> {
        let id = self.u64()?;
        Ok(Key::new(id, self.u64()?))
    }

    /// Takes the rest of the body.
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.rest)
    }

    /// Fails if bytes are left.
    fn finish(&self) -> Result<(), BodyError> {
        match self.rest.len() {
            0 => Ok(()),
            1 => Err(BodyError::new(format!("{} has 1 byte too many", self.what))),
            left => Err(BodyError::new(format!(
                "{} has {left} bytes too many",
                self.what
            ))),
        }
    }

    /// Takes a key, which is the whole body.
    fn key_only(mut self) -> Result<Key, BodyError> {
        let key = self.key()?;
        self.finish()?;
        Ok(key)
    }
}

/// Decodes the body of an `ExecInput`, whose bytes are at least one.
fn decode_input(body: &[u8]) -> Result<HostMessage<'_>, BodyError> {
    let mut reader = Reader::new(body, "an execution's input");
    let key = reader.key()?;
    let bytes = reader.rest();
    if bytes.is_empty() {
        return Err(BodyError::new(format!(
            "the input of execution {} has no byte",
            name(key)
        )));
    }
    Ok(HostMessage::Input(key, bytes))
}

/// Decodes the body of an `ExecCancel`, whose cause is a cancellation, a drop, or a timeout.
fn decode_cancel(body: &[u8]) -> Result<HostMessage<'static>, BodyError> {
    let mut reader = Reader::new(body, "an execution's cancellation");
    let key = reader.key()?;
    let code = reader.u8()?;
    reader.finish()?;
    match cause(code) {
        Some(cause @ (Cause::Cancelled | Cause::Dropped | Cause::TimedOut)) => {
            Ok(HostMessage::Cancel(key, cause))
        }
        _ => Err(BodyError::new(format!(
            "the cancellation of execution {} names the cause {code}",
            name(key)
        ))),
    }
}

/// Decodes the body of an `ExecCredit`: its key, pool, and units, which are at least one.
fn decode_credit(body: &[u8]) -> Result<(Key, Pool, u32), BodyError> {
    let mut reader = Reader::new(body, "an execution's credit");
    let key = reader.key()?;
    let code = reader.u8()?;
    let units = reader.u32()?;
    reader.finish()?;
    let pool = pool(code).ok_or_else(|| {
        BodyError::new(format!(
            "the credit of execution {} names the unknown pool {code}",
            name(key)
        ))
    })?;
    if units == 0 {
        return Err(BodyError::new(format!(
            "the credit of execution {} has no unit",
            name(key)
        )));
    }
    Ok((key, pool, units))
}

/// Decodes the body of an `ExecOutput`, whose bytes are at least one.
fn decode_output(body: &[u8]) -> Result<GuestMessage<'_>, BodyError> {
    let mut reader = Reader::new(body, "an execution's output");
    let key = reader.key()?;
    let stream = decode_stream(key, reader.u8()?)?;
    let bytes = reader.rest();
    if bytes.is_empty() {
        return Err(BodyError::new(format!(
            "the output of execution {} has no byte",
            name(key)
        )));
    }
    Ok(GuestMessage::Output(key, stream, bytes))
}

/// Decodes the body of an `ExecEnd`.
fn decode_end(body: &[u8]) -> Result<GuestMessage<'static>, BodyError> {
    let mut reader = Reader::new(body, "a stream's end");
    let key = reader.key()?;
    let stream = decode_stream(key, reader.u8()?)?;
    reader.finish()?;
    Ok(GuestMessage::End(key, stream))
}

/// Decodes the body of an `ExecTerminal`.
fn decode_terminal(body: &[u8]) -> Result<GuestMessage<'static>, BodyError> {
    let mut reader = Reader::new(body, "an execution's outcome");
    let key = reader.key()?;
    let refused = |problem: String| {
        BodyError::new(format!("the outcome of execution {} {problem}", name(key)))
    };
    let status = match (reader.u8()?, reader.i32()?) {
        (0, 0) => None,
        (1, code) => Some(Status::Exited(code)),
        (2, signal) => Some(Status::Signaled(signal)),
        (kind, value) => return Err(refused(format!("has the status {kind} {value}"))),
    };
    let mut outcome = Outcome::new(status);
    match reader.u8()? {
        0 => {}
        code => match cause(code) {
            Some(cause) => outcome = outcome.with_cause(cause),
            None => return Err(refused(format!("names the unknown cause {code}"))),
        },
    }
    match (reader.u8()?, reader.u32()?, reader.i32()?) {
        (0, 0, 0) => {}
        (1, stage, error) => {
            outcome = outcome.with_launch_failure(LaunchFailure::new(stage, error))
        }
        (failed, stage, error) => {
            return Err(refused(format!(
                "has the launch failure {failed} {stage} {error}"
            )));
        }
    }
    let flags = reader.u8()?;
    reader.finish()?;
    if flags & !(STDOUT_TRUNCATED | STDERR_TRUNCATED | REUSABLE) != 0 {
        return Err(refused(format!("has the flags {flags:#x}")));
    }
    if flags & STDOUT_TRUNCATED != 0 {
        outcome = outcome.with_truncated(Stream::Stdout);
    }
    if flags & STDERR_TRUNCATED != 0 {
        outcome = outcome.with_truncated(Stream::Stderr);
    }
    if flags & REUSABLE != 0 {
        outcome = outcome.with_reusable();
    }
    Ok(GuestMessage::Terminal(key, outcome))
}

/// Decodes the stream of the execution `key` that `code` names.
fn decode_stream(key: Key, code: u8) -> Result<Stream, BodyError> {
    match code {
        1 => Ok(Stream::Stdout),
        2 => Ok(Stream::Stderr),
        _ => Err(BodyError::new(format!(
            "a message of execution {} names the unknown stream {code}",
            name(key)
        ))),
    }
}

/// Returns the body that holds `key` alone.
fn key_body(key: Key) -> Vec<u8> {
    let mut body = Vec::with_capacity(KEY_LEN);
    put_u64(&mut body, key.id());
    put_u64(&mut body, key.generation());
    body
}

/// Returns the body of a credit of `units` for the pool `pool` of the execution `key`.
fn credit_body(key: Key, pool: Pool, units: u32) -> Vec<u8> {
    let mut body = key_body(key);
    body.push(match pool {
        Pool::Input => 0,
        Pool::Stdout => 1,
        Pool::Stderr => 2,
        Pool::Output => 3,
    });
    put_u32(&mut body, units);
    body
}

/// Returns the body of the execution `key`'s terminal `outcome`.
fn terminal_body(key: Key, outcome: &Outcome) -> Vec<u8> {
    let mut body = key_body(key);
    let (kind, value) = match outcome.status() {
        None => (0, 0),
        Some(Status::Exited(code)) => (1, code),
        Some(Status::Signaled(signal)) => (2, signal),
    };
    body.push(kind);
    body.extend_from_slice(&value.to_le_bytes());
    body.push(outcome.cause().map_or(0, cause_code));
    let (failed, stage, error) = outcome
        .launch_failure()
        .map_or((0, 0, 0), |failure| (1, failure.stage(), failure.error()));
    body.push(failed);
    put_u32(&mut body, stage);
    body.extend_from_slice(&error.to_le_bytes());
    let mut flags = 0;
    if outcome.is_truncated(Stream::Stdout) {
        flags |= STDOUT_TRUNCATED;
    }
    if outcome.is_truncated(Stream::Stderr) {
        flags |= STDERR_TRUNCATED;
    }
    if outcome.is_reusable() {
        flags |= REUSABLE;
    }
    body.push(flags);
    body
}

/// Returns the code of `stream`.
fn stream_code(stream: Stream) -> u8 {
    match stream {
        Stream::Stdout => 1,
        Stream::Stderr => 2,
    }
}

/// Returns the code of `cause`, or [`UNKNOWN_CAUSE`] for one that the framing does not know.
fn cause_code(cause: Cause) -> u8 {
    match cause {
        Cause::Cancelled => 1,
        Cause::Dropped => 2,
        Cause::TimedOut => 3,
        Cause::Reset => 4,
        Cause::Disconnected => 5,
        _ => UNKNOWN_CAUSE,
    }
}

/// Returns the cause that `code` names, if any.
fn cause(code: u8) -> Option<Cause> {
    Some(match code {
        1 => Cause::Cancelled,
        2 => Cause::Dropped,
        3 => Cause::TimedOut,
        4 => Cause::Reset,
        5 => Cause::Disconnected,
        _ => return None,
    })
}

/// Returns the pool that `code` names, if any.
fn pool(code: u8) -> Option<Pool> {
    Some(match code {
        0 => Pool::Input,
        1 => Pool::Stdout,
        2 => Pool::Stderr,
        3 => Pool::Output,
        _ => return None,
    })
}

/// Returns the name of `pool`, for problems.
fn pool_name(pool: Pool) -> &'static str {
    match pool {
        Pool::Input => "input",
        Pool::Stdout => "standard output",
        Pool::Stderr => "standard error",
        Pool::Output => "output",
    }
}

/// Returns the window of `size` units of the unit that `code` names, or the problem.
fn window(code: u8, size: u32) -> Result<Window, String> {
    match code {
        0 => Ok(Window::bytes(size)),
        1 => Ok(Window::frames(size)),
        _ => Err(format!("names the unknown unit {code}")),
    }
}

/// Returns the flag that `code` encodes, 0 or 1, or the problem.
fn flag(code: u8) -> Result<bool, String> {
    match code {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(format!("has the flag {code}")),
    }
}

/// Returns `duration` in whole milliseconds, at most `u64::MAX`.
fn milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Appends `value` to `body`.
fn put_u32(body: &mut Vec<u8>, value: u32) {
    body.extend_from_slice(&value.to_le_bytes());
}

/// Appends `value` to `body`.
fn put_u64(body: &mut Vec<u8>, value: u64) {
    body.extend_from_slice(&value.to_le_bytes());
}

/// Appends the count `count` of a start's items to `body`, as a `u16`. A start with more items
/// than a `u16` counts exceeds the framing's limit too.
fn put_count(body: &mut Vec<u8>, count: usize) {
    let count = u16::try_from(count).unwrap_or(u16::MAX);
    body.extend_from_slice(&count.to_le_bytes());
}

/// Appends `bytes` to `body`, after their length, a `u32`.
fn put_bytes(body: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(body, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    body.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests;
