//! The guest's I/O driver of an NVXS session: [`GuestConnection`] runs the guest's role of
//! [`protocol::nvxs`](crate::protocol::nvxs) over a byte stream, such as the control console that a
//! guest agent opened. The public [`control::Connection`](super::control::Connection) and NVX's
//! managed agent run their sessions through it.
//!
//! [`attach`](GuestConnection::attach) sends the guest's attachment, and
//! [`next_event`](GuestConnection::next_event) blocks until the broker's next RESET or DATA
//! record completes. After a RESET, the guest abandons the work of the earlier epoch, then calls
//! [`acknowledge`](GuestConnection::acknowledge). It returns consumed DATA bytes to its receive
//! window with [`credit`](GuestConnection::credit), and [`send`](GuestConnection::send) writes
//! application bytes in as many DATA records as they take.
//!
//! The connection never reads past the record in progress, so a caller that polls the stream for
//! readiness between calls, through [`get_ref`](GuestConnection::get_ref), sees every record that
//! the connection has not returned. It holds at most one partial record.
//!
//! A failure of the stream or of a received record ends the connection, and every later call
//! fails with [`GuestConnectionError::Ended`]. So does a send that fails after it wrote anything,
//! because the rest of a record or of a message cannot follow.

use std::fmt;
use std::io::{self, Read, Write};

use crate::protocol::nvxs::{GuestEvent, GuestSession, GuestState, SessionError};

/// Most bytes that one read from the stream takes.
const READ_CHUNK: usize = 16 * 1024;

/// Failure of a [`GuestConnection`].
#[derive(Debug)]
pub(crate) enum GuestConnectionError {
    /// The broker closed the stream at a record boundary.
    Closed,
    /// The broker violated the protocol, or the stream ended inside a record, which ended the
    /// connection; or the session refused an operation that its state does not allow, which
    /// changed nothing.
    Protocol(SessionError),
    /// The stream failed.
    Io(io::Error),
    /// An earlier failure ended the connection.
    Ended,
}

impl fmt::Display for GuestConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("the broker closed the NVXS session"),
            Self::Protocol(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "NVXS I/O failed: {error}"),
            Self::Ended => formatter.write_str("an earlier failure ended the NVXS connection"),
        }
    }
}

