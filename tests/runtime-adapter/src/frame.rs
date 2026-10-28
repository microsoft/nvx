//! The adapter's own application framing, which its host program and guest agent exchange over the
//! control session between the host and the guest, in place of NVX's `NVXC` frames.
//!
//! A message is a header of [`HEADER_LEN`] bytes, then a body. The header holds the body's length,
//! a little-endian `u32`, then the message's kind. The session carries bytes and keeps no message
//! boundaries, so a message may span several of its records, and one record may end a message and
//! start the next. [`Decoder`] reassembles the messages: it checks each header before it takes any
//! byte of the body, and holds at most one partial message, whose body has at most [`MAX_BODY`]
//! bytes.
//!
//! The framing cannot find the next message after a malformed one, so a [`FrameError`] ends the
//! stream.
//!
//! The framing carries the body of each kind as bytes, within the kind's limit. The bodies of the
//! messages of a layered root, of its probes, and of executions have encodings of their own, which
//! the [`root`](crate::root), [`probe`](crate::probe), and [`exec`](crate::exec) modules write and
//! read, and that of a clone's activation is the canonical encoding of an activation record, which
//! `nvx::activation` writes and reads: a body that does not decode is a [`BodyError`], which the
//! receiver answers or refuses, while the stream goes on.

use std::fmt;

use nvx::activation::MAX_RECORD_BYTES;
use nvx::protocol::OUTER_MAX_PAYLOAD;

/// Bytes of a message's header: the body's length, then the message's kind.
pub const HEADER_LEN: usize = 5;

/// Most bytes of a message's body: as many as four of the session's largest records carry, so that
/// the longest message, with its header, spans five records.
pub const MAX_BODY: usize = 4 * OUTER_MAX_PAYLOAD;

/// Most bytes of the body of a root configuration, the guest's readiness, a root's failure, a
/// failure of the mapping table, a probe, or a probe's failure: short bodies, unlike an echo or a
/// probe's report.
pub const MAX_TEXT_BODY: usize = 4096;

/// Most bytes of the body of an execution's control message: its execution's key, and a few
/// fields; see [`exec`](crate::exec).
pub const MAX_CONTROL_BODY: usize = 64;

/// Most bytes of the body of an `Activate`: the canonical encoding of an activation record, which
/// holds at most `nvx::activation::MAX_RECORD_BYTES`.
pub const MAX_ACTIVATE_BODY: usize = MAX_RECORD_BYTES;

/// A message of the adapter's framing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Asks the guest to answer with [`Message::Pong`].
    Ping,
    /// The guest's answer to [`Message::Ping`].
    Pong,
    /// Bytes that the guest sends back in an `Echo` of its own.
    Echo(Vec<u8>),
    /// Asks the guest to stop serving, which it confirms with [`Message::Stopped`].
    Stop,
    /// The guest's confirmation of [`Message::Stop`], its last message.
    Stopped,
    /// The host's configuration of a layered root, what the root needs besides its boot tokens,
    /// encoded as a [`RootConfig`](crate::root::RootConfig). The guest decodes and checks it
    /// itself, so that it answers one that it refuses with [`Message::RootFailed`].
    RootConfig(Vec<u8>),
    /// The guest's answer to [`Message::RootConfig`] once it has assembled the root, or to
    /// [`Message::Activate`] once it activated a clone, with the guest agent's build, and what it
    /// reports about a clone's activation, encoded as a [`Ready`](crate::root::Ready).
    Ready(Vec<u8>),
    /// A stage of the guest's layered root failed, encoded as a
    /// [`RootFailure`](crate::root::RootFailure): the guest's answer to [`Message::RootConfig`],
    /// or its report of a failed teardown before it confirms a [`Message::Stop`].
    RootFailed(Vec<u8>),
    /// Asks the guest to run a probe, a named step of a scenario's checks, encoded as a
    /// [`Probe`](crate::probe::Probe).
    Probe(Vec<u8>),
    /// The guest's answer to [`Message::Probe`]: what the probe found, encoded as
    /// [`Facts`](crate::probe::Facts).
    Report(Vec<u8>),
    /// The guest's answer to a [`Message::Probe`] that failed: the problem, as UTF-8 text.
    ProbeFailed(Vec<u8>),
    /// Asks the guest to run a program as an execution, encoded as a
    /// [`Start`](crate::exec::Start).
    ExecStart(Vec<u8>),
    /// Bytes of an execution's standard input, from the host.
    ExecInput(Vec<u8>),
    /// The end of an execution's standard input, after its last byte, from the host.
    ExecInputEnd(Vec<u8>),
    /// The host's cancellation of an execution, with its cause.
    ExecCancel(Vec<u8>),
    /// Credit of an execution's pool: from the host, of an output pool; from the guest, of the
    /// standard input.
    ExecCredit(Vec<u8>),
    /// The guest started an execution's program.
    ExecStarted(Vec<u8>),
    /// Bytes of an execution's standard output or error, from the guest.
    ExecOutput(Vec<u8>),
    /// The end of an execution's standard output or error, after its last byte, from the guest.
    ExecEnd(Vec<u8>),
    /// The execution's standard input closed before the host ended it, from the guest.
    ExecInputClosed(Vec<u8>),
    /// An execution's terminal record, its outcome: the guest's last message about it.
    ExecTerminal(Vec<u8>),
    /// A part of the host's mapping table: consecutive mappings that the guest applies beneath
    /// its root, in order, in a `MAPS` payload of `nvx::protocol`, which
    /// [`encode_maps`](crate::root::encode_maps) writes and
    /// [`decode_maps`](crate::root::decode_maps) reads. The guest answers it with
    /// [`Message::Mapped`] or [`Message::MapFailed`].
    Maps(Vec<u8>),
    /// The guest's answer to [`Message::Maps`] once it applied the part's mappings.
    Mapped,
    /// The guest's answer to a [`Message::Maps`] whose part it refused, or one of whose mappings
    /// it failed to apply, encoded as a [`MapFailure`](crate::root::MapFailure).
    MapFailed(Vec<u8>),
    /// Asks the guest to start the sandbox's workloads once it applied the mapping table, which
    /// it confirms with [`Message::Started`].
    Start,
    /// The guest's confirmation of [`Message::Start`].
    Started,
    /// The host's activation of a clone, which a restore of a template's snapshot started: the
    /// clone's activation record, in its canonical encoding, which
    /// `nvx::activation::ActivationRecord::encode` writes, and which the guest decodes and checks
    /// itself. The guest answers it with [`Message::Ready`] once it activated the clone.
    Activate(Vec<u8>),
}

