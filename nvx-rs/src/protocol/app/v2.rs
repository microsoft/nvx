//! Version 2 of the application frames (magic `NVXC`), which carries every control path and
//! streams an execution's standard input, output and error.
//!
//! A frame has version 1's 24-byte header, with version [`VERSION`]: the magic, the version, the
//! frame kind, two zero flags bytes, the request ID, a status, and the payload length, followed by
//! the payload. Its integers are little-endian. A frame fits one outer data record, so its
//! payload holds at most [`MAX_PAYLOAD`] bytes. Its request ID is never zero: a request takes a
//! new one, its answers repeat it, and so do the frames of the execution that an `EXEC` starts, in
//! both directions. The status is the exit status of an `EXIT` and the error number of an
//! `ERROR`, and zero in every other frame.
//!
//! | Kind | Code | From | Payload |
//! | --- | --- | --- | --- |
//! | `PING` | `0x01` | Host | None |
//! | `EXEC` | `0x02` | Host | An `EXEC` payload |
//! | `STOP` | `0x03` | Host | None |
//! | `CANCEL` | `0x04` | Host | None |
//! | `MAPS` | `0x06` | Host | A `MAPS` payload |
//! | `CAPTURE` | `0x07` | Host | The capture request's flag |
//! | `ACTIVATE` | `0x08` | Host | The activate request's workload identity and activation record |
//! | `STDIN` | `0x09` | Host | 1 or more bytes of an execution's standard input |
//! | `STDIN_END` | `0x0a` | Host | None |
//! | `READY` | `0x81` | Guest | The protocol version, one byte, then, in answer to `ACTIVATE`, what Ready reports about the activation |
//! | `STDOUT` | `0x82` | Guest | 1 or more bytes of an execution's standard output |
//! | `STDERR` | `0x83` | Guest | 1 or more bytes of an execution's standard error |
//! | `EXIT` | `0x84` | Guest | How the execution ended, an [`Outcome`], as text |
//! | `STOPPED` | `0x85` | Guest | None |
//! | `STDIN_ACK` | `0x86` | Guest | How many more bytes of standard input the guest agent wrote, a `u32` from 1 to [`STDIN_WINDOW`] |
//! | `BUSY` | `0x87` | Guest | None |
//! | `ERROR` | `0xff` | Guest | The error category, such as `invalid-request`, as 1 or more bytes of text |
//!
//! The exchanges of version 1 keep their payloads and their order. `PING` asks whether the guest
//! agent is ready, and `READY` answers with the protocol version, which the host checks. `MAPS`,
//! `CAPTURE` and `ACTIVATE` carry the payloads that their codecs encode and decode, as in
//! version 1, and `READY` acknowledges them; in answer to `ACTIVATE`, it also carries what Ready
//! reports about the activation, in the canonical encoding of
//! [`ReadyActivation`](crate::activation::ReadyActivation). `ERROR` refuses a request, and `BUSY`
//! refuses one while an execution runs. `STOP` shuts the VM down, also while an execution runs,
//! and `STOPPED` acknowledges it.
//!
//! An `EXEC` starts an execution, which its `EXEC` payload describes as in version 1, and which
//! its request ID names until the `EXIT` that ends it, or the `ERROR` that refuses it. Its
//! standard output and error come in `STDOUT` and `STDERR` frames, with no total cap. The host
//! sends its standard input in `STDIN` frames, and ends it with `STDIN_END` after the bytes
//! before it. The guest agent acknowledges the bytes of `STDIN` frames with `STDIN_ACK` once it
//! has written them into the execution's standard input, and the host never has more than
//! [`STDIN_WINDOW`] bytes that the guest agent has not acknowledged. `CANCEL` ends the execution,
//! whose `EXIT` reports it. Version 1's `FEATURES` request, `0x05`, has no place in version 2.
//!
//! NVX's guest agents and the fake OpenVMM run one execution at a time. A `STOP` while it runs
//! ends it, whose `EXIT` comes before `STOPPED`. Bytes that its standard input never took, as
//! once the workload closed it, stay unacknowledged; `STDIN`, `STDIN_END` and `CANCEL` of an
//! execution that ended get no answer; and standard input beyond the window, or after its end,
//! ends the session.

