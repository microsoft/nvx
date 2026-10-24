use std::fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use super::Transport;
use crate::host::supervisor::Inherited;
use crate::host::sys::{self, Errno, PidFd};
use crate::hypervisor::Hypervisor;

#[cfg(feature = "cli")]
pub(crate) use crate::host::sys::{interrupted, watch_interrupts, write_interruptibly};

/// Whether this host can run NVX microVMs.
pub const SUPPORTED: bool = true;

/// File that holds the identity of the current boot, a UUID that the kernel chooses at random.
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

/// Mode of a directory that only the current user may access: read, write, and search for its
/// owner, and nothing for anyone else.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Mode of the directory of a sandbox store, the exact mode that OpenVMM requires of the
/// directory of its control socket.
const STORE_DIR_MODE: u32 = 0o700;

/// Permission bits of a mode: those of the owner, group, and others, and the setuid, setgid, and
/// sticky bits.
const PERMISSION_BITS: u32 = 0o7777;

/// Permission bits of a mode that grant access to the group and to others.
const GROUP_AND_OTHER_BITS: u32 = 0o077;

/// Returns the start time of a live process, `None` if the process no longer exists, or an error
/// if its state cannot be determined.
///
/// The value is the `starttime` field of `/proc/<pid>/stat`. A process runs until its last thread
/// exits, as on Windows: a zombie whose main thread exited first still runs while its other threads
/// exit, and they keep the process's descriptors, such as an inherited log and its lock, open.
/// Zombies without other threads count as exited.
pub fn process_start_time(pid: u32) -> io::Result<Option<u64>> {
    let Some(stat) = read_stat(pid)? else {
        return Ok(None);
    };
    start_time_from_stat(pid, &stat, || other_threads_remain(pid))
}

/// Reads `/proc/<pid>/stat`, or returns `None` if no process has the ID.
fn read_stat(pid: u32) -> io::Result<Option<String>> {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => Ok(Some(stat)),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::ESRCH as i32) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Interprets the `/proc/<pid>/stat` line of `pid` as [`process_start_time`] does, asking
/// `other_threads_remain` only about a zombie.
pub(crate) fn start_time_from_stat(
    pid: u32,
    stat: &str,
    other_threads_remain: impl FnOnce() -> io::Result<bool>,
) -> io::Result<Option<u64>> {
    let (state, start_time) = parse_stat(pid, stat)?;
    match state {
        "X" => Ok(None),
        "Z" => Ok(other_threads_remain()?.then_some(start_time)),
        _ => Ok(Some(start_time)),
    }
}

/// Returns the state and the start time in the `/proc/<pid>/stat` line of `pid`.
fn parse_stat(pid: u32, stat: &str) -> io::Result<(&str, u64)> {
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
    Ok((fields[0], start_time))
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

/// Returns how a wait with `left` before its deadline goes on: the whole milliseconds, rounded
/// down, for which it polls the process's pidfd, and, when there are none, the remainder below a
/// millisecond that it sleeps before it checks the process a last time.
fn wait_step(left: Duration) -> (Duration, Duration) {
    let whole = Duration::from_millis(u64::try_from(left.as_millis()).unwrap_or(u64::MAX));
    let sleep = if whole.is_zero() {
        left
    } else {
        Duration::ZERO
    };
    (whole, sleep)
}

/// A process that [`open_process`] or [`pin_child`] verified, held through a pidfd. On hosts
/// without pidfds, only its identity holds it, which waits check at every poll.
pub(crate) struct ProcessHandle(Option<PidFd>);

impl ProcessHandle {
    /// Returns a handle without a pidfd, as hosts without pidfds have.
    #[cfg(test)]
    pub(crate) fn without_pidfd() -> Self {
        Self(None)
    }

    /// Whether the process's exit wakes its waiters: it does through a pidfd.
    pub(crate) fn has_exit_event(&self) -> bool {
        self.0.is_some()
    }

    /// Waits until the process exits or `deadline` passes, and returns whether it exited, or
    /// `None` without a pidfd, where the caller polls the process's identity instead.
    ///
    /// `poll` waits for whole milliseconds, so a remainder below a millisecond is slept rather
    /// than rounded up to a millisecond, before a last check, as [`wait_step`] describes. The wait
    /// thus ends about at its deadline, and never before it unless the process exited.
    pub(crate) fn wait(&self, deadline: Instant) -> io::Result<Option<bool>> {
        let Some(pidfd) = &self.0 else {
            return Ok(None);
        };
        loop {
            let (whole, sleep) = wait_step(deadline.saturating_duration_since(Instant::now()));
            if !sleep.is_zero() {
                thread::sleep(sleep);
            }
            if pidfd.wait_exit(whole)? {
                return Ok(Some(true));
            }
            if whole.is_zero() || Instant::now() >= deadline {
                return Ok(Some(false));
            }
        }
    }

    /// Sends `SIGKILL` to the process through its pidfd, unless it has exited.
    ///
    /// Without a pidfd, it fails rather than signal process `pid`, whose ID another process may
    /// have reused.
    pub(crate) fn kill(&self, pid: u32) -> io::Result<()> {
        let Some(pidfd) = &self.0 else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "cannot pin process {pid} for termination: this host does not provide pidfds"
                ),
            ));
        };
        match pidfd.kill() {
            Err(error) if error.raw_os_error() == Some(Errno::ESRCH as i32) => Ok(()),
            result => super::unless_exited(result, || pidfd.wait_exit(Duration::ZERO)),
        }
    }
}

