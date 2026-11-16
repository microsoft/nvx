//! Verified references to OpenVMM processes, through which callers wait for them and end them.
//!
//! A [`ProcessRef`] names one process by its ID, its start time, and the [`BootId`] of the host
//! boot during which it started, which together never name another process. [`Process::open`]
//! checks a reference against the host and returns [`Opened::Running`] with the process held
//! through a handle of the operating system, or [`Opened::Exited`] with proof that the recorded
//! process exited. The handle is a pidfd on Linux, and a process handle on Windows. Waits and
//! termination go through that handle, so they reach the referenced process, whether or not the
//! caller started it, and never another process that reuses its ID. A process that does not match its
//! reference is never signaled, and neither is one that a reference from an earlier boot names:
//! every process of that boot has exited.
//!
//! [`Process::wait`] sleeps until the exit event of the process: until its pidfd becomes readable
//! or its handle is signaled. A VMM that a caller finds again after the caller restarted, within
//! the same boot, is therefore waited for as a child is. Linux hosts without pidfds, before
//! Linux 5.3 or in sandboxes that refuse them, check the reference at a short interval instead,
//! until the deadline of the wait, and cannot terminate a process that they cannot hold.
//!
//! [`Process::bytes_read`] reports how many bytes the process has read through the operating
//! system's read calls, and [`own_bytes_read`] how many the calling process has, so that a caller
//! can tell how much of its files a VMM, or the caller itself, read.
//!
//! [`vm::spawn`](super::vm::spawn) launches OpenVMM, and the [`Launched`](super::vm::Launched)
//! that it returns holds the child that it started through a handle of its own.

use std::fmt;
use std::io;
use std::str::FromStr;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::platform;
use crate::error::Error;

/// Longest wait for a terminated OpenVMM process to exit.
#[cfg_attr(
    not(feature = "cli"),
    allow(
        dead_code,
        reason = "only the CLI waits for the OpenVMM processes that it terminates"
    )
)]
pub(crate) const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Interval at which the exit of a process is polled where no exit event serves: the exit of any
/// process on Linux hosts without pidfds, and the exit of a child that the crate's spawn of a VMM
/// waits for with a timeout on Linux, since
/// [`std::process::Child`] can wait only without one. An exit is noticed at most this late, for
/// about 40 checks a second.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The identity of one boot of the host, which changes whenever the host restarts.
///
/// On Linux, it is the UUID in `/proc/sys/kernel/random/boot_id`, which the kernel chooses at
/// random at every boot. On Windows, it is the boot counter that Windows keeps in the registry
/// value `BootId` of
/// `HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters`,
/// little-endian and zero-extended to 16 bytes. OpenVMM records the same identity in snapshots.
///
/// Its text form, which `Display`, `FromStr`, and serde use, is 32 lowercase hexadecimal digits;
/// parsing also accepts uppercase ones.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BootId([u8; 16]);

impl BootId {
    /// Returns the identity of the current boot.
    ///
    /// The first identity that it reads serves the rest of the calling process, which cannot
    /// outlive the boot. Fails with [`io::ErrorKind::Unsupported`] on hosts other than Linux and
    /// Windows.
    pub fn current() -> io::Result<Self> {
        /// The identity of the current boot, once read.
        static CURRENT: OnceLock<BootId> = OnceLock::new();
        if let Some(boot) = CURRENT.get() {
            return Ok(*boot);
        }
        let boot = Self(platform::boot_id()?);
        Ok(*CURRENT.get_or_init(|| boot))
    }

    /// Returns the identity whose bytes are `bytes`.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the bytes of the identity.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}

impl fmt::Debug for BootId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "BootId({self})")
    }
}

impl FromStr for BootId {
    type Err = Error;

    /// Parses the text form of a boot identity, failing with
    /// [`ErrorKind::Io`](crate::ErrorKind::Io) for anything other than 32 hexadecimal digits.
    fn from_str(text: &str) -> crate::Result<Self> {
        platform::parse_hex_id(text).map(Self).ok_or_else(|| {
            Error::io(format!(
                "{text:?} is not a boot identity, which is 32 hexadecimal digits"
            ))
        })
    }
}

impl Serialize for BootId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BootId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A reference to one process: its ID, its start time, and the boot of the host during which it
/// started, which together never name another process.
///
/// The start time is, on Linux, the `starttime` field of `/proc/<pid>/stat`, in clock ticks since
/// boot, and on Windows, the creation time of the process as a 64-bit `FILETIME`. A reference is a
/// claim, which a caller may record and read back after it restarts: [`Process::open`] checks it
/// before anything waits for the process or signals it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessRef {
    /// Process ID.
    pub pid: u32,
    /// Start time: clock ticks since boot on Linux, and a creation `FILETIME` on Windows.
    pub start_time: u64,
    /// The boot of the host during which the process started.
    pub boot: BootId,
}