impl Message {
    /// Returns the message's kind, the last byte of its header.
    pub fn kind(&self) -> u8 {
        self.parts().0.code()
    }

    /// Returns the message's encoding: its header, then its body, which has at most the bytes that
    /// its kind allows: [`MAX_BODY`] for an `Echo`, a `Report`, an execution's start, or its input
    /// or output, or a part of the mapping table, [`MAX_TEXT_BODY`] for a root configuration, the
    /// guest's readiness, a root's failure, a failure of the mapping table, a probe, or a probe's
    /// failure, [`MAX_CONTROL_BODY`] for an execution's other messages, [`MAX_ACTIVATE_BODY`] for
    /// an `Activate`, and none for the others.
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        let (kind, body) = self.parts();
        if body.len() > kind.max_body() {
            return Err(FrameError::Length {
                kind: kind.code(),
                length: body.len(),
            });
        }
        // The body has at most `MAX_BODY` bytes, so its length fits the header's field.
        let length = body.len() as u32;
        let mut encoded = Vec::with_capacity(HEADER_LEN + body.len());
        encoded.extend_from_slice(&length.to_le_bytes());
        encoded.push(kind.code());
        encoded.extend_from_slice(body);
        Ok(encoded)
    }

    /// Returns the message's kind and body.
    fn parts(&self) -> (Kind, &[u8]) {
        match self {
            Self::Ping => (Kind::Ping, &[]),
            Self::Pong => (Kind::Pong, &[]),
            Self::Echo(body) => (Kind::Echo, body),
            Self::Stop => (Kind::Stop, &[]),
            Self::Stopped => (Kind::Stopped, &[]),
            Self::RootConfig(body) => (Kind::RootConfig, body),
            Self::Ready(body) => (Kind::Ready, body),
            Self::RootFailed(body) => (Kind::RootFailed, body),
            Self::Probe(body) => (Kind::Probe, body),
            Self::Report(body) => (Kind::Report, body),
            Self::ProbeFailed(body) => (Kind::ProbeFailed, body),
            Self::ExecStart(body) => (Kind::ExecStart, body),
            Self::ExecInput(body) => (Kind::ExecInput, body),
            Self::ExecInputEnd(body) => (Kind::ExecInputEnd, body),
            Self::ExecCancel(body) => (Kind::ExecCancel, body),
            Self::ExecCredit(body) => (Kind::ExecCredit, body),
            Self::ExecStarted(body) => (Kind::ExecStarted, body),
            Self::ExecOutput(body) => (Kind::ExecOutput, body),
            Self::ExecEnd(body) => (Kind::ExecEnd, body),
            Self::ExecInputClosed(body) => (Kind::ExecInputClosed, body),
            Self::ExecTerminal(body) => (Kind::ExecTerminal, body),
            Self::Maps(body) => (Kind::Maps, body),
            Self::Mapped => (Kind::Mapped, &[]),
            Self::MapFailed(body) => (Kind::MapFailed, body),
            Self::Start => (Kind::Start, &[]),
            Self::Started => (Kind::Started, &[]),
            Self::Activate(body) => (Kind::Activate, body),
        }
    }
}

