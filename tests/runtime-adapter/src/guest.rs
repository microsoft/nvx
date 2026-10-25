//! The adapter's guest agent: it serves the host program over the guest's end of the control
//! session between the host and the guest, with the adapter's own [framing](crate::frame), through
//! `nvx::guest::nvxs`, and on a tty through `nvx::guest::terminal`.
//!
//! [`session`] runs the session, and hands each of the host's messages to a handler, which
//! answers it and says whether serving goes on. [`serve`] runs it with the guest agent of a VM
//! without a layered root, and the [`worker`](crate::worker) with that of a VM with one.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use nvx::guest::nvxs::{GuestConnection, GuestConnectionError};
use nvx::guest::terminal::Terminal;
use nvx::protocol::OUTER_MIN_RECEIVE_WINDOW;
use nvx::protocol::nvxs::GuestEvent;

use crate::frame::{Decoder, FrameError, Message};

/// The receive window that the guest acknowledges each epoch with: the smallest that the session
/// allows, so that the rest of a longer message arrives only as the guest credits what it took.
pub const RECEIVE_WINDOW: u32 = OUTER_MIN_RECEIVE_WINDOW;

/// The problem with which the guest agent of a VM without a layered root answers a probe.
pub const NO_ROOT: &str = "the VM has no layered root to probe";

/// How serving ended, when nothing failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    /// The host asked the guest to stop, and the guest confirmed.
    Stopped,
    /// The broker closed the stream at a record boundary.
    Closed,
}

/// A failure that ended serving.
#[derive(Debug)]
#[non_exhaustive]
pub enum ServeError {
    /// The control session failed.
    Session(GuestConnectionError),
    /// The host sent a malformed message.
    Frame(FrameError),
    /// The host sent a message of a kind that the guest agent does not accept: one that only the
    /// guest sends, or one that the guest agent does not take in its state.
    Unexpected(u8),
    /// The host sent a message about an execution that breaks its rules: a body that does not
    /// decode, a second start of an execution, or a message that the execution engine refuses.
    Execution(String),
    /// The tty of the session could not be opened or put in raw mode.
    Terminal(io::Error),
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => error.fmt(formatter),
            Self::Frame(error) => error.fmt(formatter),
            Self::Unexpected(kind) => {
                write!(
                    formatter,
                    "the host sent a message of kind {kind}, which the guest agent does not accept"
                )
            }
            Self::Execution(problem) => write!(
                formatter,
                "the host sent a message about an execution that breaks its rules: {problem}"
            ),
            Self::Terminal(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::Frame(error) => Some(error),
            Self::Unexpected(_) | Self::Execution(_) => None,
            Self::Terminal(error) => Some(error),
        }
    }
}

impl From<GuestConnectionError> for ServeError {
    fn from(error: GuestConnectionError) -> Self {
        Self::Session(error)
    }
}

impl From<FrameError> for ServeError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

/// What the guest agent does once it handled a message from the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step<T> {
    /// It sends these messages, in order, and serves on.
    Continue(Vec<Message>),
    /// It sends these messages, in order, then ends serving with the value, without reading
    /// further.
    End(Vec<Message>, T),
}

/// How a [`session`] ended, when nothing failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended<T> {
    /// The handler ended it, with this value.
    Handled(T),
    /// The broker closed the stream at a record boundary.
    Closed,
}

/// Runs the guest's end of the control session on `stream`, and hands each message of the host
/// to `handle`, until `handle` ends serving or the broker closes the stream.
///
/// The guest attaches, then:
///
/// - It acknowledges each reset of the session with [`RECEIVE_WINDOW`], once it dropped the
///   earlier epoch's partial message: a new epoch starts with a new message. The handler keeps
///   its state across epochs.
/// - It credits each payload back once it holds the payload, so that a message longer than the
///   window arrives whole.
/// - It sends the messages with which `handle` answers each message, in order, and returns once
///   `handle` ends serving.
///
/// A failure of the session, a malformed message, and an error of `handle`, such as a message
/// that it does not accept, end serving with that error. A reply that the framing refuses fails
/// with [`ServeError::Frame`], before it is sent.
pub fn session<S: Read + Write, T>(
    stream: S,
    mut handle: impl FnMut(Message) -> Result<Step<T>, ServeError>,
) -> Result<Ended<T>, ServeError> {
    let mut connection = GuestConnection::new(stream);
    connection.attach()?;
    let mut decoder = Decoder::new();
    loop {
        let payload = match connection.next_event() {
            Ok(GuestEvent::Reset(_)) => {
                decoder.clear();
                connection.acknowledge(RECEIVE_WINDOW)?;
                continue;
            }
            Ok(GuestEvent::Data(payload)) => payload,
            Err(GuestConnectionError::Closed) => return Ok(Ended::Closed),
            Err(error) => return Err(error.into()),
        };
        // A DATA payload has at most `OUTER_MAX_PAYLOAD` bytes, so its length fits a `u32`.
        connection.credit(payload.len() as u32)?;
        let mut rest = payload.as_slice();
        while !rest.is_empty() {
            let (consumed, message) = decoder.push(rest)?;
            rest = &rest[consumed..];
            let Some(message) = message else {
                continue;
            };
            let (replies, ended) = match handle(message)? {
                Step::Continue(replies) => (replies, None),
                Step::End(replies, value) => (replies, Some(value)),
            };
            for reply in replies {
                connection.send(&reply.encode()?)?;
            }
            if let Some(value) = ended {
                return Ok(Ended::Handled(value));
            }
        }
    }
}

