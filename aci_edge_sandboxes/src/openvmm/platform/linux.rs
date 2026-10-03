use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use super::Transport;
use crate::openvmm::config::Hypervisor;

/// Whether this host can run the OpenVMM backend.
pub(crate) const SUPPORTED: bool = true;

/// Returns the start time of a live process, `None` if the process no longer exists, or an error
/// if its state cannot be determined.
///
/// The value is the `starttime` field of `/proc/<pid>/stat`. Zombie processes count as exited.
pub(crate) fn process_start_time(pid: u32) -> io::Result<Option<u64>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let malformed = || io::Error::other(format!("/proc/{pid}/stat has an unexpected format"));
    let fields: Vec<&str> = stat[stat.rfind(')').ok_or_else(malformed)? + 1..]
        .split_whitespace()
        .collect();
    match fields.first() {
        Some(&"Z" | &"X") => Ok(None),
        // Field 22 of the stat line; the fields after the command name start at field 3.
        Some(_) => fields
            .get(19)
            .and_then(|value| value.parse().ok())
            .map(Some)
            .ok_or_else(malformed),
        None => Err(malformed()),
    }
}

/// Kills the process if it still has the recorded identity.
///
/// A pidfd pins the process first, so the identity check cannot race with PID reuse. Hosts
/// without pidfds (Linux before 5.3, or seccomp profiles that reject them) get an error instead
/// of a racy kill.
pub(crate) fn kill_process(pid: u32, start_time: u64) -> io::Result<()> {
    let raw_pid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
    // SAFETY: pidfd_open takes a process ID and flags and returns a new descriptor or -1.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, raw_pid, 0) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(io::Error::new(
                error.kind(),
                format!("cannot pin OpenVMM process {pid} for termination: {error}"),
            ))
        };
    }
    // SAFETY: the kernel returned a fresh descriptor that nothing else owns.
    let pidfd = unsafe { OwnedFd::from_raw_fd(descriptor as RawFd) };
    if process_start_time(pid)? != Some(start_time) {
        return Ok(());
    }
    // SAFETY: the descriptor is a valid pidfd, and a null siginfo with no flags is allowed.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

/// Starts the command in a new session so it outlives the caller and its terminal, and with no
/// descriptor of the caller beyond the standard streams that the command sets up.
pub(crate) fn detach(command: &mut Command, _breakaway_from_job: bool) {
    // SAFETY: the closure makes only async-signal-safe system calls and allocates nothing, as
    // code that runs between fork and exec must.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            mark_descriptors_close_on_exec()
        });
    }
}

/// Marks every descriptor above standard error close-on-exec.
///
/// Rust opens its own descriptors close-on-exec, but a descriptor that the caller inherited or
/// that native code opened may lack the flag, and a detached VM would hold it until it stops: a
/// pipe writer it holds, for example, keeps the reader from seeing end-of-file. Marking rather
/// than closing keeps the descriptor through which `Command::spawn` reports a failed exec.
fn mark_descriptors_close_on_exec() -> io::Result<()> {
    let first: libc::c_uint = 3;
    // SAFETY: close_range takes two descriptor bounds and flags and changes no memory.
    let marked = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if marked == 0 {
        return Ok(());
    }
    // Linux before 5.11 lacks CLOSE_RANGE_CLOEXEC, and seccomp policies may reject close_range.
    mark_listed_descriptors_close_on_exec()
}

/// Marks every descriptor above standard error that `/proc/self/fd` lists close-on-exec.
///
/// It reads the directory with getdents64 into a stack buffer, so it allocates nothing.
pub(crate) fn mark_listed_descriptors_close_on_exec() -> io::Result<()> {
    // SAFETY: the path is NUL-terminated, and the call returns a new descriptor or -1.
    let directory = unsafe {
        libc::open(
            c"/proc/self/fd".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if directory < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = mark_directory_entries(directory);
    // SAFETY: this function opened the descriptor, and nothing else uses it.
    unsafe { libc::close(directory) };
    result
}

fn mark_directory_entries(directory: RawFd) -> io::Result<()> {
    #[repr(C, align(8))]
    struct Records([u8; 4096]);

    let malformed = || io::Error::from_raw_os_error(libc::EIO);
    let mut buffer = Records([0; 4096]);
    loop {
        // SAFETY: the kernel writes at most the buffer's length into it.
        let read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory,
                buffer.0.as_mut_ptr(),
                buffer.0.len(),
            )
        };
        let Ok(read) = usize::try_from(read) else {
            return Err(io::Error::last_os_error());
        };
        if read == 0 {
            return Ok(());
        }
        let mut records = buffer.0.get(..read).ok_or_else(malformed)?;
        while !records.is_empty() {
            // A linux_dirent64 holds d_ino (8 bytes), d_off (8), d_reclen (2), d_type (1), and
            // the NUL-terminated name.
            let length = match records.get(16..18) {
                Some(&[low, high]) => usize::from(u16::from_ne_bytes([low, high])),
                _ => return Err(malformed()),
            };
            let (record, rest) = records.split_at_checked(length).ok_or_else(malformed)?;
            let name = record.get(19..).ok_or_else(malformed)?;
            if let Some(descriptor) = descriptor_number(name)
                && descriptor > 2
                && descriptor != directory
            {
                // SAFETY: F_SETFD changes only the flags of the descriptor; a stale number fails.
                unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) };
            }
            records = rest;
        }
    }
}

/// Parses a `/proc/self/fd` entry name, which ends at its NUL terminator.
fn descriptor_number(name: &[u8]) -> Option<RawFd> {
    let digits = name.split(|&byte| byte == 0).next()?;
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0, |value: RawFd, &byte| {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit < 10)?;
        value.checked_mul(10)?.checked_add(RawFd::from(digit))
    })
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
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
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
pub(crate) fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
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
    let stream = connect_nonblocking(endpoint)?;
    let peer = peer_pid(&stream)?;
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
        match connect_nonblocking(endpoint) {
            Ok(stream) => return peer_pid(&stream).map(Some),
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

fn connect_nonblocking(path: &str) -> io::Result<UnixStream> {
    // SAFETY: socket has no memory-safety preconditions.
    let descriptor = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel returned a fresh descriptor that nothing else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(descriptor) };
    // SAFETY: sockaddr_un is plain data for which all-zero bytes are valid.
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_bytes();
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket path is too long",
        ));
    }
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let length =
        libc::socklen_t::try_from(mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
            .map_err(io::Error::other)?;
    // SAFETY: the address is a NUL-terminated sockaddr_un of `length` bytes.
    if unsafe { libc::connect(socket.as_raw_fd(), (&raw const address).cast(), length) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(socket);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length =
        libc::socklen_t::try_from(size_of::<libc::ucred>()).map_err(io::Error::other)?;
    // SAFETY: the descriptor is a connected socket and the buffer matches the reported length.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    u32::try_from(credentials.pid).map_err(io::Error::other)
}

struct UnixTransport {
    stream: UnixStream,
}

fn socket_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout.map(|timeout| timeout.max(Duration::from_millis(1)))
}

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