/// Opens the process `pid` if it still runs with the start time `start_time`, and returns `None`
/// if it has exited, even if another process now has its ID.
///
/// The pidfd is opened before the identity is checked, so it holds the process that the check
/// then finds: a process that started at `start_time` and still runs held the ID when the pidfd
/// was opened. Hosts without pidfds, Linux before 5.3 or sandboxes that refuse them, get a
/// handle without one. Fails as [`check_identity`] does when `/proc` hides the process.
pub(crate) fn open_process(pid: u32, start_time: u64) -> io::Result<Option<ProcessHandle>> {
    let pidfd = match PidFd::open(pid) {
        Ok(pidfd) => Some(pidfd),
        Err(error) => match error.raw_os_error().map(Errno::from_raw) {
            // No process has the ID, or a thread of another process has it, so the process
            // that had it has exited.
            Some(Errno::ESRCH | Errno::EINVAL) => return Ok(None),
            _ if pidfds_unavailable(&error) => None,
            _ => return Err(error),
        },
    };
    check_identity(pid, start_time, pidfd, read_stat(pid)?)
}

/// Returns a handle to the process `pid`, through `pidfd` where the host provides pidfds, if
/// `stat`, its `/proc/<pid>/stat` line read after the pidfd was opened, shows that it still runs
/// with the start time `start_time`, and `None` if it has exited.
///
/// A missing line means that the process exited, unless a process still has the ID: its pidfd
/// shows that it runs, or without a pidfd, signal 0 finds a process with the ID, which may be
/// another one that reused it. `/proc` then hides that process from this user, as the `hidepid`
/// mount option hides the processes of other users, and the check fails with
/// [`io::ErrorKind::PermissionDenied`] since the process's identity cannot be read.
pub(crate) fn check_identity(
    pid: u32,
    start_time: u64,
    pidfd: Option<PidFd>,
    stat: Option<String>,
) -> io::Result<Option<ProcessHandle>> {
    let Some(stat) = stat else {
        let held = match &pidfd {
            Some(pidfd) => !pidfd.wait_exit(Duration::ZERO)?,
            None => sys::process_exists(pid)?,
        };
        if held {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "cannot check the identity of process {pid}: /proc hides it from this user"
                ),
            ));
        }
        return Ok(None);
    };
    if start_time_from_stat(pid, &stat, || other_threads_remain(pid))? != Some(start_time) {
        return Ok(None);
    }
    Ok(Some(ProcessHandle(pidfd)))
}

/// Holds the child process `pid`, which the caller has not reaped, and returns its start time.
///
/// Until it is reaped, a child that exited keeps its ID and its start time.
pub(crate) fn pin_child(pid: u32) -> io::Result<(ProcessHandle, u64)> {
    let pidfd = match PidFd::open(pid) {
        Ok(pidfd) => Some(pidfd),
        Err(error) if pidfds_unavailable(&error) => None,
        Err(error) => return Err(error),
    };
    let stat = read_stat(pid)?
        .ok_or_else(|| io::Error::other(format!("child process {pid} has been reaped")))?;
    let (_, start_time) = parse_stat(pid, &stat)?;
    Ok((ProcessHandle(pidfd), start_time))
}

/// Returns whether a failure of `pidfd_open` shows that this host does not provide pidfds:
/// Linux before 5.3 lacks the call, and seccomp profiles and other sandboxes may refuse it.
fn pidfds_unavailable(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(Errno::from_raw),
        Some(Errno::ENOSYS | Errno::EPERM | Errno::EACCES)
    )
}