impl std::error::Error for GuestConnectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protocol(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// The guest's end of an NVXS session over a byte stream.
pub(crate) struct GuestConnection<S> {
    /// Stream connected to the broker's guest leg.
    stream: S,
    /// The guest's role of the session.
    session: GuestSession,
    /// Bytes read from the stream, never past the record in progress.
    input: Box<[u8]>,
    /// Records encoded for the stream.
    output: Vec<u8>,
    /// Whether a failure ended the connection.
    ended: bool,
}

impl<S: Read + Write> GuestConnection<S> {
    /// Creates an unattached connection over `stream`.
    pub(crate) fn new(stream: S) -> Self {
        Self {
            stream,
            session: GuestSession::new(),
            input: vec![0; READ_CHUNK].into_boxed_slice(),
            output: Vec::new(),
            ended: false,
        }
    }

    /// Returns the guest's role of the session, for its state, identity, and receive window.
    pub(crate) fn session(&self) -> &GuestSession {
        &self.session
    }

    /// Returns the stream, for example to poll it for readiness. Reading or writing it directly
    /// would corrupt the session.
    pub(crate) fn get_ref(&self) -> &S {
        &self.stream
    }

    /// Sends the guest's attachment, which the broker answers with a RESET.
    pub(crate) fn attach(&mut self) -> Result<(), GuestConnectionError> {
        self.send_record(GuestSession::attach)
    }

    /// Blocks until the broker's next record completes, and returns its event.
    pub(crate) fn next_event(&mut self) -> Result<GuestEvent, GuestConnectionError> {
        self.check()?;
        if !matches!(
            self.session.state(),
            GuestState::Attaching | GuestState::Resetting | GuestState::Active
        ) {
            return Err(GuestConnectionError::Protocol(SessionError::InvalidState));
        }
        loop {
            let wanted = self.session.needed().min(self.input.len());
            let count = match self.stream.read(&mut self.input[..wanted]) {
                Ok(0) => {
                    let error = match self.session.disconnect() {
                        Ok(()) => GuestConnectionError::Closed,
                        Err(error) => GuestConnectionError::Protocol(error),
                    };
                    return Err(self.end(error));
                }
                Ok(count) => count.min(wanted),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(self.end(GuestConnectionError::Io(error))),
            };
            // A read never passes the record in progress, so the session consumes every byte of
            // it, and a record completes only with its last byte.
            let (consumed, event) = self
                .session
                .receive(&self.input[..count])
                .map_err(|error| self.end(GuestConnectionError::Protocol(error)))?;
            debug_assert_eq!(consumed, count);
            if let Some(event) = event {
                return Ok(event);
            }
        }
    }

    /// Acknowledges the session's reset with a receive window of `window` bytes, from
    /// [`OUTER_MIN_RECEIVE_WINDOW`](crate::protocol::OUTER_MIN_RECEIVE_WINDOW) through
    /// [`OUTER_MAX_RECEIVE_WINDOW`](crate::protocol::OUTER_MAX_RECEIVE_WINDOW), once the guest
    /// abandoned the work of the earlier epoch.
    pub(crate) fn acknowledge(&mut self, window: u32) -> Result<(), GuestConnectionError> {
        self.send_record(|session, output| session.acknowledge(window, output))
    }

    /// Sends `data` to the host in as many DATA records as it takes, of at most
    /// [`OUTER_MAX_PAYLOAD`](crate::protocol::OUTER_MAX_PAYLOAD) bytes each.
    ///
    /// Data that fits one record travels in one, so an application protocol that needs one frame
    /// per record sends frames that fit. Empty data sends nothing.
    pub(crate) fn send(&mut self, data: &[u8]) -> Result<(), GuestConnectionError> {
        self.check()?;
        let mut rest = data;
        let mut wrote = false;
        while !rest.is_empty() {
            self.output.clear();
            let taken = match self.session.send(rest, &mut self.output) {
                Ok(taken) => taken,
                Err(error) if wrote => return Err(self.end(GuestConnectionError::Protocol(error))),
                Err(error) => return Err(GuestConnectionError::Protocol(error)),
            };
            self.write()?;
            wrote = true;
            rest = &rest[taken..];
        }
        Ok(())
    }

    /// Returns `bytes` consumed DATA bytes to the receive window, from 1 through the session's
    /// [`outstanding`](GuestSession::outstanding) bytes.
    pub(crate) fn credit(&mut self, bytes: u32) -> Result<(), GuestConnectionError> {
        self.send_record(|session, output| session.credit(bytes, output))
    }

    /// Encodes one record with `encode`, which appends it to the output through the session, and
    /// writes it, unless an earlier failure ended the connection. A record that the session
    /// refuses changes nothing.
    fn send_record(
        &mut self,
        encode: impl FnOnce(&mut GuestSession, &mut Vec<u8>) -> Result<(), SessionError>,
    ) -> Result<(), GuestConnectionError> {
        self.check()?;
        self.output.clear();
        encode(&mut self.session, &mut self.output).map_err(GuestConnectionError::Protocol)?;
        self.write()
    }

    /// Writes the encoded records, ending the connection if that fails.
    fn write(&mut self) -> Result<(), GuestConnectionError> {
        let written = self
            .stream
            .write_all(&self.output)
            .and_then(|()| self.stream.flush());
        written.map_err(|error| self.end(GuestConnectionError::Io(error)))
    }

    /// Fails if an earlier failure ended the connection.
    fn check(&self) -> Result<(), GuestConnectionError> {
        if self.ended {
            Err(GuestConnectionError::Ended)
        } else {
            Ok(())
        }
    }

    /// Ends the connection with `error`, and returns it.
    fn end(&mut self, error: GuestConnectionError) -> GuestConnectionError {
        self.ended = true;
        error
    }
}

#[cfg(test)]
pub(crate) mod tests;
