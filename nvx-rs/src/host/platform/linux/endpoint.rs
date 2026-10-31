//! The Unix sockets of OpenVMM's endpoints: its control endpoint and the transport over it, and
//! the listener of a clone's restore-ready event.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::host::platform::{Transport, time_left};
use crate::host::sys;

/// Returns the control endpoint OpenVMM should listen on for a sandbox.
pub(crate) fn control_endpoint(socket_path: &Path) -> io::Result<String> {
    socket_path
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("control socket path is not valid UTF-8"))
}

/// The listener at which the launch of a clone awaits the restore-ready event of its VMM: a Unix
/// socket, which the VMM connects to once, and which queues its connection until the host
/// accepts it, then the connection that the host accepted, from which it reads the event.
#[derive(Debug)]
pub(crate) struct ReadyListener {
    /// The listening socket, which the VMM finds as long as it is open.
    socket: UnixListener,
    /// The VMM's connection, once the host accepted it.
    connection: Option<UnixStream>,
}

/// Listens at the Unix socket `socket_path`, in place of a socket that an earlier launch left
/// there, as the restore-ready endpoint of a clone's launch, and returns the endpoint, as
/// OpenVMM's `--restore-ready-path` takes it, and its listener. The socket has mode 0600,
/// whatever the umask, as [`set_ready_socket_mode`] gives it.
pub(crate) fn listen_ready(socket_path: &Path) -> io::Result<(String, ReadyListener)> {
    let endpoint = socket_path
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("restore-ready socket path is not valid UTF-8"))?;
    match fs::remove_file(socket_path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let socket = UnixListener::bind(socket_path)?;
    // The umask may clear bits of the mode that the bind gives the socket, even its owner's
    // permission to write it, without which a VMM that is not root cannot connect: the mode is
    // set again before any VMM may connect.
    set_ready_socket_mode(socket_path)?;
    // The host polls the socket before it accepts, so an accept never blocks.
    socket.set_nonblocking(true)?;
    let listener = ReadyListener {
        socket,
        connection: None,
    };
    Ok((endpoint, listener))
}

impl ReadyListener {
    /// Waits at most `timeout` for what the VMM, the process `pid`, sends to the endpoint, and
    /// appends it to `received`, which then holds at most `limit` bytes. Returns whether more may
    /// arrive: `false` once the VMM closed its end of the connection.
    ///
    /// The endpoint's first connection must be the VMM's: one of another process fails with
    /// [`io::ErrorKind::PermissionDenied`].
    pub(crate) fn receive(
        &mut self,
        pid: u32,
        received: &mut Vec<u8>,
        limit: usize,
        timeout: Duration,
    ) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        if self.connection.is_none() {
            if !sys::wait_readable(&self.socket, timeout)? {
                return Ok(true);
            }
            let connection = match self.socket.accept() {
                Ok((connection, _)) => connection,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(true);
                }
                Err(error) => return Err(error),
            };
            let peer = sys::peer_pid(&connection)?;
            if peer != pid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "process {peer} connected to the restore-ready endpoint, not the VMM, \
                         process {pid}"
                    ),
                ));
            }
            connection.set_nonblocking(true)?;
            self.connection = Some(connection);
        }
        let connection = self
            .connection
            .as_mut()
            .expect("the host accepted the VMM's connection");
        let mut buffer = [0; 64];
        let room = limit.saturating_sub(received.len()).min(buffer.len());
        if room == 0
            || !sys::wait_readable(
                connection,
                deadline.saturating_duration_since(Instant::now()),
            )?
        {
            return Ok(true);
        }
        match connection.read(&mut buffer[..room]) {
            Ok(0) => Ok(false),
            Ok(count) => {
                received.extend_from_slice(&buffer[..count]);
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }
}

/// Gives the restore-ready socket at `socket_path`, which the caller bound, the mode 0600, whatever
/// the umask: its owner, the user of the clone's VMM, may connect to it, which takes the
/// permission to write it, and no one else may.
///
/// The socket is opened only to locate it, following no link at its name, and refused unless it
/// is a socket.
fn set_ready_socket_mode(socket_path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    use nix::fcntl::OFlag;

    let socket = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_PATH | OFlag::O_NOFOLLOW).bits())
        .open(socket_path)?;
    if !socket.metadata()?.file_type().is_socket() {
        return Err(io::Error::other(format!(
            "{} is not the restore-ready socket that the launch bound",
            socket_path.display()
        )));
    }
    // A descriptor that only locates the socket cannot change its mode itself, but its link in
    // /proc names exactly the socket that it opened.
    fs::set_permissions(
        Path::new("/proc/self/fd").join(socket.as_raw_fd().to_string()),
        Permissions::from_mode(0o600),
    )
}

/// Connects to a control endpoint served by the process `expected_pid`.
///
/// The connection attempt never blocks: a listener whose backlog is full yields
/// [`io::ErrorKind::WouldBlock`], and the caller retries within its deadline.
pub(crate) fn connect_endpoint(
    endpoint: &str,
    expected_pid: u32,
    _timeout: Duration,
) -> io::Result<Box<dyn Transport>> {
    let stream = sys::connect_nonblocking(endpoint)?;
    let peer = sys::peer_pid(&stream)?;
    if peer != expected_pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "control endpoint is served by process {peer}, not OpenVMM process {expected_pid}"
            ),
        ));
    }
    Ok(Box::new(UnixTransport { stream }))
}

/// Returns the process that listens on a control endpoint, or `None` if nothing listens.
pub(crate) fn endpoint_server_pid(endpoint: &str) -> io::Result<Option<u32>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match sys::connect_nonblocking(endpoint) {
            Ok(stream) => return sys::peer_pid(&stream).map(Some),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            // A full listen backlog drains as OpenVMM accepts clients.
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Blocking Unix-domain stream adapted to the control transport's deadline API.
pub(super) struct UnixTransport {
    /// Connected stream to OpenVMM's control socket.
    pub(super) stream: UnixStream,
}

/// Clamps a present socket timeout to the nonzero duration accepted by `std`.
fn socket_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout.map(|timeout| timeout.max(Duration::from_millis(1)))
}

/// Normalizes socket would-block and timeout errors to [`io::ErrorKind::TimedOut`].
fn map_timeout(error: io::Error) -> io::Error {
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) {
        io::ErrorKind::TimedOut.into()
    } else {
        error
    }
}

impl Transport for UnixTransport {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        self.stream.set_read_timeout(socket_timeout(timeout))?;
        loop {
            match self.stream.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result.map_err(map_timeout),
            }
        }
    }

    fn write_all(&mut self, mut data: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        // A blocking send restarts its timeout whenever the endpoint makes room, so one that
        // drains slowly would hold the write past `timeout`. Sends therefore never wait, and
        // polls wait for room within what is left of `timeout`.
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        while !data.is_empty() {
            let left = time_left(deadline)?;
            match sys::send_nonblocking(&self.stream, data)? {
                0 => sys::wait_writable(&self.stream, left)?,
                sent => data = &data[sent..],
            }
        }
        Ok(())
    }

    fn unread(&self) -> io::Result<Option<usize>> {
        sys::unread_by_peer(&self.stream).map(Some)
    }
}