use crate::protocol::{OUTER_MAX_PAYLOAD, ProtocolError, array, violation};

/// Version of the frame format and of the protocol, which each header and each `READY` carry.
pub(crate) const VERSION: u8 = 2;
/// Largest payload of a frame, which fits one outer data record with its header.
pub(crate) const MAX_PAYLOAD: usize = OUTER_MAX_PAYLOAD - HEADER_LEN;
/// Most bytes of an execution's standard input that the host may have sent in `STDIN` frames and
/// the guest agent not yet acknowledged with `STDIN_ACK`.
pub(crate) const STDIN_WINDOW: u32 = 64 * 1024;

/// Length of a frame's header.
const HEADER_LEN: usize = 24;
/// Magic of every frame.
const MAGIC: [u8; 4] = *b"NVXC";
/// Kind of [`Frame::Ping`].
const PING: u8 = 0x01;
/// Kind of [`Frame::Exec`].
const EXEC: u8 = 0x02;
/// Kind of [`Frame::Stop`].
const STOP: u8 = 0x03;
/// Kind of [`Frame::Cancel`].
const CANCEL: u8 = 0x04;
/// Kind of [`Frame::Maps`].
const MAPS: u8 = 0x06;
/// Kind of [`Frame::Capture`].
const CAPTURE: u8 = 0x07;
/// Kind of [`Frame::Activate`].
const ACTIVATE: u8 = 0x08;
/// Kind of [`Frame::Stdin`].
const STDIN: u8 = 0x09;
/// Kind of [`Frame::StdinEnd`].
const STDIN_END: u8 = 0x0a;
/// Kind of [`Frame::Ready`].
const READY: u8 = 0x81;
/// Kind of [`Frame::Stdout`].
const STDOUT: u8 = 0x82;
/// Kind of [`Frame::Stderr`].
const STDERR: u8 = 0x83;
/// Kind of [`Frame::Exit`].
const EXIT: u8 = 0x84;
/// Kind of [`Frame::Stopped`].
const STOPPED: u8 = 0x85;
/// Kind of [`Frame::StdinAck`].
const STDIN_ACK: u8 = 0x86;
/// Kind of [`Frame::Busy`].
const BUSY: u8 = 0x87;
/// Kind of [`Frame::Error`].
const ERROR: u8 = 0xff;

