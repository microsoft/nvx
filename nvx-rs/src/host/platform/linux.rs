use std::fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::stat::Mode;
use nix::unistd::Uid;

use super::Transport;
use super::inherit::Inherited;
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
const PRIVATE_DIR_MODE: Mode = Mode::S_IRWXU;

/// Mode of the directory of a sandbox store, the exact mode that OpenVMM requires of the
/// directory of its control socket.
const STORE_DIR_MODE: Mode = Mode::S_IRWXU;

/// Permission bits of a mode that grant access to the group and to others.
const GROUP_AND_OTHER_BITS: Mode = Mode::S_IRWXG.union(Mode::S_IRWXO);

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
///
/// The line is read as bytes because it need not be UTF-8: its command name may hold any bytes,
/// and the kernel cuts a name to 15 bytes without regard to character boundaries.
fn read_stat(pid: u32) -> io::Result<Option<Vec<u8>>> {
    match fs::read(format!("/proc/{pid}/stat")) {
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
    stat: &[u8],
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
fn parse_stat(pid: u32, stat: &[u8]) -> io::Result<(&str, u64)> {
    let malformed = || io::Error::other(format!("/proc/{pid}/stat has an unexpected format"));
    // The command name may contain spaces, parentheses, and bytes that are not UTF-8, so the
    // fields start after the last closing parenthesis, and only they must be text.
    let name_end = stat
        .iter()
        .rposition(|&byte| byte == b')')
        .ok_or_else(malformed)?;
    let fields: Vec<&str> = std::str::from_utf8(&stat[name_end + 1..])
        .map_err(|_| malformed())?
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
    stat: Option<Vec<u8>>,
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

/// The file of the host's machine ID.
const MACHINE_ID_PATH: &str = "/etc/machine-id";

/// Returns the identity of this host, as OpenVMM records it in a capture's time contract: its
/// machine ID, from [`MACHINE_ID_PATH`], as 16 bytes in the order in which it is written.
pub(crate) fn host_id() -> io::Result<[u8; 16]> {
    let text = fs::read_to_string(MACHINE_ID_PATH)?;
    super::parse_host_id(&text).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{MACHINE_ID_PATH} has an unexpected format"),
        )
    })
}

