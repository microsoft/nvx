//! The worker's end of the control session, which never blocks, so that one loop serves the
//! session and every execution: it reads the broker's records as they arrive, decodes the host's
//! messages from their DATA, and queues the worker's records until the stream takes them.
//!
//! It runs the guest's role of the session as the guest agent's blocking `session` does: it
//! attaches, acknowledges each reset with the guest agent's receive window once it dropped the
//! earlier epoch's partial message, and credits each DATA payload back, once it decoded the
//! payload's messages. A reset also drops the records that the stream has not begun to take,
//! which belong to the earlier epoch, and ends the one that it began, so that the stream stays
//! whole. In a clone, the connection resumes the session that the template's VM attached before
//! its capture, and whose first epoch it acknowledged, which the snapshot holds, rather than
//! attaching: the restore's broker resets it, which the connection acknowledges as any reset.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{self, FcntlArg, OFlag};
use nix::poll::{self, PollFd, PollFlags, PollTimeout};
use nvx::guest::nvxs::GuestConnectionError;
use nvx::protocol::nvxs::{GuestEvent, GuestSession, SessionError};

use crate::frame::{Decoder, Message};
use crate::guest::{RECEIVE_WINDOW, ServeError};

/// Most bytes that one read from the stream takes.
const READ_CHUNK: usize = 16 * 1024;

/// The most bytes of records that wait for the stream before the worker's executions take no more
/// output from their programs, until the stream has taken enough of them: a full stream holds
/// back the executions' output, and nothing else.
pub(super) const BACKLOG: usize = 1024 * 1024;

/// The most bytes of records that wait for the stream before the worker stops taking the host's
/// messages too, until the stream has taken enough of them, as a worker that blocked on its
/// writes would stop. Beyond [`BACKLOG`], only the replies to the host's messages and the
/// executions' messages other than output add records, so only a host that sends messages
/// without reading the replies gets here, and it holds the worker's memory to this, and what
/// one message and one step of each execution add.
pub(super) const OVERLOAD: usize = 4 * 1024 * 1024;

/// What the connection received.
pub(super) enum Received {
    /// A message of the host.
    Message(Message),
    /// The broker reset the session, which the connection acknowledged: the worker abandons the
    /// earlier epoch's work.
    Reset,
    /// The broker closed the stream at a record boundary.
    Closed,
}

/// The worker's end of the control session over its stream, which it made nonblocking.
pub(super) struct Connection<S> {
    /// The stream to the broker.
    stream: S,
    /// The guest's role of the session.
    session: GuestSession,
    /// The bytes read from the stream: those from `start` to `end` wait for the session.
    input: Box<[u8]>,
    /// Where the bytes that wait for the session start.
    start: usize,
    /// Where they end.
    end: usize,
    /// The decoder of the host's messages.
    decoder: Decoder,
    /// The DATA payload that the connection decodes.
    payload: Vec<u8>,
    /// How many bytes of the payload it decoded.
    position: usize,
    /// The bytes of the payloads that it decoded and has not credited.
    uncredited: u32,
    /// The records for the stream, in order.
    output: VecDeque<Vec<u8>>,
    /// The bytes of the first record that the stream took.
    written: usize,
    /// The bytes of records that wait for the stream.
    queued: usize,
}

impl<S: Read + Write + AsFd> Connection<S> {
    /// Makes `stream` nonblocking, and returns the connection over it, with the guest's
    /// attachment queued.
    pub(super) fn new(stream: S) -> Result<Self, ServeError> {
        let mut connection = Self::over(stream, GuestSession::new())?;
        connection.record(GuestSession::attach)?;
        Ok(connection)
    }

    /// Makes `stream` nonblocking, and returns the connection over it, with `session`, the guest's
    /// role of a session that the guest attached already, on the same stream: that of a template's
    /// VM, which attached before its capture, and acknowledged its broker's reset, in a clone that
    /// a restore of the snapshot started, whose broker resets the session that the snapshot holds
    /// again, with another instance. The connection queues no attachment.
    ///
    /// A session that has not attached takes no record, so that the connection fails as the
    /// guest agent's blocking `session` does once the broker sends one.
    #[cfg(target_arch = "x86_64")]
    pub(super) fn resume(stream: S, session: GuestSession) -> Result<Self, ServeError> {
        Self::over(stream, session)
    }

    /// Makes `stream` nonblocking, and returns the connection over it, with `session`, the guest's
    /// role of the session, and nothing queued.
    fn over(stream: S, session: GuestSession) -> Result<Self, ServeError> {
        let flags = fcntl::fcntl(&stream, FcntlArg::F_GETFL).map_err(failed)?;
        let flags = OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK;
        fcntl::fcntl(&stream, FcntlArg::F_SETFL(flags)).map_err(failed)?;
        Ok(Self {
            stream,
            session,
            input: vec![0; READ_CHUNK].into_boxed_slice(),
            start: 0,
            end: 0,
            decoder: Decoder::new(),
            payload: Vec::new(),
            position: 0,
            uncredited: 0,
            output: VecDeque::new(),
            written: 0,
            queued: 0,
        })
    }

