//! A VM's authenticated control connection, its failures, and the VM's boot console.

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use crate::host::process::ProcessRef;
use crate::host::transport::{self, HostConnection, HostConnectionError, Transport};
use crate::host::vm_directory::VmOutputs;
use crate::protocol::CAPABILITY_LEN;
use crate::protocol::nvxs::SessionError;
use crate::{Error, ErrorKind, Result};

/// An authenticated control connection to a VM's guest: an ordered stream of bytes in each
/// direction, over which the program speaks a protocol of its own.
///
/// [`Launched::connect`](super::Launched::connect) and [`attach`](super::attach) open one, once
/// the VM's own process serves its control endpoint, the VM accepted the capability, and the
/// guest acknowledged the session. That the guest acknowledged the session tells nothing of
/// whether its application is ready: the program's protocol tells that.
///
/// [`send`](Self::send) sends bytes after those of earlier sends, and [`receive`](Self::receive)
/// returns the next bytes that the guest sent, in order. Where `receive` splits the stream is
/// unspecified, and need not match the guest's writes, so the program's protocol frames its own
/// messages.
///
/// Each call takes a deadline, or `None` to wait without one. A receive that times out keeps what
/// arrived, and a send whose deadline passed before it began sends nothing: the connection stays
/// usable after either. A receive whose deadline has passed waits for nothing: it returns bytes
/// that have arrived, or fails with [`ConnectionErrorKind::TimedOut`]. Any other failure,
/// including a send that times out while it writes, ends the connection, after which every call
/// fails with [`ConnectionErrorKind::Ended`]. [`ConnectionError::is_lost`] tells a session that
/// the VM closed or reset, after which the program may connect again.
///
/// A connection is [`Send`], so a thread of the program's may block on it. On Linux, it
/// implements `std::os::fd::AsFd` for a program with an event loop of its own: the descriptor
/// polls readable once bytes arrive that the connection has not read. The connection reads ahead,
/// so it may hold bytes that the descriptor no longer shows, from the moment it opens and after
/// any receive. Before each wait for the descriptor, the program therefore receives with a
/// deadline that has passed until that fails with [`ConnectionErrorKind::TimedOut`]. The program
/// never reads, writes, or changes the descriptor itself. Windows has no such descriptor, and a
/// program there runs each connection on a thread.
///
/// # Example
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use std::time::{Duration, Instant};
///
/// use nvx::host::vm;
/// # let prepared: vm::Prepared = unimplemented!();
///
/// let launched = vm::spawn(prepared)?;
/// let deadline = Instant::now() + Duration::from_secs(10);
/// let mut connection = launched.connect(deadline)?;
/// connection.send(b"a message of the program's protocol", Some(deadline))?;
/// let reply = connection.receive(Some(deadline))?;
/// # Ok(())
/// # }
/// ```
pub struct Connection {
    /// The host's NVXS connection to the VM's broker.
    inner: HostConnection,
}

impl Connection {
    /// Connects to `endpoint` once the process that `process` names serves it, and authenticates
    /// with `capability` by `deadline`.
    pub(crate) fn connect(
        endpoint: &str,
        process: &ProcessRef,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> Result<Self, ConnectionError> {
        let transport = transport::connect_verified(endpoint, process, deadline)
            .map_err(ConnectionError::from_connect)?;
        let inner = HostConnection::attach(transport, capability, deadline)?;
        Ok(Self { inner })
    }

    /// Sends `data` to the guest, after the bytes of earlier sends, by `deadline`, or without a
    /// time limit. Empty data sends nothing.
    ///
    /// A send whose deadline passed before it began sends nothing, and leaves the connection
    /// usable. One that times out while it writes ends the connection, since the guest could see
    /// the stream cut short.
    pub fn send(&mut self, data: &[u8], deadline: Option<Instant>) -> Result<(), ConnectionError> {
        Ok(self.inner.send(data, deadline)?)
    }

    /// Returns the next bytes from the guest, at least one, waiting until `deadline`, or without a
    /// time limit.
    ///
    /// Once `deadline` has passed, it returns only bytes that have arrived, and otherwise fails
    /// with [`ConnectionErrorKind::TimedOut`] without waiting. A receive that times out loses
    /// nothing.
    pub fn receive(&mut self, deadline: Option<Instant>) -> Result<Vec<u8>, ConnectionError> {
        Ok(self.inner.receive(deadline)?)
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Connection").finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl std::os::fd::AsFd for Connection {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.inner
            .as_fd()
            .expect("a verified connection is a Unix socket")
    }
}

/// Classification of a [`ConnectionError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConnectionErrorKind {
    /// The deadline passed. The connection stays usable after a receive, and after a send whose
    /// deadline passed before it began, but not after a send that timed out while it wrote.
    TimedOut,
    /// The VM's process exited before the connection opened.
    Exited,
    /// A process other than the VM's serves its control endpoint, or the host may not connect to
    /// it. The connection sent no capability.
    PeerMismatch,
    /// The VM refused the capability.
    Rejected,
    /// The VM closed the session, or the stream to it broke.
    Closed,
    /// The VM reset the session, which ended this connection's attachment.
    Reset,
    /// The VM sent what the session's protocol does not allow.
    Protocol,
    /// Another I/O failure of the host, or one that left it unable to tell whether the VM's
    /// process runs.
    Io,
    /// An earlier failure ended the connection.
    Ended,
}