/// Kind of a [`Message::Ping`].
const KIND_PING: u8 = 1;
/// Kind of a [`Message::Pong`].
const KIND_PONG: u8 = 2;
/// Kind of a [`Message::Echo`].
const KIND_ECHO: u8 = 3;
/// Kind of a [`Message::Stop`].
const KIND_STOP: u8 = 4;
/// Kind of a [`Message::Stopped`].
const KIND_STOPPED: u8 = 5;
/// Kind of a [`Message::RootConfig`].
const KIND_ROOT_CONFIG: u8 = 6;
/// Kind of a [`Message::Ready`].
const KIND_READY: u8 = 7;
/// Kind of a [`Message::RootFailed`].
const KIND_ROOT_FAILED: u8 = 8;
/// Kind of a [`Message::Probe`].
const KIND_PROBE: u8 = 9;
/// Kind of a [`Message::Report`].
const KIND_REPORT: u8 = 10;
/// Kind of a [`Message::ProbeFailed`].
const KIND_PROBE_FAILED: u8 = 11;
/// Kind of a [`Message::ExecStart`].
const KIND_EXEC_START: u8 = 12;
/// Kind of a [`Message::ExecInput`].
const KIND_EXEC_INPUT: u8 = 13;
/// Kind of a [`Message::ExecInputEnd`].
const KIND_EXEC_INPUT_END: u8 = 14;
/// Kind of a [`Message::ExecCancel`].
const KIND_EXEC_CANCEL: u8 = 15;
/// Kind of a [`Message::ExecCredit`].
const KIND_EXEC_CREDIT: u8 = 16;
/// Kind of a [`Message::ExecStarted`].
const KIND_EXEC_STARTED: u8 = 17;
/// Kind of a [`Message::ExecOutput`].
const KIND_EXEC_OUTPUT: u8 = 18;
/// Kind of a [`Message::ExecEnd`].
const KIND_EXEC_END: u8 = 19;
/// Kind of a [`Message::ExecInputClosed`].
const KIND_EXEC_INPUT_CLOSED: u8 = 20;
/// Kind of a [`Message::ExecTerminal`].
const KIND_EXEC_TERMINAL: u8 = 21;
/// Kind of a [`Message::Maps`].
const KIND_MAPS: u8 = 22;
/// Kind of a [`Message::Mapped`].
const KIND_MAPPED: u8 = 23;
/// Kind of a [`Message::MapFailed`].
const KIND_MAP_FAILED: u8 = 24;
/// Kind of a [`Message::Start`].
const KIND_START: u8 = 25;
/// Kind of a [`Message::Started`].
const KIND_STARTED: u8 = 26;
/// Kind of a [`Message::Activate`].
const KIND_ACTIVATE: u8 = 27;

/// Kind of a [`Message`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Kind {
    /// [`Message::Ping`].
    Ping = KIND_PING,
    /// [`Message::Pong`].
    Pong = KIND_PONG,
    /// [`Message::Echo`].
    Echo = KIND_ECHO,
    /// [`Message::Stop`].
    Stop = KIND_STOP,
    /// [`Message::Stopped`].
    Stopped = KIND_STOPPED,
    /// [`Message::RootConfig`].
    RootConfig = KIND_ROOT_CONFIG,
    /// [`Message::Ready`].
    Ready = KIND_READY,
    /// [`Message::RootFailed`].
    RootFailed = KIND_ROOT_FAILED,
    /// [`Message::Probe`].
    Probe = KIND_PROBE,
    /// [`Message::Report`].
    Report = KIND_REPORT,
    /// [`Message::ProbeFailed`].
    ProbeFailed = KIND_PROBE_FAILED,
    /// [`Message::ExecStart`].
    ExecStart = KIND_EXEC_START,
    /// [`Message::ExecInput`].
    ExecInput = KIND_EXEC_INPUT,
    /// [`Message::ExecInputEnd`].
    ExecInputEnd = KIND_EXEC_INPUT_END,
    /// [`Message::ExecCancel`].
    ExecCancel = KIND_EXEC_CANCEL,
    /// [`Message::ExecCredit`].
    ExecCredit = KIND_EXEC_CREDIT,
    /// [`Message::ExecStarted`].
    ExecStarted = KIND_EXEC_STARTED,
    /// [`Message::ExecOutput`].
    ExecOutput = KIND_EXEC_OUTPUT,
    /// [`Message::ExecEnd`].
    ExecEnd = KIND_EXEC_END,
    /// [`Message::ExecInputClosed`].
    ExecInputClosed = KIND_EXEC_INPUT_CLOSED,
    /// [`Message::ExecTerminal`].
    ExecTerminal = KIND_EXEC_TERMINAL,
    /// [`Message::Maps`].
    Maps = KIND_MAPS,
    /// [`Message::Mapped`].
    Mapped = KIND_MAPPED,
    /// [`Message::MapFailed`].
    MapFailed = KIND_MAP_FAILED,
    /// [`Message::Start`].
    Start = KIND_START,
    /// [`Message::Started`].
    Started = KIND_STARTED,
    /// [`Message::Activate`].
    Activate = KIND_ACTIVATE,
}