    /// Returns what the connection received next, or `None` until the stream has more.
    ///
    /// A failure of the stream, a record that the session refuses, and a malformed message fail
    /// as the guest agent's blocking `session` does.
    pub(super) fn receive(&mut self) -> Result<Option<Received>, ServeError> {
        loop {
            if self.position < self.payload.len() {
                let (consumed, message) = self.decoder.push(&self.payload[self.position..])?;
                self.position += consumed;
                if self.position == self.payload.len() {
                    // A DATA payload has at most `OUTER_MAX_PAYLOAD` bytes, so its length fits
                    // a `u32`, as do those of a receive window's payloads together.
                    self.uncredited += self.payload.len() as u32;
                    self.payload = Vec::new();
                    self.position = 0;
                }
                match message {
                    Some(message) => return Ok(Some(Received::Message(message))),
                    None => continue,
                }
            }
            if self.start == self.end {
                match self.stream.read(&mut self.input) {
                    Ok(0) => {
                        self.session.disconnect().map_err(protocol)?;
                        return Ok(Some(Received::Closed));
                    }
                    Ok(count) => (self.start, self.end) = (0, count.min(self.input.len())),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(stream(error)),
                }
            }
            let (consumed, event) = self
                .session
                .receive(&self.input[self.start..self.end])
                .map_err(protocol)?;
            self.start += consumed;
            match event {
                None => {}
                Some(GuestEvent::Data(payload)) => {
                    self.payload = payload;
                    self.position = 0;
                }
                Some(GuestEvent::Reset(_)) => {
                    self.reset()?;
                    return Ok(Some(Received::Reset));
                }
            }
        }
    }

    /// Queues `message` for the host, in as many DATA records as it takes. A message that the
    /// framing refuses fails with [`ServeError::Frame`], and queues nothing.
    pub(super) fn send(&mut self, message: &Message) -> Result<(), ServeError> {
        let bytes = message.encode()?;
        let mut rest = bytes.as_slice();
        while !rest.is_empty() {
            let mut record = Vec::new();
            let taken = self.session.send(rest, &mut record).map_err(protocol)?;
            rest = &rest[taken..];
            self.push(record);
        }
        Ok(())
    }

    /// Credits the payloads that the connection decoded since it last credited, if any.
    pub(super) fn credit(&mut self) -> Result<(), ServeError> {
        if self.uncredited == 0 {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.uncredited);
        self.record(|session, output| session.credit(bytes, output))
    }

    /// Writes the queued records while the stream takes them.
    pub(super) fn flush(&mut self) -> Result<(), ServeError> {
        while let Some(record) = self.output.front() {
            match self.stream.write(&record[self.written..]) {
                Ok(0) => return Err(stream(io::ErrorKind::WriteZero.into())),
                Ok(count) => {
                    self.written += count;
                    self.queued -= count;
                    if self.written == record.len() {
                        self.output.pop_front();
                        self.written = 0;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(stream(error)),
            }
        }
        Ok(())
    }

    /// Writes every queued record, waiting while the stream takes none, until `deadline` passes,
    /// if there is one, after which it leaves the rest queued.
    pub(super) fn finish(&mut self, deadline: Option<Instant>) -> Result<(), ServeError> {
        loop {
            self.flush()?;
            if self.output.is_empty() {
                return Ok(());
            }
            let timeout = match deadline {
                None => PollTimeout::NONE,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(());
                    }
                    // Poll counts whole milliseconds, so a shorter wait would return before the
                    // deadline.
                    PollTimeout::try_from(left.max(Duration::from_millis(1)))
                        .unwrap_or(PollTimeout::MAX)
                }
            };
            let mut descriptors = [PollFd::new(self.stream.as_fd(), PollFlags::POLLOUT)];
            match poll::poll(&mut descriptors, timeout) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(error) => return Err(failed(error)),
            }
        }
    }

    /// Returns whether more than [`BACKLOG`] bytes of records wait for the stream, so that the
    /// executions take no more output.
    pub(super) fn is_backlogged(&self) -> bool {
        self.queued > BACKLOG
    }

    /// Returns whether more than [`OVERLOAD`] bytes of records wait for the stream, so that the
    /// worker takes none of the host's messages.
    pub(super) fn is_overloaded(&self) -> bool {
        self.queued > OVERLOAD
    }

    /// Returns whether records wait for the stream.
    pub(super) fn wants_write(&self) -> bool {
        !self.output.is_empty()
    }

    /// Returns whether the connection holds bytes that it read and has not returned, so that
    /// [`receive`](Self::receive) may return more without the stream.
    pub(super) fn has_input(&self) -> bool {
        self.start < self.end || self.position < self.payload.len()
    }

    /// Returns the stream's descriptor, which the worker polls.
    pub(super) fn descriptor(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }

    /// Abandons the earlier epoch: drops its partial message, its uncredited payloads, and the
    /// records that the stream has not begun to take, and queues the acknowledgement.
    fn reset(&mut self) -> Result<(), ServeError> {
        self.decoder.clear();
        self.uncredited = 0;
        self.output.truncate(usize::from(self.written > 0));
        self.queued = self
            .output
            .front()
            .map_or(0, |record| record.len() - self.written);
        self.record(|session, output| session.acknowledge(RECEIVE_WINDOW, output))
    }

    /// Queues one record that `encode` appends through the session.
    fn record(
        &mut self,
        encode: impl FnOnce(&mut GuestSession, &mut Vec<u8>) -> Result<(), SessionError>,
    ) -> Result<(), ServeError> {
        let mut record = Vec::new();
        encode(&mut self.session, &mut record).map_err(protocol)?;
        self.push(record);
        Ok(())
    }

    /// Queues `record`.
    fn push(&mut self, record: Vec<u8>) {
        self.queued += record.len();
        self.output.push_back(record);
    }
}

/// Returns the failure of a record that the session refuses.
fn protocol(error: SessionError) -> ServeError {
    ServeError::Session(GuestConnectionError::Protocol(error))
}

/// Returns the failure of the stream.
fn stream(error: io::Error) -> ServeError {
    ServeError::Session(GuestConnectionError::Io(error))
}

/// Returns the failure of a system call on the stream.
fn failed(error: Errno) -> ServeError {
    stream(error.into())
}
