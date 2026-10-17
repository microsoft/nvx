use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use super::Transport;
use crate::host::sys::{self, Errno};
use crate::hypervisor::Hypervisor;

#[cfg(feature = "cli")]
pub(crate) use crate::host::sys::{interrupted, watch_interrupts};

/// Whether this host can run NVX microVMs.
pub const SUPPORTED: bool = true;

/// Returns the start time of a live process, `None` if the process no longer exists, or an error
/// if its state cannot be determined.
///
/// The value is the `starttime` field of `/proc/<pid>/stat`. A process runs until its last thread
/// exits, as on Windows: a zombie whose main thread exited first still runs while its other threads
/// exit, and they keep the process's descriptors, such as an inherited log and its lock, open.
/// Zombies without other threads count as exited.
pub fn process_start_time(pid: u32) -> io::Result<Option<u64>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::ESRCH as i32) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    start_time_from_stat(pid, &stat, || other_threads_remain(pid))
}

/// Interprets the `/proc/<pid>/stat` line of `pid` as [`process_start_time`] does, asking
/// `other_threads_remain` only about a zombie.
pub(crate) fn start_time_from_stat(
    pid: u32,
    stat: &str,
    other_threads_remain: impl FnOnce() -> io::Result<bool>,
) -> io::Result<Option<u64>> {
    let malformed = || io::Error::other(format!("/proc/{pid}/stat has an unexpected format"));
    // The command name may contain spaces and parentheses, so the fields start after the last
    // closing parenthesis.
    let fields: Vec<&str> = stat[stat.rfind(')').ok_or_else(malformed)? + 1..]
        .split_whitespace()
        .collect();
    // Field 22 of the stat line; the fields after the command name start at field 3.
    let start_time = fields
        .get(19)
        .and_then(|value| value.parse().ok())
        .ok_or_else(malformed)?;
    match fields[0] {
        "X" => Ok(None),
        "Z" => Ok(other_threads_remain()?.then_some(start_time)),
        _ => Ok(Some(start_time)),
    }
}

/// Returns whether a process has threads besides its main thread.
pub(crate) fn other_threads_remain(pid: u32) -> io::Result<bool> {
    match fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(threads) => Ok(threads.count() > 1),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::ESRCH as i32) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Kills the process if it still has the recorded identity.
///
/// A pidfd pins the process first, so the identity check cannot race with PID reuse. Hosts
/// without pidfds (Linux before 5.3, or seccomp profiles that reject them) get an error instead
/// of a racy kill.
pub(crate) fn kill_process(pid: u32, start_time: u64) -> io::Result<()> {
    let process = match sys::PidFd::open(pid) {
        Ok(process) => process,
        Err(error) if error.raw_os_error() == Some(Errno::ESRCH as i32) => return Ok(()),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot pin OpenVMM process {pid} for termination: {error}"),
            ));
        }
    };
    if process_start_time(pid)? != Some(start_time) {
        return Ok(());
    }
    match process.kill() {
        Err(error) if error.raw_os_error() == Some(Errno::ESRCH as i32) => Ok(()),
        result => result,
    }
}

/// Starts the command in a new session so it outlives the caller and its terminal, and with no
/// descriptor of the caller beyond the standard streams that the command sets up.
pub(crate) fn detach(command: &mut Command, _breakaway_from_job: bool) {
    sys::detach(command);
}

/// Checks that the hypervisor device is present and accessible.
pub(crate) fn probe_hypervisor(hypervisor: Hypervisor) -> Result<(), String> {
    let device = match hypervisor {
        Hypervisor::Kvm => "/dev/kvm",
        Hypervisor::Mshv => "/dev/mshv",
        Hypervisor::Whp => return Err("WHP is only available on Windows".to_owned()),
    };
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map(drop)
        .map_err(|error| format!("{device} is not accessible: {error}"))
}

/// Creates `path` if needed and restricts it to the current user.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a plain directory",
            path.display()
        )));
    }
    fs::set_permissions(path, Permissions::from_mode(0o700))
}

/// Returns the device and inode of a file or directory.
pub fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Returns the control endpoint OpenVMM should listen on for a sandbox.
pub(crate) fn control_endpoint(socket_path: &Path) -> io::Result<String> {
    socket_path
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("control socket path is not valid UTF-8"))
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
struct UnixTransport {
    /// Connected stream to OpenVMM's control socket.
    stream: UnixStream,
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

    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_write_timeout(socket_timeout(timeout))?;
        self.stream.write_all(data).map_err(map_timeout)
    }
}
