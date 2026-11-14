//! Unix stream connections to OpenVMM's control socket.

use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::sockopt::PeerCredentials;
use nix::sys::socket::{
    AddressFamily, MsgFlags, SockFlag, SockType, UnixAddr, connect, getsockopt, send, socket,
};

use super::Errno;

/// Connects to the Unix socket at `path` without waiting in a full listen backlog, and returns
/// the connected stream in blocking mode.
///
/// A full backlog fails with [`io::ErrorKind::WouldBlock`], and a path that does not fit a Unix
/// socket address with [`io::ErrorKind::InvalidInput`].
pub(crate) fn connect_nonblocking(path: &str) -> io::Result<UnixStream> {
    let address = UnixAddr::new(path).map_err(|error| match error {
        Errno::ENAMETOOLONG => io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket path is too long",
        ),
        error => error.into(),
    })?;
    let socket = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        None,
    )?;
    connect(socket.as_raw_fd(), &address)?;
    let stream = UnixStream::from(socket);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// Returns the process ID of the peer credentials of a connected Unix stream.
pub(crate) fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    let credentials = getsockopt(stream, PeerCredentials)?;
    u32::try_from(credentials.pid()).map_err(io::Error::other)
}

/// Sends as much of `data` as the send buffer of `stream` takes without waiting, and returns how
/// much that was: nothing while the buffer is full.
pub(crate) fn send_nonblocking(stream: &UnixStream, data: &[u8]) -> io::Result<usize> {
    let flags = MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL;
    match send(stream.as_raw_fd(), data, flags) {
        Err(Errno::EAGAIN | Errno::EINTR) => Ok(0),
        result => result.map_err(io::Error::from),
    }
}

/// Waits until `descriptor` has something to read, or its peer closed it, a signal arrives, or
/// `timeout` passes, and returns whether it is readable.
pub(crate) fn wait_readable(descriptor: &impl AsFd, timeout: Duration) -> io::Result<bool> {
    // Round up, so that a wait shorter than a millisecond does not spin.
    let timeout =
        PollTimeout::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(PollTimeout::MAX);
    let mut descriptors = [PollFd::new(descriptor.as_fd(), PollFlags::POLLIN)];
    match poll(&mut descriptors, timeout) {
        Ok(0) | Err(Errno::EINTR) => Ok(false),
        Ok(_) => Ok(true),
        Err(error) => Err(error.into()),
    }
}

/// Waits until `stream` has room to send, a signal arrives, or `timeout` passes (`None` waits
/// indefinitely).
pub(crate) fn wait_writable(stream: &UnixStream, timeout: Option<Duration>) -> io::Result<()> {
    // Round up, so that a wait shorter than a millisecond does not spin.
    let timeout = timeout.map_or(PollTimeout::NONE, |timeout| {
        PollTimeout::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(PollTimeout::MAX)
    });
    let mut descriptors = [PollFd::new(stream.as_fd(), PollFlags::POLLOUT)];
    match poll(&mut descriptors, timeout) {
        Ok(_) | Err(Errno::EINTR) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