/// How a [`Process`] learns that it exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaitMethod {
    /// The exit event of the operating system: a pidfd on Linux, and the process handle on
    /// Windows. Waits sleep until the process exits or their deadline passes.
    Event,
    /// A check of the process's identity every 25 ms, until the process exits or the deadline of
    /// the wait passes, on Linux hosts without pidfds: before Linux 5.3, or in sandboxes that
    /// refuse them.
    Polling,
}

/// An independently owned, close-on-exec copy of a process's pidfd, for a caller's exit waiter.
///
/// Implements the standard descriptor traits, including [`std::os::fd::AsFd`]. Dropping it closes
/// only this copy, without keeping the process running or affecting the original [`Process`].
#[cfg(target_os = "linux")]
pub type ExitHandle = std::os::fd::OwnedFd;

/// An independently owned, non-inheritable copy of a process handle, for a caller's exit waiter.
///
/// Implements the standard handle traits, including [`std::os::windows::io::AsHandle`]. Dropping it
/// closes only this copy, without keeping the process running or affecting the original [`Process`].
/// The duplicate retains the original process handle's access rights.
#[cfg(windows)]
pub type ExitHandle = std::os::windows::io::OwnedHandle;

/// A process that this host found running with the identity of a [`ProcessRef`], held through a
/// handle of the operating system: a pidfd on Linux, and a process handle on Windows.
///
/// The handle keeps waits and termination on this process, whether or not the caller is its
/// parent, even after the process exits and another one reuses its ID. It belongs to the
/// `Process`, which closes it when dropped, and does not keep the process running. Threads may
/// share a `Process` and wait for it at once.
pub struct Process {
    /// The reference that the process matched.
    reference: ProcessRef,
    /// The handle that holds the process.
    handle: platform::ProcessHandle,
}

/// The result of checking a recorded process reference against this host.
#[derive(Debug)]
pub enum Opened {
    /// The process matched the recorded boot, ID, and start time; the caller owns its `Process`.
    Running(Process),
    /// The recorded process exited: its boot ended, or no running process matches its identity.
    Exited(Exited),
}

/// Proof that a verified process exited, produced only by [`Process::wait`] or [`Process::open`].
///
/// It proves exit, not the process's exit status, and cannot be constructed by a caller.
#[derive(Debug)]
pub struct Exited(());

impl Process {
    /// Finds the process that `reference` names, or returns proof that it has exited.
    ///
    /// The process has exited when no process has its ID, when the process that has it started
    /// at another time, and when the reference comes from another boot.
    ///
    /// An error means that the state of the process cannot be determined: it may still run.
    /// Fails with [`io::ErrorKind::PermissionDenied`] when the process that has the ID hides its
    /// identity from this user: on Windows, when this user may not query it, and on Linux, when
    /// `/proc` does not show it, as its `hidepid` mount option arranges for the processes of
    /// other users. Fails with [`io::ErrorKind::InvalidInput`] for a process ID that the host
    /// never assigns, such as 0, and with [`io::ErrorKind::Unsupported`] on hosts other than
    /// Linux and Windows.
    pub fn open(reference: &ProcessRef) -> io::Result<Opened> {
        let handle = open_reference(reference, BootId::current, platform::open_process)?;
        Ok(match handle {
            Some(handle) => Opened::Running(Self {
                reference: reference.clone(),
                handle,
            }),
            None => Opened::Exited(Exited(())),
        })
    }

    /// Returns the process that `handle` holds, whose identity the caller verified to be
    /// `reference`, as a child that it has not reaped has.
    pub(crate) fn held(reference: ProcessRef, handle: platform::ProcessHandle) -> Self {
        Self { reference, handle }
    }

    /// Returns the reference that the process matched.
    pub fn reference(&self) -> &ProcessRef {
        &self.reference
    }

    /// Returns how waits learn that the process exited.
    pub fn wait_method(&self) -> WaitMethod {
        if self.handle.has_exit_event() {
            WaitMethod::Event
        } else {
            WaitMethod::Polling
        }
    }

    /// Duplicates the existing OS exit-wait handle for a caller to register with its own runtime.
    ///
    /// The returned handle is independent of this borrow and remains valid after this `Process`
    /// is dropped. Returns `None` only on a Linux host that polls without a pidfd. Duplication
    /// failures have kind [`ErrorKind::Io`](crate::ErrorKind::Io) and retain their original source.
    #[cfg(any(target_os = "linux", windows))]
    pub fn exit_handle(&self) -> crate::Result<Option<ExitHandle>> {
        self.handle.exit_handle().map_err(|error| {
            Error::io("cannot duplicate the process's exit handle").with_source(error)
        })
    }

