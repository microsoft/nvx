//! The guest's end of its VM's control connection: an ordered stream of bytes in each direction
//! between the guest and the host's program, over which both speak a protocol of their own.
//!
//! [`Connection::attach`] attaches the guest over a byte stream, such as the VM's control console,
//! which the guest opened in raw mode, with [`Terminal`](super::terminal::Terminal) or otherwise.
//! The VM then resets the session, which [`Connection::receive`] reports as [`Event::Reset`]: once
//! the guest attaches, again whenever a program's connection to the VM ends, and after a restore.
//! Each reset starts the session over. The guest abandons the earlier session's work, then calls
//! [`Connection::acknowledge`], after which a program's connection to the VM opens, and the
//! session carries bytes: [`Event::Data`] returns the program's, and [`Connection::send`] sends
//! the guest's.
//!
//! The connection keeps the session's framing, its checks, and its flow control to itself, and
//! has no protocol of its own: the guest and the host's program choose theirs, of which NVX's own
//! control protocol is one, and the connection needs none of NVX's.
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//!
//! use nvx::guest::control::{Connection, ConnectionError, Event};
//!
//! /// Serves the session on `console`, a control console that the caller opened for reading and
//! /// writing, in raw mode, and echoes what the host's program sends.
//! fn serve(console: File) -> Result<(), ConnectionError> {
//!     let mut connection = Connection::attach(console)?;
//!     loop {
//!         match connection.receive()? {
//!             // Abandon the earlier session's work here, then take the new session's bytes.
//!             Event::Reset => connection.acknowledge()?,
//!             // The program's protocol frames its own messages across what `receive` returns.
//!             Event::Data(bytes) => connection.send(&bytes)?,
//!         }
//!     }
//! }
//! ```

use std::error::Error as StdError;
use std::fmt;
use std::io::{Read, Write};

use super::nvxs::{GuestConnection, GuestConnectionError};
use crate::protocol::OUTER_RECEIVE_WINDOW;
use crate::protocol::nvxs::{GuestEvent, GuestState, SessionError};

/// What [`Connection::receive`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The VM reset the session, which starts over: the guest abandons the work of the earlier
    /// session, if any, then calls [`Connection::acknowledge`].
    Reset,
    /// The next bytes that the host's program sent, at least one, in order.
    ///
    /// Where the connection splits the program's bytes is unspecified, and need not match the
    /// program's sends, so the program's protocol frames its own messages.
    Data(Vec<u8>),
}

/// The guest's end of its VM's control connection over a byte stream `S`, such as the VM's
/// control console: an ordered stream of bytes in each direction between the guest and the host's
/// program.
///
/// [`attach`](Self::attach) attaches the guest, and [`receive`](Self::receive) returns the
/// session's resets and the program's bytes. After each [`Event::Reset`], the guest abandons the
/// earlier session's work, then [acknowledges](Self::acknowledge) the reset, before which the
/// session carries no bytes either way. [`send`](Self::send) sends bytes after those of earlier
/// sends.
///
/// The calls block on the stream, which blocks too: a read or a write that would block fails, and
/// ends the connection. Between calls, the connection holds nothing that it has not returned,
/// since it never reads past what it returns next: whatever the VM sent that the guest has not
/// received keeps the stream readable. So a guest that waits with a deadline, or for more than the
/// connection, polls the stream for readability through [`get_ref`](Self::get_ref) before it
/// receives. A receive then waits at most for the rest of what the VM began to send, and, when it
/// returns the program's bytes, for the VM to take the credit that it writes for them.
///
/// The VM holds the program's bytes back while the guest has not received enough of the earlier
/// ones: what the guest received, it holds itself.
///
/// A failure of the stream, or bytes that break the session's protocol, end the connection, after
/// which every call fails with [`ConnectionErrorKind::Ended`]. So does a send that fails after it
/// wrote anything, since the rest of its bytes cannot follow. A call out of the session's order
/// fails with [`ConnectionErrorKind::OutOfOrder`], and changes nothing.
pub struct Connection<S> {
    /// The guest's NVXS connection to the VM's broker.
    inner: GuestConnection<S>,
}

impl<S: Read + Write> Connection<S> {
    /// Attaches the guest to its VM's control session over `stream`, which the VM answers with a
    /// reset.
    pub fn attach(stream: S) -> Result<Self, ConnectionError> {
        let mut inner = GuestConnection::new(stream);
        inner.attach()?;
        Ok(Self { inner })
    }