impl Kind {
    /// Returns the kind with `code`, if the framing defines one.
    fn from_code(code: u8) -> Option<Self> {
        match code {
            KIND_PING => Some(Self::Ping),
            KIND_PONG => Some(Self::Pong),
            KIND_ECHO => Some(Self::Echo),
            KIND_STOP => Some(Self::Stop),
            KIND_STOPPED => Some(Self::Stopped),
            KIND_ROOT_CONFIG => Some(Self::RootConfig),
            KIND_READY => Some(Self::Ready),
            KIND_ROOT_FAILED => Some(Self::RootFailed),
            KIND_PROBE => Some(Self::Probe),
            KIND_REPORT => Some(Self::Report),
            KIND_PROBE_FAILED => Some(Self::ProbeFailed),
            KIND_EXEC_START => Some(Self::ExecStart),
            KIND_EXEC_INPUT => Some(Self::ExecInput),
            KIND_EXEC_INPUT_END => Some(Self::ExecInputEnd),
            KIND_EXEC_CANCEL => Some(Self::ExecCancel),
            KIND_EXEC_CREDIT => Some(Self::ExecCredit),
            KIND_EXEC_STARTED => Some(Self::ExecStarted),
            KIND_EXEC_OUTPUT => Some(Self::ExecOutput),
            KIND_EXEC_END => Some(Self::ExecEnd),
            KIND_EXEC_INPUT_CLOSED => Some(Self::ExecInputClosed),
            KIND_EXEC_TERMINAL => Some(Self::ExecTerminal),
            KIND_MAPS => Some(Self::Maps),
            KIND_MAPPED => Some(Self::Mapped),
            KIND_MAP_FAILED => Some(Self::MapFailed),
            KIND_START => Some(Self::Start),
            KIND_STARTED => Some(Self::Started),
            KIND_ACTIVATE => Some(Self::Activate),
            _ => None,
        }
    }

    /// Returns the kind's code, the last byte of a header.
    fn code(self) -> u8 {
        self as u8
    }

    /// Returns the most bytes of a body of this kind.
    fn max_body(self) -> usize {
        match self {
            Self::Echo
            | Self::Report
            | Self::ExecStart
            | Self::ExecInput
            | Self::ExecOutput
            | Self::Maps => MAX_BODY,
            Self::RootConfig
            | Self::Ready
            | Self::RootFailed
            | Self::Probe
            | Self::ProbeFailed
            | Self::MapFailed => MAX_TEXT_BODY,
            Self::ExecInputEnd
            | Self::ExecCancel
            | Self::ExecCredit
            | Self::ExecStarted
            | Self::ExecEnd
            | Self::ExecInputClosed
            | Self::ExecTerminal => MAX_CONTROL_BODY,
            Self::Activate => MAX_ACTIVATE_BODY,
            Self::Ping
            | Self::Pong
            | Self::Stop
            | Self::Stopped
            | Self::Mapped
            | Self::Start
            | Self::Started => 0,
        }
    }

    /// Returns the message of this kind with `body`, which has at most
    /// [`max_body`](Self::max_body) bytes.
    fn message(self, body: Vec<u8>) -> Message {
        match self {
            Self::Ping => Message::Ping,
            Self::Pong => Message::Pong,
            Self::Echo => Message::Echo(body),
            Self::Stop => Message::Stop,
            Self::Stopped => Message::Stopped,
            Self::RootConfig => Message::RootConfig(body),
            Self::Ready => Message::Ready(body),
            Self::RootFailed => Message::RootFailed(body),
            Self::Probe => Message::Probe(body),
            Self::Report => Message::Report(body),
            Self::ProbeFailed => Message::ProbeFailed(body),
            Self::ExecStart => Message::ExecStart(body),
            Self::ExecInput => Message::ExecInput(body),
            Self::ExecInputEnd => Message::ExecInputEnd(body),
            Self::ExecCancel => Message::ExecCancel(body),
            Self::ExecCredit => Message::ExecCredit(body),
            Self::ExecStarted => Message::ExecStarted(body),
            Self::ExecOutput => Message::ExecOutput(body),
            Self::ExecEnd => Message::ExecEnd(body),
            Self::ExecInputClosed => Message::ExecInputClosed(body),
            Self::ExecTerminal => Message::ExecTerminal(body),
            Self::Maps => Message::Maps(body),
            Self::Mapped => Message::Mapped,
            Self::MapFailed => Message::MapFailed(body),
            Self::Start => Message::Start,
            Self::Started => Message::Started,
            Self::Activate => Message::Activate(body),
        }
    }
}

/// A malformed message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    /// A kind that the framing does not define.
    Kind(u8),
    /// A body longer than its kind allows: [`MAX_BODY`] bytes, [`MAX_TEXT_BODY`],
    /// [`MAX_CONTROL_BODY`], [`MAX_ACTIVATE_BODY`], or none.
    Length {
        /// Kind of the message.
        kind: u8,
        /// Bytes of the body.
        length: usize,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kind(kind) => write!(formatter, "unknown message kind {kind}"),
            Self::Length { kind, length } => write!(
                formatter,
                "message of kind {kind} with a body of {length} bytes"
            ),
        }
    }
}