/// A frame of version 2, without its request ID, which [`Frame::encode`] takes and
/// [`Frame::decode`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Frame<'a> {
    /// `PING`: asks whether the guest agent is ready.
    Ping,
    /// `EXEC`: starts an execution, which the `EXEC` payload describes.
    Exec(&'a [u8]),
    /// `STOP`: shuts the VM down.
    Stop,
    /// `CANCEL`: ends the execution that the request ID names.
    Cancel,
    /// `MAPS`: hands the guest agent entries of the host mapping table, in a `MAPS` payload.
    Maps(&'a [u8]),
    /// `CAPTURE`: asks nvx-init to capture the VM, with the capture request's payload.
    Capture(&'a [u8]),
    /// `ACTIVATE`: asks nvx-init to activate a restored VM, with the activate request's payload.
    Activate(&'a [u8]),
    /// `STDIN`: bytes of the standard input of the execution that the request ID names.
    Stdin(&'a [u8]),
    /// `STDIN_END`: ends the standard input of the execution that the request ID names.
    StdinEnd,
    /// `READY`: acknowledges a request.
    Ready {
        /// The protocol version that the guest agent speaks.
        version: u8,
        /// What Ready reports about the activation, in answer to `ACTIVATE`; empty otherwise.
        activation: &'a [u8],
    },
    /// `STDOUT`: bytes of the execution's standard output.
    Stdout(&'a [u8]),
    /// `STDERR`: bytes of the execution's standard error.
    Stderr(&'a [u8]),
    /// `EXIT`: ends the execution.
    Exit {
        /// Its exit status, 128 plus the signal number for [`Outcome::Signal`].
        status: i32,
        /// How it ended.
        outcome: Outcome,
    },
    /// `STOPPED`: acknowledges `STOP`.
    Stopped,
    /// `STDIN_ACK`: acknowledges this many more bytes of the execution's standard input, which
    /// the guest agent wrote into it.
    StdinAck(u32),
    /// `BUSY`: refuses a request while an execution runs.
    Busy,
    /// `ERROR`: refuses a request.
    Error {
        /// The guest's error number.
        status: i32,
        /// The error category, such as `invalid-request`.
        category: &'a [u8],
    },
}

/// How an execution ended, which [`Frame::Exit`] reports. Version 2 caps no output, so no
/// execution ends at an output limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The workload exited; the status is its exit status.
    Exit,
    /// A signal ended the workload; the status is 128 plus the signal number.
    Signal,
    /// The workload's timeout elapsed, and the guest agent killed it.
    Timeout,
    /// A cancellation ended the workload.
    Cancelled,
    /// The guest agent could not supervise the workload.
    Failed,
}

impl Outcome {
    /// Returns the text that names this outcome in an `EXIT` payload.
    fn text(self) -> &'static [u8] {
        match self {
            Self::Exit => b"exit",
            Self::Signal => b"signal",
            Self::Timeout => b"timeout",
            Self::Cancelled => b"cancelled",
            Self::Failed => b"failed",
        }
    }

    /// Returns the outcome that `text` names, if any.
    fn parse(text: &[u8]) -> Option<Self> {
        [
            Self::Exit,
            Self::Signal,
            Self::Timeout,
            Self::Cancelled,
            Self::Failed,
        ]
        .into_iter()
        .find(|outcome| outcome.text() == text)
    }
}

impl<'a> Frame<'a> {
    /// Encodes this frame with `request_id`.
    ///
    /// It refuses what [`Frame::decode`] refuses: a zero request ID, an empty chunk of standard
    /// input, output or error, an acknowledgement of no bytes or of more than [`STDIN_WINDOW`], an
    /// empty error category, and a payload longer than [`MAX_PAYLOAD`].
    pub(crate) fn encode(&self, request_id: u64) -> Result<Vec<u8>, ProtocolError> {
        self.check(request_id)?;
        let ready;
        let ack;
        let (kind, status, head, data): (u8, i32, &[u8], &[u8]) = match *self {
            Self::Ping => (PING, 0, &[], &[]),
            Self::Exec(data) => (EXEC, 0, &[], data),
            Self::Stop => (STOP, 0, &[], &[]),
            Self::Cancel => (CANCEL, 0, &[], &[]),
            Self::Maps(data) => (MAPS, 0, &[], data),
            Self::Capture(data) => (CAPTURE, 0, &[], data),
            Self::Activate(data) => (ACTIVATE, 0, &[], data),
            Self::Stdin(data) => (STDIN, 0, &[], data),
            Self::StdinEnd => (STDIN_END, 0, &[], &[]),
            Self::Ready {
                version,
                activation,
            } => {
                ready = [version];
                (READY, 0, &ready, activation)
            }
            Self::Stdout(data) => (STDOUT, 0, &[], data),
            Self::Stderr(data) => (STDERR, 0, &[], data),
            Self::Exit { status, outcome } => (EXIT, status, &[], outcome.text()),
            Self::Stopped => (STOPPED, 0, &[], &[]),
            Self::StdinAck(count) => {
                ack = count.to_le_bytes();
                (STDIN_ACK, 0, &ack, &[])
            }
            Self::Busy => (BUSY, 0, &[], &[]),
            Self::Error { status, category } => (ERROR, status, &[], category),
        };
        let length = head.len() + data.len();
        if length > MAX_PAYLOAD {
            return Err(violation("an NVXC frame must fit one outer data record"));
        }
        let mut frame = Vec::with_capacity(HEADER_LEN + length);
        frame.extend_from_slice(&MAGIC);
        frame.extend_from_slice(&[VERSION, kind, 0, 0]);
        frame.extend_from_slice(&request_id.to_le_bytes());
        frame.extend_from_slice(&status.to_le_bytes());
        frame.extend_from_slice(&(length as u32).to_le_bytes());
        frame.extend_from_slice(head);
        frame.extend_from_slice(data);
        Ok(frame)
    }

    /// Decodes a frame, and returns its request ID with it.
    ///
    /// It refuses bytes that hold no whole frame, or more than one outer data record holds; a
    /// header with another magic or version, nonzero flags, or a payload length other than the
    /// payload's; an unknown kind; a nonzero status outside `EXIT` and `ERROR`; a payload in a
    /// kind that has none; a `READY` without a version; a `STDIN_ACK` without exactly a `u32`;
    /// an `EXIT` whose text names no [`Outcome`]; and what [`Frame::encode`] refuses.
    pub(crate) fn decode(frame: &'a [u8]) -> Result<(u64, Self), ProtocolError> {
        let (header, payload) = frame
            .split_at_checked(HEADER_LEN)
            .filter(|_| frame.len() <= OUTER_MAX_PAYLOAD)
            .ok_or_else(malformed)?;
        if header[..4] != MAGIC || header[4] != VERSION {
            return Err(violation(
                "the control peer sent a frame of another protocol than NVXC version 2",
            ));
        }
        let request_id = u64::from_le_bytes(array(&header[8..16]));
        let status = i32::from_le_bytes(array(&header[16..20]));
        let length = u32::from_le_bytes(array(&header[20..24]));
        if header[6..8] != [0, 0] || length as usize != payload.len() {
            return Err(malformed());
        }
        let empty = |variant| payload.is_empty().then_some(variant);
        let decoded = match header[5] {
            PING => empty(Self::Ping),
            EXEC => Some(Self::Exec(payload)),
            STOP => empty(Self::Stop),
            CANCEL => empty(Self::Cancel),
            MAPS => Some(Self::Maps(payload)),
            CAPTURE => Some(Self::Capture(payload)),
            ACTIVATE => Some(Self::Activate(payload)),
            STDIN => Some(Self::Stdin(payload)),
            STDIN_END => empty(Self::StdinEnd),
            READY => payload
                .split_first()
                .map(|(&version, activation)| Self::Ready {
                    version,
                    activation,
                }),
            STDOUT => Some(Self::Stdout(payload)),
            STDERR => Some(Self::Stderr(payload)),
            EXIT => Outcome::parse(payload).map(|outcome| Self::Exit { status, outcome }),
            STOPPED => empty(Self::Stopped),
            STDIN_ACK => payload
                .try_into()
                .ok()
                .map(|count| Self::StdinAck(u32::from_le_bytes(count))),
            BUSY => empty(Self::Busy),
            ERROR => Some(Self::Error {
                status,
                category: payload,
            }),
            _ => None,
        }
        .filter(|decoded| status == 0 || matches!(decoded, Self::Exit { .. } | Self::Error { .. }))
        .ok_or_else(malformed)?;
        decoded.check(request_id)?;
        Ok((request_id, decoded))
    }

    /// Checks what both directions refuse of a frame with `request_id`, other than its size: a
    /// zero request ID, an empty chunk, an acknowledgement of no bytes or of more than
    /// [`STDIN_WINDOW`], and an empty error category.
    fn check(&self, request_id: u64) -> Result<(), ProtocolError> {
        let valid = match *self {
            Self::Stdin(data) | Self::Stdout(data) | Self::Stderr(data) => !data.is_empty(),
            Self::StdinAck(count) => (1..=STDIN_WINDOW).contains(&count),
            Self::Error { category, .. } => !category.is_empty(),
            _ => true,
        };
        if request_id == 0 || !valid {
            return Err(malformed());
        }
        Ok(())
    }
}

/// Returns the error of a frame that version 2 does not allow.
fn malformed() -> ProtocolError {
    violation("the NVXC frame of version 2 is malformed")
}

#[cfg(test)]
mod tests;