    /// Waits until the process exits or `deadline` passes, returning exit proof or `None`.
    ///
    /// A process exits with its last thread. The wait sleeps until the exit event of the
    /// process, or with [`WaitMethod::Polling`], checks its identity at a short interval. An
    /// error means that the wait cannot determine whether the process exited.
    pub fn wait(&self, deadline: Instant) -> io::Result<Option<Exited>> {
        let exited = wait_handle(
            &self.handle,
            self.reference.pid,
            self.reference.start_time,
            deadline,
        )?;
        Ok(exited.then_some(Exited(())))
    }

    /// Returns whether the process has exited, without waiting.
    pub fn has_exited(&self) -> io::Result<bool> {
        self.wait(Instant::now()).map(|exited| exited.is_some())
    }

    /// Ends the process at once, unless it has exited or is exiting: with `SIGKILL` on Linux, and
    /// on Windows by terminating it with the status 1. It does not wait for the exit; see
    /// [`wait`](Self::wait).
    ///
    /// Fails with [`io::ErrorKind::Unsupported`] with [`WaitMethod::Polling`], whose host cannot
    /// hold the process, rather than signal an ID that another process may have reused, and
    /// with [`io::ErrorKind::PermissionDenied`] when this user may not end the process.
    pub fn kill(&self) -> io::Result<()> {
        self.handle.kill(self.reference.pid)
    }

    /// Returns how many bytes the process has read through the operating system's read calls,
    /// from files, pipes, sockets, and devices alike, whether the host's cache served them or not:
    /// the `rchar` of `/proc/<pid>/io` on Linux, and the `ReadTransferCount` of
    /// `GetProcessIoCounters` on Windows. Neither counts what the process reads otherwise: the
    /// pages that it maps from a file, as OpenVMM maps the memory of a snapshot that it restores,
    /// nor, on Linux, what it reads through io_uring, as OpenVMM reads a sandbox's block devices.
    /// On Linux, the count also holds what the process's children read, once the process reaped
    /// them, as the kernel accounts them to their parent, while Windows counts the process's own
    /// reads alone.
    ///
    /// On Linux, the count is read from the process's `/proc` file, which is opened before the
    /// process's identity is checked, so that the count is the process's own, and the read fails
    /// with [`io::ErrorKind::NotFound`] once the process has exited. On Windows, the handle keeps
    /// the count of a process that exited. Fails with [`io::ErrorKind::PermissionDenied`] when
    /// this user may not read the process's counters, and with [`io::ErrorKind::Unsupported`] on
    /// hosts other than Linux and Windows.
    pub fn bytes_read(&self) -> io::Result<u64> {
        self.handle
            .bytes_read(self.reference.pid, self.reference.start_time)
    }
}

impl fmt::Debug for Process {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Process")
            .field("reference", &self.reference)
            .field("wait_method", &self.wait_method())
            .finish_non_exhaustive()
    }
}

/// Returns how many bytes the calling process has read through the operating system's read
/// calls, as [`Process::bytes_read`] counts them: on Linux, with what the children that it reaped
/// read.
///
/// Fails with [`io::ErrorKind::Unsupported`] on hosts other than Linux and Windows.
pub fn own_bytes_read() -> io::Result<u64> {
    platform::own_bytes_read()
}

/// Checks `reference` against the boot that `current_boot` returns, and during the same boot,
/// opens the process with `open`, as [`Process::open`] does.
fn open_reference<H>(
    reference: &ProcessRef,
    current_boot: impl FnOnce() -> io::Result<BootId>,
    open: impl FnOnce(u32, u64) -> io::Result<Option<H>>,
) -> io::Result<Option<H>> {
    if reference.pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "0 is not the ID of a process",
        ));
    }
    // Every process of an earlier boot has exited.
    if current_boot()? != reference.boot {
        return Ok(None);
    }
    open(reference.pid, reference.start_time)
}

/// Waits until the process that `handle` holds exits or `deadline` passes, and returns whether
/// it exited, checking the identity `pid` and `start_time` where the handle has no exit event.
fn wait_handle(
    handle: &platform::ProcessHandle,
    pid: u32,
    start_time: u64,
    deadline: Instant,
) -> io::Result<bool> {
    match handle.wait(deadline)? {
        Some(exited) => Ok(exited),
        // Each check verifies the identity as `Process::open` does, so that a process that
        // `/proc` hides never counts as exited.
        None => poll_exit(deadline, || {
            Ok(platform::open_process(pid, start_time)?.is_some())
        }),
    }
}

/// Asks `runs` whether the process still runs, at every [`POLL_INTERVAL`] until it does not or
/// `deadline` passes, and returns whether it stopped running: it exited, even if another
/// process reused its ID.
fn poll_exit(deadline: Instant, mut runs: impl FnMut() -> io::Result<bool>) -> io::Result<bool> {
    loop {
        if !runs()? {
            return Ok(true);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        thread::sleep(POLL_INTERVAL.min(deadline - now));
    }
}

#[cfg(test)]
mod tests;
