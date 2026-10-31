//! Processes that this user may wait for and kill, held through pidfds that keep their IDs from
//! naming any other process, the bytes that processes read, the identities of the boot and of the
//! host, and the host's monotonic time.

use std::fs::{self, File};
use std::io::{self, Read};
use std::thread;
use std::time::{Duration, Instant};

use super::BOOT_ID_PATH;
use crate::host::platform::{parse_hex_id, parse_host_id, unless_exited};
use crate::host::sys::{self, Errno, PidFd};

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
pub(super) fn wait_step(left: Duration) -> (Duration, Duration) {
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
            result => unless_exited(result, || pidfd.wait_exit(Duration::ZERO)),
        }
    }

    /// Returns how many bytes process `pid`, which the handle holds, and which started at
    /// `start_time`, has read through read calls: the `rchar` of its `/proc/<pid>/io`.
    ///
    /// The file is opened before the process's identity is checked, so a process that still runs
    /// with that start time held the ID when the file was opened, and the count is its own, with
    /// or without a pidfd. Fails with [`io::ErrorKind::NotFound`] once the process has exited.
    pub(crate) fn bytes_read(&self, pid: u32, start_time: u64) -> io::Result<u64> {
        let path = format!("/proc/{pid}/io");
        let exited = || io::Error::new(io::ErrorKind::NotFound, format!("process {pid} exited"));
        let gone = |error: &io::Error| {
            error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::ESRCH as i32)
        };
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if gone(&error) => return Err(exited()),
            Err(error) => return Err(error),
        };
        if process_start_time(pid)? != Some(start_time) {
            return Err(exited());
        }
        let mut text = String::new();
        match file.read_to_string(&mut text) {
            Ok(_) => read_count(&path, &text),
            Err(error) if gone(&error) => Err(exited()),
            Err(error) => Err(error),
        }
    }
}

/// Returns how many bytes this process has read through read calls: the `rchar` of
/// `/proc/self/io`.
pub(crate) fn own_bytes_read() -> io::Result<u64> {
    let path = "/proc/self/io";
    read_count(path, &fs::read_to_string(path)?)
}

/// Returns the `rchar` count of the text `text` of the file at `path`, a process's `/proc` file
/// `io`: the bytes that the process, and the children that it reaped, passed to read calls, from
/// any file, whether the page cache served them or not, which counts no read through io_uring nor
/// a file's mapped pages.
fn read_count(path: &str, text: &str) -> io::Result<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("rchar:"))
        .and_then(|count| count.trim().parse().ok())
        .ok_or_else(|| io::Error::other(format!("{path} has no rchar count")))
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
    parse_hex_id(&digits).ok_or_else(|| {
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
    parse_host_id(&text).ok_or_else(|| {
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