impl std::error::Error for FrameError {}

/// A body that does not decode as its kind's encoding, such as a root configuration that its guest
/// refuses. Unlike a [`FrameError`], it leaves the stream intact, at the start of the next message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyError(String);

impl BodyError {
    /// Returns the error that `problem` describes.
    pub fn new(problem: impl Into<String>) -> Self {
        Self(problem.into())
    }

    /// Returns the problem.
    pub fn problem(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BodyError {}

/// Returns the longest start of `text` of at most `limit` bytes that ends at a character's
/// boundary.
pub(crate) fn truncated(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Reassembles messages from bytes that arrive in pieces of any size, such as the payloads of the
/// control session's records.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    /// Header of the message in progress.
    header: [u8; HEADER_LEN],
    /// Bytes of the header that arrived.
    header_len: usize,
    /// Kind of the message in progress, once its header arrived and checked out.
    kind: Option<Kind>,
    /// Bytes of the body that the header announced.
    body_len: usize,
    /// Bytes of the body that arrived.
    body: Vec<u8>,
}

impl Decoder {
    /// Creates a decoder that expects the start of a message.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes bytes until a message completes or `input` runs out.
    ///
    /// Returns the number of bytes consumed and the message that the last of them completed, if
    /// any. Bytes after that message are not consumed, so the caller passes them again. A
    /// malformed header fails before any byte of its body is taken, and leaves the decoder
    /// empty.
    pub fn push(&mut self, input: &[u8]) -> Result<(usize, Option<Message>), FrameError> {
        let mut consumed = 0;
        let kind = match self.kind {
            Some(kind) => kind,
            None => {
                consumed = (HEADER_LEN - self.header_len).min(input.len());
                self.header[self.header_len..][..consumed].copy_from_slice(&input[..consumed]);
                self.header_len += consumed;
                if self.header_len < HEADER_LEN {
                    return Ok((consumed, None));
                }
                let [b0, b1, b2, b3, code] = self.header;
                // A length that `usize` cannot hold exceeds every limit.
                let length =
                    usize::try_from(u32::from_le_bytes([b0, b1, b2, b3])).unwrap_or(usize::MAX);
                let kind = Kind::from_code(code).ok_or(FrameError::Kind(code));
                let checked = kind.and_then(|kind| {
                    if length > kind.max_body() {
                        Err(FrameError::Length { kind: code, length })
                    } else {
                        Ok(kind)
                    }
                });
                let kind = checked.inspect_err(|_| self.clear())?;
                self.kind = Some(kind);
                self.body_len = length;
                self.body.reserve_exact(length);
                kind
            }
        };
        let taken = (self.body_len - self.body.len()).min(input.len() - consumed);
        self.body
            .extend_from_slice(&input[consumed..consumed + taken]);
        consumed += taken;
        if self.body.len() < self.body_len {
            return Ok((consumed, None));
        }
        let body = std::mem::take(&mut self.body);
        self.clear();
        Ok((consumed, Some(kind.message(body))))
    }

    /// Drops the message in progress, if any, such as when the session resets and its new epoch
    /// starts with a new message.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Returns whether part of a message arrived.
    pub fn is_partial(&self) -> bool {
        self.header_len != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Decoder, FrameError, HEADER_LEN, Kind, MAX_ACTIVATE_BODY, MAX_BODY, MAX_CONTROL_BODY,
        MAX_TEXT_BODY, Message,
    };

    /// Returns one message of each kind, with bodies of no, few, and the most bytes.
    fn messages() -> Vec<Message> {
        vec![
            Message::Ping,
            Message::Echo(Vec::new()),
            Message::Echo(b"hello".to_vec()),
            Message::Pong,
            Message::Echo((0..=u8::MAX).cycle().take(MAX_BODY).collect()),
            Message::RootConfig(b"version=1\n".to_vec()),
            Message::RootConfig(vec![b'c'; MAX_TEXT_BODY]),
            Message::Ready(b"\x050.1.0".to_vec()),
            Message::Ready(vec![b'r'; MAX_TEXT_BODY]),
            Message::RootFailed(Vec::new()),
            Message::RootFailed(vec![b'f'; MAX_TEXT_BODY]),
            Message::Probe(b"mounts".to_vec()),
            Message::Probe(vec![b'p'; MAX_TEXT_BODY]),
            Message::Report(b"key=value\n".to_vec()),
            Message::Report((0..=u8::MAX).rev().cycle().take(MAX_BODY).collect()),
            Message::ProbeFailed(b"unknown probe".to_vec()),
            Message::ProbeFailed(vec![b'e'; MAX_TEXT_BODY]),
            Message::ExecStart(vec![b's'; MAX_BODY]),
            Message::ExecInput(Vec::new()),
            Message::ExecInput((0..=u8::MAX).cycle().take(MAX_BODY).collect()),
            Message::ExecInputEnd(vec![1; MAX_CONTROL_BODY]),
            Message::ExecCancel(vec![2; 17]),
            Message::ExecCredit(vec![3; 21]),
            Message::ExecStarted(vec![4; 16]),
            Message::ExecOutput((0..=u8::MAX).rev().cycle().take(MAX_BODY).collect()),
            Message::ExecEnd(vec![5; 17]),
            Message::ExecInputClosed(Vec::new()),
            Message::ExecTerminal(vec![6; MAX_CONTROL_BODY]),
            Message::Maps(Vec::new()),
            Message::Maps((0..=u8::MAX).cycle().take(MAX_BODY).collect()),
            Message::Mapped,
            Message::MapFailed(b"\x02\0\0\0refused".to_vec()),
            Message::MapFailed(vec![b'm'; MAX_TEXT_BODY]),
            Message::Start,
            Message::Started,
            Message::Activate(Vec::new()),
            Message::Activate(b"\x01\x00record".to_vec()),
            Message::Activate(vec![b'a'; MAX_ACTIVATE_BODY]),
            Message::Stop,
            Message::Stopped,
        ]
    }

    /// Returns a message of each kind, in the order of their codes, with a body of a few bytes if
    /// the kind has one.
    fn one_of_each() -> Vec<Message> {
        let body = |code: u8| vec![code; usize::from(code % 4) + 1];
        vec![
            Message::Ping,
            Message::Pong,
            Message::Echo(body(3)),
            Message::Stop,
            Message::Stopped,
            Message::RootConfig(body(6)),
            Message::Ready(body(7)),
            Message::RootFailed(body(8)),
            Message::Probe(body(9)),
            Message::Report(body(10)),
            Message::ProbeFailed(body(11)),
            Message::ExecStart(body(12)),
            Message::ExecInput(body(13)),
            Message::ExecInputEnd(body(14)),
            Message::ExecCancel(body(15)),
            Message::ExecCredit(body(16)),
            Message::ExecStarted(body(17)),
            Message::ExecOutput(body(18)),
            Message::ExecEnd(body(19)),
            Message::ExecInputClosed(body(20)),
            Message::ExecTerminal(body(21)),
            Message::Maps(body(22)),
            Message::Mapped,
            Message::MapFailed(body(24)),
            Message::Start,
            Message::Started,
            Message::Activate(body(27)),
        ]
    }

    /// Returns the encodings of `messages`, one after the other.
    fn stream(messages: &[Message]) -> Vec<u8> {
        messages
            .iter()
            .flat_map(|message| message.encode().unwrap())
            .collect()
    }

    /// Decodes `pieces` in order, and returns the messages that they complete.
    fn decode<'a>(
        decoder: &mut Decoder,
        pieces: impl IntoIterator<Item = &'a [u8]>,
    ) -> Vec<Message> {
        let mut decoded = Vec::new();
        for piece in pieces {
            let mut rest = piece;
            while !rest.is_empty() {
                let (consumed, message) = decoder.push(rest).unwrap();
                assert_ne!(consumed, 0, "a push consumes bytes");
                rest = &rest[consumed..];
                decoded.extend(message);
            }
        }
        decoded
    }

    #[test]
    fn the_header_holds_the_body_length_then_the_kind() {
        assert_eq!(Message::Ping.encode().unwrap(), [0, 0, 0, 0, 1]);
        assert_eq!(Message::Pong.encode().unwrap(), [0, 0, 0, 0, 2]);
        assert_eq!(
            Message::Echo(b"abc".to_vec()).encode().unwrap(),
            [3, 0, 0, 0, 3, b'a', b'b', b'c']
        );
        assert_eq!(Message::Stop.encode().unwrap(), [0, 0, 0, 0, 4]);
        assert_eq!(Message::Stopped.encode().unwrap(), [0, 0, 0, 0, 5]);
        assert_eq!(
            Message::RootConfig(b"c".to_vec()).encode().unwrap(),
            [1, 0, 0, 0, 6, b'c']
        );
        assert_eq!(
            Message::Ready(b"\x01r".to_vec()).encode().unwrap(),
            [2, 0, 0, 0, 7, 1, b'r']
        );
        assert_eq!(
            Message::RootFailed(b"f".to_vec()).encode().unwrap(),
            [1, 0, 0, 0, 8, b'f']
        );
        assert_eq!(
            Message::Probe(b"p".to_vec()).encode().unwrap(),
            [1, 0, 0, 0, 9, b'p']
        );
        assert_eq!(
            Message::Report(b"r".to_vec()).encode().unwrap(),
            [1, 0, 0, 0, 10, b'r']
        );
        assert_eq!(
            Message::ProbeFailed(b"e".to_vec()).encode().unwrap(),
            [1, 0, 0, 0, 11, b'e']
        );
        let exec = [
            Message::ExecStart(b"x".to_vec()),
            Message::ExecInput(b"x".to_vec()),
            Message::ExecInputEnd(b"x".to_vec()),
            Message::ExecCancel(b"x".to_vec()),
            Message::ExecCredit(b"x".to_vec()),
            Message::ExecStarted(b"x".to_vec()),
            Message::ExecOutput(b"x".to_vec()),
            Message::ExecEnd(b"x".to_vec()),
            Message::ExecInputClosed(b"x".to_vec()),
            Message::ExecTerminal(b"x".to_vec()),
        ];
        for (kind, message) in (12..).zip(exec) {
            assert_eq!(message.encode().unwrap(), [1, 0, 0, 0, kind, b'x']);
        }
        assert_eq!(
            Message::Maps(b"x".to_vec()).encode().unwrap(),
            [1, 0, 0, 0, 22, b'x']
        );
        assert_eq!(Message::Mapped.encode().unwrap(), [0, 0, 0, 0, 23]);
        assert_eq!(
            Message::MapFailed(b"x".to_vec()).encode().unwrap(),
            [1, 0, 0, 0, 24, b'x']
        );
        assert_eq!(Message::Start.encode().unwrap(), [0, 0, 0, 0, 25]);
        assert_eq!(Message::Started.encode().unwrap(), [0, 0, 0, 0, 26]);
        assert_eq!(
            Message::Activate(b"x".to_vec()).encode().unwrap(),
            [1, 0, 0, 0, 27, b'x']
        );
        // The kinds are numbered from 1, without gaps.
        for (kind, message) in (1..).zip(one_of_each()) {
            assert_eq!(message.kind(), kind, "{message:?}");
        }
        for message in messages().into_iter().chain(one_of_each()) {
            assert_eq!(message.encode().unwrap()[HEADER_LEN - 1], message.kind());
        }
    }

    #[test]
    fn each_kind_decodes_from_its_code_alone() {
        for code in 0..=u8::MAX {
            let defined = (1..=27).contains(&code).then_some(code);
            assert_eq!(Kind::from_code(code).map(Kind::code), defined, "{code}");
        }
    }

    #[test]
    fn messages_arrive_whole_in_pieces_of_any_size() {
        let messages = messages();
        let stream = stream(&messages);
        for size in [1, 2, 3, HEADER_LEN, 7, 4096, 64 * 1024, stream.len()] {
            let mut decoder = Decoder::new();
            assert_eq!(
                decode(&mut decoder, stream.chunks(size)),
                messages,
                "{size}"
            );
            assert!(!decoder.is_partial());
        }
    }

    #[test]
    fn messages_arrive_whole_across_a_split_at_every_byte() {
        let mut messages = vec![
            Message::Echo(b"split".to_vec()),
            Message::Ping,
            Message::Stop,
        ];
        // And a message of each kind, those without a body included.
        messages.extend(one_of_each());
        let stream = stream(&messages);
        for split in 0..=stream.len() {
            let mut decoder = Decoder::new();
            let pieces = [&stream[..split], &stream[split..]];
            assert_eq!(decode(&mut decoder, pieces), messages, "{split}");
            assert!(!decoder.is_partial());
        }
    }

    #[test]
    fn a_push_stops_at_the_end_of_a_message() {
        let stream = stream(&[Message::Ping, Message::Echo(b"next".to_vec())]);
        let mut decoder = Decoder::new();
        assert_eq!(decoder.push(&stream), Ok((HEADER_LEN, Some(Message::Ping))));
        assert!(!decoder.is_partial());
        assert_eq!(
            decoder.push(&stream[HEADER_LEN..]),
            Ok((HEADER_LEN + 4, Some(Message::Echo(b"next".to_vec()))))
        );
    }

    #[test]
    fn malformed_headers_fail_before_their_body_is_taken() {
        let too_long = u32::try_from(MAX_BODY + 1).unwrap().to_le_bytes();
        let too_long_text = u32::try_from(MAX_TEXT_BODY + 1).unwrap().to_le_bytes();
        let too_long_control = u32::try_from(MAX_CONTROL_BODY + 1).unwrap().to_le_bytes();
        let too_long_record = u32::try_from(MAX_ACTIVATE_BODY + 1).unwrap().to_le_bytes();
        let header = |length: [u8; 4], kind: u8| [length[0], length[1], length[2], length[3], kind];
        let text = |kind: u8| header(too_long_text, kind);
        let mut cases = vec![
            ([0, 0, 0, 0, 0], FrameError::Kind(0)),
            ([0, 0, 0, 0, 28], FrameError::Kind(28)),
            ([9, 0, 0, 0, 0xff], FrameError::Kind(0xff)),
            (
                [0xff, 0xff, 0xff, 0xff, 3],
                FrameError::Length {
                    kind: 3,
                    length: u32::MAX as usize,
                },
            ),
            (
                header(too_long_record, 27),
                FrameError::Length {
                    kind: 27,
                    length: MAX_ACTIVATE_BODY + 1,
                },
            ),
        ];
        // The kinds without a body.
        for kind in [1, 2, 4, 5, 23, 25, 26] {
            let error = FrameError::Length { kind, length: 1 };
            cases.push(([1, 0, 0, 0, kind], error));
        }
        for kind in [3, 10, 12, 13, 18, 22] {
            let length = MAX_BODY + 1;
            cases.push((header(too_long, kind), FrameError::Length { kind, length }));
        }
        for kind in [6, 7, 8, 9, 11, 24] {
            let length = MAX_TEXT_BODY + 1;
            cases.push((text(kind), FrameError::Length { kind, length }));
        }
        for kind in [14, 15, 16, 17, 19, 20, 21] {
            let length = MAX_CONTROL_BODY + 1;
            let error = FrameError::Length { kind, length };
            cases.push((header(too_long_control, kind), error));
        }
        for (header, error) in cases {
            let mut decoder = Decoder::new();
            assert_eq!(decoder.push(&header[..2]), Ok((2, None)));
            assert!(decoder.is_partial());
            let mut rest = header[2..].to_vec();
            rest.extend_from_slice(b"body");
            assert_eq!(decoder.push(&rest), Err(error), "{header:?}");
            assert!(!decoder.is_partial(), "{header:?}");
            assert_eq!(decoder.body.capacity(), 0, "{header:?}");
        }
    }

    #[test]
    fn clearing_drops_a_partial_message() {
        let echo = Message::Echo(vec![1; 100]).encode().unwrap();
        let mut decoder = Decoder::new();
        assert_eq!(decoder.push(&echo[..50]), Ok((50, None)));
        assert!(decoder.is_partial());
        decoder.clear();
        assert!(!decoder.is_partial());
        let ping = Message::Ping.encode().unwrap();
        assert_eq!(decoder.push(&ping), Ok((HEADER_LEN, Some(Message::Ping))));
    }

    #[test]
    fn encoding_refuses_a_body_beyond_the_limit() {
        let longest = Message::Echo(vec![0; MAX_BODY]).encode().unwrap();
        assert_eq!(longest.len(), HEADER_LEN + MAX_BODY);
        let report = Message::Report(vec![0; MAX_BODY]).encode().unwrap();
        assert_eq!(report.len(), HEADER_LEN + MAX_BODY);
        let maps = Message::Maps(vec![0; MAX_BODY]).encode().unwrap();
        assert_eq!(maps.len(), HEADER_LEN + MAX_BODY);
        let ready = Message::Ready(vec![0; MAX_TEXT_BODY]).encode().unwrap();
        assert_eq!(ready.len(), HEADER_LEN + MAX_TEXT_BODY);
        // An Activate carries the largest record, whose limit is the activation record's own.
        assert_eq!(MAX_ACTIVATE_BODY, nvx::activation::MAX_RECORD_BYTES);
        let activate = Message::Activate(vec![0; MAX_ACTIVATE_BODY])
            .encode()
            .unwrap();
        assert_eq!(activate.len(), HEADER_LEN + MAX_ACTIVATE_BODY);
        let cases = [
            (Message::Echo(vec![0; MAX_BODY + 1]), 3, MAX_BODY + 1),
            (Message::Report(vec![0; MAX_BODY + 1]), 10, MAX_BODY + 1),
            (
                Message::RootConfig(vec![0; MAX_TEXT_BODY + 1]),
                6,
                MAX_TEXT_BODY + 1,
            ),
            (
                Message::Ready(vec![0; MAX_TEXT_BODY + 1]),
                7,
                MAX_TEXT_BODY + 1,
            ),
            (
                Message::RootFailed(vec![0; MAX_TEXT_BODY + 1]),
                8,
                MAX_TEXT_BODY + 1,
            ),
            (
                Message::Probe(vec![0; MAX_TEXT_BODY + 1]),
                9,
                MAX_TEXT_BODY + 1,
            ),
            (
                Message::ProbeFailed(vec![0; MAX_TEXT_BODY + 1]),
                11,
                MAX_TEXT_BODY + 1,
            ),
            (Message::ExecStart(vec![0; MAX_BODY + 1]), 12, MAX_BODY + 1),
            (Message::ExecInput(vec![0; MAX_BODY + 1]), 13, MAX_BODY + 1),
            (Message::ExecOutput(vec![0; MAX_BODY + 1]), 18, MAX_BODY + 1),
            (
                Message::ExecCredit(vec![0; MAX_CONTROL_BODY + 1]),
                16,
                MAX_CONTROL_BODY + 1,
            ),
            (
                Message::ExecTerminal(vec![0; MAX_CONTROL_BODY + 1]),
                21,
                MAX_CONTROL_BODY + 1,
            ),
            (Message::Maps(vec![0; MAX_BODY + 1]), 22, MAX_BODY + 1),
            (
                Message::MapFailed(vec![0; MAX_TEXT_BODY + 1]),
                24,
                MAX_TEXT_BODY + 1,
            ),
            (
                Message::Activate(vec![0; MAX_ACTIVATE_BODY + 1]),
                27,
                MAX_ACTIVATE_BODY + 1,
            ),
        ];
        for (message, kind, length) in cases {
            assert_eq!(
                message.encode(),
                Err(FrameError::Length { kind, length }),
                "{kind}"
            );
        }
    }
}