    /// Blocks until the VM's next reset, or the next bytes of the host's program, arrive.
    ///
    /// Fails with [`ConnectionErrorKind::OutOfOrder`] while the guest has not acknowledged a
    /// reset, since the VM sends nothing more until it does.
    pub fn receive(&mut self) -> Result<Event, ConnectionError> {
        if self.inner.session().state() == GuestState::Resetting {
            return Err(GuestConnectionError::Protocol(SessionError::InvalidState).into());
        }
        match self.inner.next_event()? {
            GuestEvent::Reset(_) => Ok(Event::Reset),
            GuestEvent::Data(bytes) => {
                // A DATA payload has 1 through `OUTER_MAX_PAYLOAD` bytes, which the session
                // counted outstanding, so its length fits a `u32` and the credit is valid.
                self.inner.credit(bytes.len() as u32)?;
                Ok(Event::Data(bytes))
            }
        }
    }

    /// Acknowledges the session's reset, once the guest abandoned the earlier session's work.
    ///
    /// Fails with [`ConnectionErrorKind::OutOfOrder`] unless a reset waits for it.
    pub fn acknowledge(&mut self) -> Result<(), ConnectionError> {
        Ok(self.inner.acknowledge(OUTER_RECEIVE_WINDOW)?)
    }

    /// Sends `data` to the host's program, after the bytes of earlier sends. Empty data sends
    /// nothing.
    ///
    /// Fails with [`ConnectionErrorKind::OutOfOrder`] while the guest has not acknowledged the
    /// session's latest reset.
    pub fn send(&mut self, data: &[u8]) -> Result<(), ConnectionError> {
        Ok(self.inner.send(data)?)
    }

    /// Returns the stream, to poll it for readiness. Reading, writing, or changing it directly
    /// would corrupt the session.
    pub fn get_ref(&self) -> &S {
        self.inner.get_ref()
    }
}

impl<S> fmt::Debug for Connection<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Connection").finish_non_exhaustive()
    }
}

/// Classification of a [`ConnectionError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConnectionErrorKind {
    /// The VM closed the stream, or the stream ended partway through what the VM sent.
    Closed,
    /// The VM sent what the session's protocol does not allow.
    Protocol,
    /// The call came out of the session's order: a receive or a send while the guest has not
    /// acknowledged a reset, a send before the first reset, or an acknowledgement without a reset
    /// to acknowledge. It changed nothing, and the connection stays usable.
    OutOfOrder,
    /// The stream failed.
    Io,
    /// An earlier failure ended the connection.
    Ended,
}

/// Failure of a [`Connection`]: its [`ConnectionErrorKind`], a description, and an optional
/// cause.
#[derive(Debug)]
pub struct ConnectionError {
    /// Classification of the failure.
    kind: ConnectionErrorKind,
    /// Complete, user-facing description without a trailing period.
    message: String,
    /// Underlying error, when retaining it aids diagnostics.
    source: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl ConnectionError {
    /// Returns the classification of this failure.
    pub fn kind(&self) -> ConnectionErrorKind {
        self.kind
    }
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for ConnectionError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

impl From<GuestConnectionError> for ConnectionError {
    fn from(error: GuestConnectionError) -> Self {
        let message = error.to_string();
        let (kind, source): (_, Option<Box<dyn StdError + Send + Sync>>) = match error {
            GuestConnectionError::Closed => (ConnectionErrorKind::Closed, None),
            GuestConnectionError::Protocol(SessionError::InvalidState) => {
                (ConnectionErrorKind::OutOfOrder, None)
            }
            GuestConnectionError::Protocol(error @ SessionError::Truncated { .. }) => {
                (ConnectionErrorKind::Closed, Some(Box::new(error)))
            }
            GuestConnectionError::Protocol(error) => {
                (ConnectionErrorKind::Protocol, Some(Box::new(error)))
            }
            GuestConnectionError::Io(error) => (ConnectionErrorKind::Io, Some(Box::new(error))),
            GuestConnectionError::Ended => (ConnectionErrorKind::Ended, None),
        };
        Self {
            kind,
            message,
            source,
        }
    }
}

#[cfg(test)]
mod tests;