/// Serves the host program on `stream`, the guest's end of the control session, in a VM without
/// a layered root, until the host stops the guest or the broker closes the stream.
///
/// It runs the [`session`], in which it answers [`Message::Ping`] with [`Message::Pong`], sends
/// each [`Message::Echo`] back, answers each [`Message::Probe`] with a [`Message::ProbeFailed`]
/// that says [`NO_ROOT`], and answers [`Message::Stop`] with [`Message::Stopped`], then returns
/// without reading further.
///
/// A failure of the session, a malformed message, and a message of another kind, which only the
/// guest sends, or which configures a layered root or runs an execution, end serving with an
/// error.
pub fn serve<S: Read + Write>(stream: S) -> Result<Served, ServeError> {
    let ended = session(stream, |message| {
        Ok(match message {
            Message::Ping => Step::Continue(vec![Message::Pong]),
            Message::Echo(body) => Step::Continue(vec![Message::Echo(body)]),
            Message::Probe(_) => Step::Continue(vec![Message::ProbeFailed(NO_ROOT.into())]),
            Message::Stop => Step::End(vec![Message::Stopped], ()),
            message => return Err(ServeError::Unexpected(message.kind())),
        })
    })?;
    Ok(match ended {
        Ended::Handled(()) => Served::Stopped,
        Ended::Closed => Served::Closed,
    })
}

/// Opens the tty at `path`, the guest's end of the control session, and returns what `run`
/// returns with it, as NVX's managed agent serves a VM's control tty.
///
/// It opens the tty with NVX's [`Terminal`], as NVX's managed agent does: waiting while the kernel
/// sets the tty up during boot, and in raw mode, so that the tty passes the session's bytes as they
/// are. Once `run` succeeds, it waits until the tty has sent what `run` wrote, such as the
/// [`Message::Stopped`] that confirms a stop.
///
/// It fails with [`ServeError::Terminal`] if the tty cannot be opened or put in raw mode, and
/// otherwise as `run` does.
pub fn on_terminal<T>(
    path: &Path,
    run: impl FnOnce(&mut Terminal) -> Result<T, ServeError>,
) -> Result<T, ServeError> {
    on_terminal_within(path, |terminal| run(terminal).map(|ran| (ran, None)))
}

/// Opens the tty at `path` as [`on_terminal`] does, and returns what `run` returns with it, but
/// waits until the tty has sent what `run` wrote for at most the time that `run` returns beside
/// its result, if it returns one: a thread of its own then waits for the tty, and holds it until
/// the tty has sent everything, or the process ends, so that a tty that sends nothing does not
/// keep the caller from ending.
///
/// It fails as [`on_terminal`] does.
pub fn on_terminal_within<T>(
    path: &Path,
    run: impl FnOnce(&mut Terminal) -> Result<(T, Option<Duration>), ServeError>,
) -> Result<T, ServeError> {
    let failed = |action: String| {
        move |error: io::Error| {
            ServeError::Terminal(io::Error::new(error.kind(), format!("{action}: {error}")))
        }
    };
    let mut terminal =
        Terminal::open(path).map_err(failed(format!("cannot open {}", path.display())))?;
    terminal
        .configure_raw()
        .map_err(failed(format!("cannot put {} in raw mode", path.display())))?;
    let (ran, limit) = run(&mut terminal)?;
    // As NVX's managed agent does, a drain that fails is no failure of serving, which ended.
    match limit {
        None => {
            let _ = terminal.drain();
        }
        Some(limit) => {
            let (drained, done) = mpsc::channel();
            let drain = move || {
                let _ = terminal.drain();
                let _ = drained.send(());
            };
            // Without a thread for it, the tty is not drained, which could take longer.
            if thread::Builder::new().spawn(drain).is_ok() {
                let _ = done.recv_timeout(limit);
            }
        }
    }
    Ok(ran)
}

/// Serves the host program on the tty at `path` as [`serve`] does, through [`on_terminal`], in
/// place of NVX's managed agent on a VM's control tty.
///
/// It fails as [`on_terminal`] and [`serve`] do.
pub fn serve_terminal(path: &Path) -> Result<Served, ServeError> {
    on_terminal(path, |terminal| serve(terminal))
}