/// Returns the identity of the current boot: the UUID in `/proc/sys/kernel/random/boot_id`, as
/// 16 bytes in the order in which it is written.
pub(crate) fn boot_id() -> io::Result<[u8; 16]> {
    let text = fs::read_to_string(BOOT_ID_PATH)?;
    let digits = text.trim_end().replace('-', "");
    super::parse_hex_id(&digits).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{BOOT_ID_PATH} has an unexpected format"),
        )
    })
}

/// Starts the command in a new session so it outlives the caller and its terminal, with the
/// descriptors `inherit` under their numbers in the caller, and no other descriptor of the caller
/// beyond the standard streams that the command sets up.
///
/// Only the new process clears the close-on-exec flag of the descriptors in `inherit`, so no
/// process that another thread spawns meanwhile inherits them. Fails with
/// [`io::ErrorKind::InvalidInput`] for a descriptor of a standard stream, 0 to 2, which the
/// command's own standard streams replace in the new process, and for a descriptor that is not
/// close-on-exec in the caller, which any process that the caller spawns would inherit.
pub(crate) fn detach(command: &mut Command, inherit: &[Inherited<'_>]) -> io::Result<()> {
    sys::detach(command, allowlist(inherit)?);
    Ok(())
}

/// Starts the command in the caller's session, as a child that shares the caller's terminal, with
/// the descriptors `inherit` under their numbers in the caller, and no other descriptor of the
/// caller beyond the standard streams that the command sets up, as [`detach`] does.
///
/// Fails as [`detach`] does.
#[cfg(feature = "cli")]
pub(crate) fn inherit_only(command: &mut Command, inherit: &[Inherited<'_>]) -> io::Result<()> {
    sys::inherit_only(command, allowlist(inherit)?);
    Ok(())
}

/// Returns the numbers of the descriptors `inherit`, each once, after checking that a spawn can
/// pass them to its process alone, as [`detach`] says.
fn allowlist(inherit: &[Inherited<'_>]) -> io::Result<Vec<RawFd>> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};

    let mut descriptors: Vec<RawFd> = Vec::with_capacity(inherit.len());
    for inherited in inherit {
        let descriptor = inherited.as_fd().as_raw_fd();
        if descriptor <= sys::LAST_STANDARD_STREAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "descriptor {descriptor} cannot be inherited: the VMM's own standard streams \
                     replace descriptors 0 to {}",
                    sys::LAST_STANDARD_STREAM
                ),
            ));
        }
        let flags = FdFlag::from_bits_retain(fcntl(inherited.as_fd(), FcntlArg::F_GETFD)?);
        if !flags.contains(FdFlag::FD_CLOEXEC) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "descriptor {descriptor} cannot be inherited by the VMM alone: it is not \
                     close-on-exec, so any process that this process spawns inherits it"
                ),
            ));
        }
        if !descriptors.contains(&descriptor) {
            descriptors.push(descriptor);
        }
    }
    Ok(descriptors)
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
    match DirBuilder::new().mode(PRIVATE_DIR_MODE).create(path) {
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
    fs::set_permissions(path, Permissions::from_mode(PRIVATE_DIR_MODE))
}

/// Creates the directory `path` of a sandbox store with mode 0700, or checks that an existing one
/// is private to the current user, as [`check_store_dir`] does, and returns why it is not, if it
/// is not.
///
/// A new directory takes the setgid bit of its parent, and loses the bits that the umask clears,
/// even the current user's permission to read it, so its mode is set again, to the 0700 that
/// OpenVMM requires of the directory of its control socket. An existing directory is never
/// changed: others could have placed records in it, or still hold its files open, which a change
/// of its mode would not revoke.
pub(crate) fn create_store_dir(path: &Path) -> io::Result<Option<String>> {
    create_dir_of_mode(path, STORE_DIR_MODE)?;
    check_store_dir(path)
}

/// Creates the directory `path` with mode 0700, or checks that an existing one is private to the
/// current user, as [`check_owner_only_dir`] does, and returns why it is not, if it is not.
///
/// A new directory takes the setgid bit of its parent, and loses the bits that the umask clears,
/// even the current user's permission to read it, so its mode is set again. An existing directory
/// is never changed: others could have placed files in it, or still hold them open, which a
/// change of its mode would not revoke.
pub(crate) fn create_owner_only_dir(path: &Path) -> io::Result<Option<String>> {
    create_dir_of_mode(path, PRIVATE_DIR_MODE)?;
    check_owner_only_dir(path)
}