/// Failure of a [`Connection`], or of opening one: its [`ConnectionErrorKind`], a description,
/// and an optional cause.
///
/// It converts into the crate's [`Error`], whose source it then is.
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

    /// Returns whether the VM closed or reset the session, or the stream to it broke: the
    /// connection ended, but the VM may still run, and a new connection may succeed.
    pub fn is_lost(&self) -> bool {
        matches!(
            self.kind,
            ConnectionErrorKind::Closed | ConnectionErrorKind::Reset
        )
    }

    /// Classifies a failure of [`transport::connect_verified`], keeping its description and
    /// cause.
    fn from_connect(error: Error) -> Self {
        let (kind, message, source) = error.into_parts();
        let denied = source
            .as_deref()
            .and_then(|source| source.downcast_ref::<io::Error>())
            .is_some_and(|source| source.kind() == io::ErrorKind::PermissionDenied);
        let kind = match kind {
            ErrorKind::TimedOut => ConnectionErrorKind::TimedOut,
            ErrorKind::Exited => ConnectionErrorKind::Exited,
            ErrorKind::Protocol if denied => ConnectionErrorKind::PeerMismatch,
            ErrorKind::Protocol => ConnectionErrorKind::Closed,
            _ => ConnectionErrorKind::Io,
        };
        Self {
            kind,
            message,
            source,
        }
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

impl From<HostConnectionError> for ConnectionError {
    fn from(error: HostConnectionError) -> Self {
        let message = error.to_string();
        let (kind, source): (_, Option<Box<dyn StdError + Send + Sync>>) = match error {
            HostConnectionError::TimedOut => (ConnectionErrorKind::TimedOut, None),
            HostConnectionError::Closed => (ConnectionErrorKind::Closed, None),
            HostConnectionError::Rejected(_) => (ConnectionErrorKind::Rejected, None),
            HostConnectionError::Reset(_) => (ConnectionErrorKind::Reset, None),
            HostConnectionError::Protocol(error @ SessionError::Truncated { .. }) => {
                (ConnectionErrorKind::Closed, Some(Box::new(error)))
            }
            HostConnectionError::Protocol(error) => {
                (ConnectionErrorKind::Protocol, Some(Box::new(error)))
            }
            HostConnectionError::Io(error) => (io_kind(&error), Some(Box::new(error))),
            HostConnectionError::Ended => (ConnectionErrorKind::Ended, None),
        };
        Self {
            kind,
            message,
            source,
        }
    }
}

impl From<ConnectionError> for Error {
    fn from(error: ConnectionError) -> Self {
        let kind = match error.kind {
            ConnectionErrorKind::TimedOut => ErrorKind::TimedOut,
            ConnectionErrorKind::Exited => ErrorKind::Exited,
            ConnectionErrorKind::Io => ErrorKind::Io,
            _ => ErrorKind::Protocol,
        };
        Error::new(kind, error.message.clone()).with_source(error)
    }
}

/// Classifies a transport failure of an open connection.
fn io_kind(error: &io::Error) -> ConnectionErrorKind {
    match error.kind() {
        io::ErrorKind::TimedOut => ConnectionErrorKind::TimedOut,
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::NotConnected
        | io::ErrorKind::UnexpectedEof
        | io::ErrorKind::WriteZero => ConnectionErrorKind::Closed,
        _ => ConnectionErrorKind::Io,
    }
}

/// A VM's boot console, of a VM launched with a console listener: the bytes that the guest's
/// console writes, and those that it reads.
pub struct Console {
    /// Verified transport of the console's endpoint.
    transport: Box<dyn Transport>,
}

impl Console {
    /// Connects to the boot console of `outputs` once the process that `process` names serves it,
    /// by `deadline`.
    ///
    /// A VM without a console listener is [`ErrorKind::Unsupported`]. Other failures are those of
    /// a control connection's: a passed deadline is [`ErrorKind::TimedOut`], an exited process
    /// [`ErrorKind::Exited`], an endpoint that another process serves [`ErrorKind::Protocol`], and
    /// another failure [`ErrorKind::Io`], each with its original cause.
    pub fn connect(outputs: &VmOutputs, process: &ProcessRef, deadline: Instant) -> Result<Self> {
        let endpoint = outputs
            .console
            .as_deref()
            .ok_or_else(|| Error::unsupported("the VM has no boot console listener"))?;
        let transport = transport::connect_verified(endpoint, process, deadline)?;
        Ok(Self { transport })
    }

    /// Reads at least one byte of the console's output into `buffer`, waiting until `deadline`,
    /// or without a time limit, and returns how many it read, or 0 once the console closed.
    ///
    /// Fails with [`io::ErrorKind::TimedOut`] once `deadline` passed, which loses nothing. Once
    /// it has passed, the read takes only what has arrived, without waiting.
    pub fn read(&mut self, buffer: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
        self.transport.read(buffer, timeout(deadline))
    }

    /// Writes all of `data` to the console's input by `deadline`, or without a time limit.
    pub fn write_all(&mut self, data: &[u8], deadline: Option<Instant>) -> io::Result<()> {
        self.transport.write_all(data, timeout(deadline))
    }
}

impl fmt::Debug for Console {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Console").finish_non_exhaustive()
    }
}

/// Converts a deadline into the time left, zero once it has passed.
fn timeout(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

#[cfg(test)]
mod tests;