/// Returns the host's monotonic time, as OpenVMM measures the downtime of a capture within one
/// boot: `CLOCK_BOOTTIME`, in nanoseconds, which counts the time that the host was suspended.
pub(crate) fn monotonic_ns() -> io::Result<u64> {
    /// Nanoseconds in a second.
    const NANOS_PER_SECOND: u64 = 1_000_000_000;
    let time = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME)?;
    u64::try_from(time.tv_sec())
        .ok()
        .and_then(|seconds| seconds.checked_mul(NANOS_PER_SECOND))
        .zip(u64::try_from(time.tv_nsec()).ok())
        .and_then(|(seconds, nanos)| seconds.checked_add(nanos))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "CLOCK_BOOTTIME is out of range"))
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
    match DirBuilder::new().mode(PRIVATE_DIR_MODE.bits()).create(path) {
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
    fs::set_permissions(path, Permissions::from_mode(PRIVATE_DIR_MODE.bits()))
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
fn create_dir_of_mode(path: &Path, mode: Mode) -> io::Result<()> {
    match DirBuilder::new().mode(mode.bits()).create(path) {
        Ok(()) => {
            if let Some(directory) = open_plain_dir(path)? {
                // A descriptor that only locates the directory cannot change its mode itself, but
                // its link in /proc names exactly the directory that it opened.
                fs::set_permissions(
                    Path::new("/proc/self/fd").join(directory.as_raw_fd().to_string()),
                    Permissions::from_mode(mode.bits()),
                )?;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Returns the owner of the file that `metadata` describes and the permission bits of its mode:
/// those of the owner, group, and others, and the setuid, setgid, and sticky bits, without the
/// bits of the file's type.
///
/// The checks of ownership and permissions take these types, rather than the raw integers of
/// [`MetadataExt`], so that a mode passed as a user ID, or a user ID as a mode, fails to compile.
fn owner_and_mode(metadata: &fs::Metadata) -> (Uid, Mode) {
    (
        Uid::from_raw(metadata.uid()),
        Mode::from_bits_truncate(metadata.mode()),
    )
}

/// Returns why the directory `path` cannot hold a sandbox store, if it cannot: unless it is
/// private to the current user, as [`create_store_dir`] makes it, belonging to the user with mode
/// 0700.
pub(crate) fn check_store_dir(path: &Path) -> io::Result<Option<String>> {
    let Some(directory) = open_plain_dir(path)? else {
        return Ok(Some("is not a plain directory".to_owned()));
    };
    let (uid, mode) = owner_and_mode(&directory.metadata()?);
    Ok(store_dir_problem(uid, mode, Uid::effective()))
}

/// Returns why the directory `path` is not private to the current user, if it is not: unless it
/// is a directory, not a link to one, that belongs to the user and grants no one else any access,
/// which [`create_owner_only_dir`] makes it.
pub(crate) fn check_owner_only_dir(path: &Path) -> io::Result<Option<String>> {
    let Some(directory) = open_plain_dir(path)? else {
        return Ok(Some("is not a plain directory".to_owned()));
    };
    let (uid, mode) = owner_and_mode(&directory.metadata()?);
    Ok(owner_only_problem(uid, mode, Uid::effective()))
}

/// Returns why a directory of a sandbox store, which the user `uid` owns with the permission bits
/// `mode`, is not private to the user `euid`, if it is not: it must belong to that user, grant no
/// access to anyone else, and have mode 0700, which OpenVMM requires of the directory of its
/// control socket.
pub(crate) fn store_dir_problem(uid: Uid, mode: Mode, euid: Uid) -> Option<String> {
    owner_only_problem(uid, mode, euid).or_else(|| {
        (mode != STORE_DIR_MODE).then(|| {
            format!(
                "has mode {mode:03o}, not the mode 700 that the directory of a control socket \
                 must have"
            )
        })
    })
}

/// Returns why a directory that the user `uid` owns with the permission bits `mode` is not private
/// to the user `euid`, if it is not: it must belong to that user, and grant no access to anyone
/// else.
pub(crate) fn owner_only_problem(uid: Uid, mode: Mode, euid: Uid) -> Option<String> {
    if uid != euid {
        Some(format!(
            "belongs to user {uid}, not to the current user {euid}"
        ))
    } else if mode.intersects(GROUP_AND_OTHER_BITS) {
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

/// Returns why the directory `path`, a snapshot that OpenVMM published, is not private to the
/// current user, if it is not, as [`check_owner_only_dir`] does: OpenVMM creates it with mode 0700,
/// and its files with mode 0600.
pub(crate) fn check_snapshot_dir(path: &Path) -> io::Result<Option<String>> {
    check_owner_only_dir(path)
}

/// Returns why another user than the current one, or root, may change the file that `file` holds
/// open, if one may: unless it belongs to the current user or to root, and grants neither its
/// group nor others the permission to write it.
pub(crate) fn foreign_writer_problem(file: &File) -> io::Result<Option<String>> {
    let (uid, mode) = owner_and_mode(&file.metadata()?);
    Ok(foreign_writer(uid, mode, Uid::effective()))
}

/// Returns why another user than `euid`, or root, may change a file that the user `uid` owns with
/// the permission bits `mode`, if one may.
pub(crate) fn foreign_writer(uid: Uid, mode: Mode, euid: Uid) -> Option<String> {
    if uid != euid && !uid.is_root() {
        Some(format!(
            "belongs to user {uid}, neither the current user {euid} nor root"
        ))
    } else if mode.intersects(Mode::S_IWGRP | Mode::S_IWOTH) {
        Some(format!("is writable by other users (mode {mode:03o})"))
    } else {
        None
    }
}

/// Lets the regular file that `file` holds open for writing take no space for the blocks that
/// nobody writes: nothing to do on Linux, whose filesystems allocate no block that a file's
/// extension leaves unwritten.
pub(crate) fn make_sparse(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Renames `from` to `to` unless `to` exists, which it leaves as it is, failing with
/// [`io::ErrorKind::AlreadyExists`]: `renameat2` with `RENAME_NOREPLACE`, which a filesystem that
/// cannot rename without replacing refuses.
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    sys::rename_no_replace(from, to)
}

/// Gives the regular file `path` the mode `mode` if it belongs to the current user, and leaves
/// anything else, which includes a symbolic link or a missing file, unchanged.
///
/// The file is opened only to locate it, which needs no permission on the file itself.
pub(crate) fn restore_private_file_mode(path: &Path, mode: Mode) -> io::Result<()> {
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
    if !metadata.file_type().is_file() || Uid::from_raw(metadata.uid()) != Uid::effective() {
        return Ok(());
    }
    // A descriptor that only locates the file cannot change its mode itself, but its link in
    // /proc names exactly the file that it opened.
    fs::set_permissions(
        Path::new("/proc/self/fd").join(file.as_raw_fd().to_string()),
        Permissions::from_mode(mode.bits()),
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

/// Returns whether the file at `path` is deleted: whether no file has that name, which a removal
/// takes at once.
pub(crate) fn is_deleted(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Returns the device and inode of the file that `file` holds open.
pub(crate) fn opened_file_identity(file: &File) -> io::Result<(u64, u64)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Returns the identity by which the stores tell a file or directory apart from every other,
/// following its links: its device and inode, as [`opened_file_id`] gives them.
pub(crate) fn file_id(path: &Path) -> io::Result<(u64, u128)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), u128::from(metadata.ino())))
}

/// Returns the identity by which the stores tell the file that `file` holds open apart from every
/// other: its device and inode, as its [`FileSeal`](crate::host::seal::FileSeal) holds them.
pub(crate) fn opened_file_id(file: &File) -> io::Result<(u64, u128)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), u128::from(metadata.ino())))
}

/// Returns the number of names, its hard links, of the file that `file` holds open.
pub(crate) fn link_count(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.nlink())
}

/// An open directory, through which operations name its entries, so that they reach the entries
/// of the directory that it opened, whatever its path names meanwhile.
pub(crate) struct Directory(File);

impl Directory {
    /// Opens the directory `path`, following its links.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        use nix::fcntl::OFlag;

        OpenOptions::new()
            .read(true)
            .custom_flags(OFlag::O_DIRECTORY.bits())
            .open(path)
            .map(Self)
    }

    /// Returns the identity of its entry `name`, not following a link, as [`opened_file_id`] gives
    /// it, or `None` if it has no such entry.
    pub(crate) fn entry_id(&self, name: &str) -> io::Result<Option<(u64, u128)>> {
        use nix::fcntl::AtFlags;
        use nix::sys::stat::fstatat;

        match fstatat(&self.0, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(status) => Ok(Some((status.st_dev, u128::from(status.st_ino)))),
            Err(Errno::ENOENT) => Ok(None),
            Err(errno) => Err(errno.into()),
        }
    }

    /// Opens its entry `name` for reading, not following a link, and without waiting, as an open
    /// of a FIFO without a writer would, or returns `None` if it has no such entry. The caller
    /// checks what the entry is.
    pub(crate) fn open_file(&self, name: &str) -> io::Result<Option<File>> {
        use nix::fcntl::{OFlag, openat};

        match openat(
            &self.0,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => Ok(Some(File::from(file))),
            Err(Errno::ENOENT) => Ok(None),
            Err(errno) => Err(errno.into()),
        }
    }

    /// Opens the directory `path` with the right to search it alone, without following a link at
    /// its own name, though links on the way to it are followed, or returns `None` if it does not
    /// exist. Fails if it is a link, or not a directory.
    ///
    /// The handle names the directory's entries, and needs no right to read the directory, which
    /// an owner-only directory may withhold; it cannot [sync](Self::sync) it.
    pub(crate) fn search(path: &Path) -> io::Result<Option<Self>> {
        use nix::fcntl::OFlag;

        match OpenOptions::new()
            .read(true)
            .custom_flags((OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW).bits())
            .open(path)
        {
            Ok(dir) => Ok(Some(Self(dir))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error)
                if [Errno::ELOOP, Errno::ENOTDIR]
                    .map(|errno| Some(errno as i32))
                    .contains(&error.raw_os_error()) =>
            {
                Err(io::Error::other(format!(
                    "{} is a link, or not a directory",
                    path.display()
                )))
            }
            Err(error) => Err(error),
        }
    }

    /// Opens its subdirectory `name` with the right to search it alone, not following a link, as
    /// [`search`](Self::search) opens a directory, or returns `None` if it has no such entry.
    /// Fails if the entry is a link, or not a directory.
    pub(crate) fn search_dir(&self, name: &str) -> io::Result<Option<Self>> {
        self.open_dir_with(name, nix::fcntl::OFlag::O_PATH)
    }

    /// Opens its subdirectory `name` for reading, not following a link, or returns `None` if it
    /// has no such entry. Fails if the entry is a link, or not a directory.
    pub(crate) fn open_dir(&self, name: &str) -> io::Result<Option<Self>> {
        self.open_dir_with(name, nix::fcntl::OFlag::O_RDONLY)
    }

    /// Opens its subdirectory `name` with the access `access`, not following a link, as
    /// [`open_dir`](Self::open_dir) and [`search_dir`](Self::search_dir) do.
    fn open_dir_with(&self, name: &str, access: nix::fcntl::OFlag) -> io::Result<Option<Self>> {
        use nix::fcntl::{OFlag, openat};

        match openat(
            &self.0,
            name,
            access | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(dir) => Ok(Some(Self(File::from(dir)))),
            Err(Errno::ENOENT) => Ok(None),
            Err(Errno::ELOOP | Errno::ENOTDIR) => Err(io::Error::other(format!(
                "{name} is a link, or not a directory"
            ))),
            Err(errno) => Err(errno.into()),
        }
    }

    /// Removes its file `name`, if it has one.
    pub(crate) fn remove_file(&self, name: &str) -> io::Result<()> {
        use nix::unistd::{UnlinkatFlags, unlinkat};

        match unlinkat(&self.0, name, UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => Ok(()),
            Err(errno) => Err(errno.into()),
        }
    }

    /// Makes the changes to its entries so far durable, such as a file that was removed from it.
    pub(crate) fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }

    /// Returns why the directory cannot hold a sandbox store, as [`check_store_dir`] returns it for
    /// a path, if it cannot: unless it belongs to the current user with mode 0700.
    pub(crate) fn store_dir_problem(&self) -> io::Result<Option<String>> {
        let (uid, mode) = owner_and_mode(&self.0.metadata()?);
        Ok(store_dir_problem(uid, mode, Uid::effective()))
    }
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

    #[test]
    fn a_restore_ready_socket_lets_its_owner_connect_whatever_the_umask() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.sock");
        // A umask that clears even the owner's permission to write what it creates, in a thread
        // of the test's own, whose umask no other thread shares.
        let (endpoint, _listener) = thread::scope(|scope| {
            scope
                .spawn(|| {
                    nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
                    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o277));
                    listen_ready(&path)
                })
                .join()
                .unwrap()
        })
        .unwrap();
        let mode = fs::symlink_metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            Mode::from_bits_truncate(mode),
            Mode::S_IRUSR | Mode::S_IWUSR,
            "{mode:o}"
        );
        UnixStream::connect(&endpoint).unwrap();
    }

    #[test]
    fn a_restore_ready_client_that_closed_before_its_connection_was_accepted_is_read_to_its_end() {
        let pid = std::process::id();
        let receive_all = |listener: &mut ReadyListener| {
            let mut received = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while listener
                .receive(pid, &mut received, 64, Duration::from_millis(50))
                .unwrap()
            {
                assert!(Instant::now() < deadline, "the endpoint did not close");
            }
            received
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.sock");
        // A client that sent nothing, as a VMM that fails as it starts, and one that sent its
        // event, each closed before the listener accepted their connection: what they sent
        // remains to read, and then the end of the connection.
        for sent in [&b""[..], b"the event\n"] {
            let (endpoint, mut listener) = listen_ready(&path).unwrap();
            let mut client = UnixStream::connect(&endpoint).unwrap();
            std::io::Write::write_all(&mut client, sent).unwrap();
            drop(client);
            assert_eq!(receive_all(&mut listener), sent);
        }
        // A client that closes once the listener accepted its connection.
        let (endpoint, mut listener) = listen_ready(&path).unwrap();
        let client = UnixStream::connect(&endpoint).unwrap();
        let mut received = Vec::new();
        assert!(
            listener
                .receive(pid, &mut received, 64, Duration::from_millis(50))
                .unwrap()
        );
        drop(client);
        assert_eq!(receive_all(&mut listener), b"");
    }
}