/// Creates the directory `path`, whose parent must exist, with exactly the mode `mode`, unless it
/// exists already, which leaves it as it is.
fn create_dir_of_mode(path: &Path, mode: u32) -> io::Result<()> {
    match DirBuilder::new().mode(mode).create(path) {
        Ok(()) => {
            if let Some(directory) = open_plain_dir(path)? {
                // A descriptor that only locates the directory cannot change its mode itself, but
                // its link in /proc names exactly the directory that it opened.
                fs::set_permissions(
                    Path::new("/proc/self/fd").join(directory.as_raw_fd().to_string()),
                    Permissions::from_mode(mode),
                )?;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Returns why the directory `path` cannot hold a sandbox store, if it cannot: unless it is
/// private to the current user, as [`create_store_dir`] makes it, belonging to the user with mode
/// 0700.
pub(crate) fn check_store_dir(path: &Path) -> io::Result<Option<String>> {
    let Some(directory) = open_plain_dir(path)? else {
        return Ok(Some("is not a plain directory".to_owned()));
    };
    let metadata = directory.metadata()?;
    Ok(store_dir_problem(
        metadata.uid(),
        metadata.mode(),
        sys::effective_ids().0,
    ))
}

/// Returns why the directory `path` is not private to the current user, if it is not: unless it
/// is a directory, not a link to one, that belongs to the user and grants no one else any access,
/// which [`create_owner_only_dir`] makes it.
pub(crate) fn check_owner_only_dir(path: &Path) -> io::Result<Option<String>> {
    let Some(directory) = open_plain_dir(path)? else {
        return Ok(Some("is not a plain directory".to_owned()));
    };
    let metadata = directory.metadata()?;
    Ok(owner_only_problem(
        metadata.uid(),
        metadata.mode(),
        sys::effective_ids().0,
    ))
}

/// Returns why a directory of a sandbox store, which the user `uid` owns with the mode `mode`, is
/// not private to the user `euid`, if it is not: it must belong to that user, grant no access to
/// anyone else, and have mode 0700, which OpenVMM requires of the directory of its control
/// socket.
pub(crate) fn store_dir_problem(uid: u32, mode: u32, euid: u32) -> Option<String> {
    let mode = mode & PERMISSION_BITS;
    owner_only_problem(uid, mode, euid).or_else(|| {
        (mode != STORE_DIR_MODE).then(|| {
            format!(
                "has mode {mode:03o}, not the mode 700 that the directory of a control socket \
                 must have"
            )
        })
    })
}

/// Returns why a directory that the user `uid` owns with the mode `mode` is not private to the
/// user `euid`, if it is not: it must belong to that user, and grant no access to anyone else.
pub(crate) fn owner_only_problem(uid: u32, mode: u32, euid: u32) -> Option<String> {
    let mode = mode & PERMISSION_BITS;
    if uid != euid {
        Some(format!(
            "belongs to user {uid}, not to the current user {euid}"
        ))
    } else if mode & GROUP_AND_OTHER_BITS != 0 {
        Some(format!("is accessible to other users (mode {mode:03o})"))
    } else {
        None
    }
}

/// Returns why the file that `file` holds open, the lock, a record, or the capability of a
/// sandbox store, is not private to the current user, if it is not: never, on Linux, where no
/// one else may look up a file in the store's directory, which grants them no access.
pub(crate) fn private_file_problem(_file: &File) -> io::Result<Option<String>> {
    Ok(None)
}

/// Gives the regular file `path` the mode `mode` if it belongs to the current user, and leaves
/// anything else, which includes a symbolic link or a missing file, unchanged.
///
/// The file is opened only to locate it, which needs no permission on the file itself.
pub(crate) fn restore_private_file_mode(path: &Path, mode: u32) -> io::Result<()> {
    use nix::fcntl::OFlag;

    let opened = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_PATH | OFlag::O_NOFOLLOW).bits())
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::ELOOP as i32) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.uid() != sys::effective_ids().0 {
        return Ok(());
    }
    // A descriptor that only locates the file cannot change its mode itself, but its link in
    // /proc names exactly the file that it opened.
    fs::set_permissions(
        Path::new("/proc/self/fd").join(file.as_raw_fd().to_string()),
        Permissions::from_mode(mode),
    )
}

/// Opens the directory `path` only to locate it, which needs no permission on the directory
/// itself, or returns `None` if it is a symbolic link or not a directory.
fn open_plain_dir(path: &Path) -> io::Result<Option<File>> {
    use nix::fcntl::OFlag;

    let opened = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW).bits())
        .open(path);
    match opened {
        Ok(directory) => Ok(Some(directory)),
        Err(error)
            if error.raw_os_error() == Some(Errno::ELOOP as i32)
                || error.raw_os_error() == Some(Errno::ENOTDIR as i32) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Returns the device and inode of a file or directory.
pub fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Returns the device and inode of the file that `file` holds open.
pub(crate) fn opened_file_identity(file: &File) -> io::Result<(u64, u64)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Claims a launch's fresh log, which `file` holds open.
///
/// The claim is an exclusive `flock` lock. It belongs to the open file description, which the
/// VMM shares through the standard output and error that it inherits, and only the last close of
/// that description releases it, so it outlives the caller while the VMM runs.
pub(crate) fn claim_log(file: &File) -> io::Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(io::Error::other("another process holds the log")),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Returns whether a process still holds the claimed log at `path`, which must be the file
/// `identity`: whether its `flock` claim is held through another open file description.
///
/// The open follows no link and does not wait, as it would for a FIFO, so that only the claimed
/// file is examined.
pub(crate) fn log_claim_held(path: &Path, identity: (u64, u64)) -> io::Result<bool> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_NONBLOCK).bits())
        .open(path)?;
    if opened_file_identity(&file)? != identity {
        return Err(io::Error::other(format!(
            "{} is not the claimed log",
            path.display()
        )));
    }
    // A lock taken here goes when the file closes, at the end of this function.
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Returns whether the live process `pid` that started at `start_time`, which serves the control
/// endpoint of a launch whose log is the file `log`, is that launch's VMM: whether it holds the
/// log as its standard output.
///
/// The endpoint is the same socket path for every launch of a sandbox, so the log, which every
/// launch creates afresh, tells its VMM apart. The start time is read again after the
/// descriptor, so the answer cannot describe another process that reused the ID meanwhile.
pub(crate) fn serves_launch(pid: u32, start_time: u64, log: (u64, u64)) -> io::Result<bool> {
    if process_start_time(pid)? != Some(start_time) {
        return Ok(false);
    }
    let held = match fs::metadata(format!("/proc/{pid}/fd/1")) {
        Ok(metadata) => (metadata.dev(), metadata.ino()) == log,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::ESRCH as i32) =>
        {
            false
        }
        Err(error) => return Err(error),
    };
    Ok(held && process_start_time(pid)? == Some(start_time))
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

    fn write_all(&mut self, mut data: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        // A blocking send restarts its timeout whenever the endpoint makes room, so one that
        // drains slowly would hold the write past `timeout`. Sends therefore never wait, and
        // polls wait for room within what is left of `timeout`.
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        while !data.is_empty() {
            let left = super::time_left(deadline)?;
            match sys::send_nonblocking(&self.stream, data)? {
                0 => sys::wait_writable(&self.stream, left)?,
                sent => data = &data[sent..],
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    #[test]
    fn a_wait_sleeps_a_remainder_below_a_millisecond_instead_of_polling_a_millisecond() {
        assert_eq!(wait_step(Duration::ZERO), (Duration::ZERO, Duration::ZERO));
        for remainder in [Duration::from_micros(300), Duration::from_micros(999)] {
            assert_eq!(wait_step(remainder), (Duration::ZERO, remainder));
        }
        assert_eq!(
            wait_step(Duration::from_micros(1_999)),
            (Duration::from_millis(1), Duration::ZERO)
        );
        assert_eq!(
            wait_step(Duration::MAX),
            (Duration::from_millis(u64::MAX), Duration::ZERO)
        );
    }

    #[test]
    fn a_wait_with_less_than_a_millisecond_left_ends_at_its_deadline() {
        let mut child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let start_time = process_start_time(pid).unwrap().unwrap();
        let process = open_process(pid, start_time).unwrap().unwrap();
        let started = Instant::now();
        let mut waits = Vec::new();
        for _ in 0..20 {
            let deadline = Instant::now() + Duration::from_micros(300);
            waits.push((process.wait(deadline), Instant::now() >= deadline));
        }
        let elapsed = started.elapsed();
        child.kill().unwrap();
        child.wait().unwrap();
        for (wait, ended_after_deadline) in waits {
            assert_eq!(wait.unwrap(), Some(false));
            assert!(ended_after_deadline);
        }
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
        assert!(process.wait(Instant::now()).unwrap().unwrap());
    }
}
