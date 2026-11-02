//! Signal dispositions, masks, descriptors, and queued signals.

use std::os::fd::{AsFd, BorrowedFd};
use std::time::Duration;

use nix::sys::signal::{SigHandler, SigSet, SigmaskHow, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};

use super::{Errno, FileDescriptor, Mode, OpenFlags, Pollable, ProcessId, poll};

/// One signal read from a [`SignalDescriptor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SignalEvent {
    /// The signal, or `None` for a number that `Signal` cannot represent, such as a real-time
    /// signal.
    pub(crate) signal: Option<Signal>,
    /// Whether `sigqueue` sent the signal, with [`value`](Self::value).
    pub(crate) queued: bool,
    /// The integer that `sigqueue` sent with the signal, in its low 32 bits.
    pub(crate) value: u32,
}

/// Opens a close-on-exec, nonblocking signal descriptor that reads `mask`'s signals.
fn signal_descriptor(mask: &SigSet) -> Result<SignalFd, Errno> {
    SignalFd::with_flags(mask, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK)
}

/// An owned signal descriptor that reads signals that the calling thread blocks. Dropping it
/// closes the descriptor, and leaves the signals blocked.
#[derive(Debug)]
pub(crate) struct SignalDescriptor(SignalFd);

impl SignalDescriptor {
    /// Blocks `signals` in the calling thread, and opens a descriptor that reads them.
    ///
    /// A process that blocks signals before it starts threads blocks them in every thread.
    pub(crate) fn new(signals: &[Signal]) -> Result<Self, Errno> {
        let mask = signals.iter().copied().collect::<SigSet>();
        mask.thread_block()?;
        signal_descriptor(&mask).map(Self)
    }

    /// Reads one pending signal, or returns `None` when no signal is pending.
    pub(crate) fn read(&self) -> Option<SignalEvent> {
        let information = self.0.read_signal().ok()??;
        Some(SignalEvent {
            signal: Signal::try_from(information.ssi_signo as i32).ok(),
            queued: information.ssi_code == nix::libc::SI_QUEUE,
            value: information.ssi_int as u32,
        })
    }

    /// Borrows the descriptor for `poll` in this module tree.
    pub(super) fn borrowed(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Ignores each of `signals` for the rest of the process.
///
/// Like the C programs, it ignores failures, which `signal` reports only for an invalid signal.
pub(crate) fn ignore_signals(signals: &[Signal]) {
    for &signal in signals {
        // SAFETY: `SIG_IGN` installs no Rust callback.
        let _ = unsafe { nix::sys::signal::signal(signal, SigHandler::SigIgn) };
    }
}

/// Writes `bytes` to `descriptor` with one `write` system call, as `FileDescriptor::write` does,
/// without letting the `SIGPIPE` of a pipe or socket whose reader is gone reach the thread: the
/// write then fails with `EPIPE` alone.
///
/// The kernel sends that signal to the writing thread, so the thread blocks it for the write, and
/// takes back the one that a failed write left pending, which `sigtimedwait` takes before any
/// signal pending for the whole process. Neither the process's disposition nor another thread
/// changes, so threads may write at once, and a thread that blocks `SIGPIPE` has no signal of the
/// write's left to receive once it unblocks it. A `SIGPIPE` that was pending for the thread
/// itself before the write stays pending, and the write's joins it, so the write then takes back
/// none; one pending for the whole process stays as well, beside the write's, which the write
/// takes back.
///
/// `nix` wraps neither `sigpending` nor `sigtimedwait`, so this calls them through libc.
pub(crate) fn write_without_sigpipe(
    descriptor: &FileDescriptor,
    bytes: &[u8],
) -> Result<usize, Errno> {
    without_sigpipe(|| descriptor.write(bytes))
}

/// Writes all `bytes` to standard error and ignores failures, as `write_standard_error` does, but
/// makes each `write` as [`write_without_sigpipe`] does: a standard error whose reader is gone
/// fails the write without raising `SIGPIPE`, whatever the process's disposition of it.
#[cfg(target_arch = "x86_64")]
pub(crate) fn write_standard_error_without_sigpipe(bytes: &[u8]) {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match without_sigpipe(|| nix::unistd::write(std::io::stderr(), remaining)) {
            Ok(0) => return,
            Ok(count) => remaining = &remaining[count..],
            Err(Errno::EINTR) => {}
            Err(_) => return,
        }
    }
}

/// Makes the one `write` system call of `write` with `SIGPIPE` blocked in the calling thread, and
/// takes back the signal that its failure with `EPIPE` left pending, as [`write_without_sigpipe`]
/// describes.
fn without_sigpipe(write: impl FnOnce() -> Result<usize, Errno>) -> Result<usize, Errno> {
    let pipe = SigSet::from(Signal::SIGPIPE);
    let previous = pipe.thread_swap_mask(SigmaskHow::SIG_BLOCK)?;
    // A failure to tell leaves any pending signal alone.
    let joined = is_pending_for_thread(Signal::SIGPIPE).unwrap_or(true);
    let written = write();
    if written == Err(Errno::EPIPE) && !joined {
        let immediately = nix::libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: the set and the zero timeout are valid for the call, which only removes a
        // pending `SIGPIPE`, and a null pointer asks for no details of it.
        while unsafe { nix::libc::sigtimedwait(pipe.as_ref(), std::ptr::null_mut(), &immediately) }
            < 0
            && Errno::last() == Errno::EINTR
        {}
    }
    // Restoring the mask that the thread had a moment ago cannot fail.
    let _ = previous.thread_set_mask();
    written
}

/// Returns whether `signal` is pending for the calling thread or for the whole process.
fn is_pending(signal: Signal) -> Result<bool, Errno> {
    // SAFETY: zero is a valid initial representation of `sigset_t`, which the call fills.
    let mut pending = unsafe { std::mem::zeroed::<nix::libc::sigset_t>() };
    // SAFETY: `pending` is a writable signal set, which the call only fills.
    Errno::result(unsafe { nix::libc::sigpending(&mut pending) })?;
    // SAFETY: `pending` is a valid signal set, and `signal` a valid signal number.
    Ok(unsafe { nix::libc::sigismember(&pending, signal as nix::libc::c_int) } == 1)
}

/// Returns whether `signal` is pending for the calling thread itself, whether or not it is also
/// pending for the whole process.
///
/// `sigpending` reports both at once, and only the thread's status file tells them apart, from
/// its `SigPnd` line, so the file is read only once `sigpending` reports `signal`. It is read
/// into a buffer on the stack, and nothing is allocated.
fn is_pending_for_thread(signal: Signal) -> Result<bool, Errno> {
    if !is_pending(signal)? {
        return Ok(false);
    }
    let status = super::open(
        c"/proc/thread-self/status",
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    // The status file names the pending signals well within its first page.
    let mut buffer = [0u8; 4096];
    let mut length = 0;
    while length < buffer.len() {
        match status.read(&mut buffer[length..]) {
            Ok(0) => break,
            Ok(count) => length += count,
            Err(Errno::EINTR) => {}
            Err(error) => return Err(error),
        }
    }
    let pending = thread_pending_signals(&buffer[..length]).ok_or(Errno::EPROTO)?;
    Ok(pending & (1 << (signal as u32 - 1)) != 0)
}

/// Returns the signals pending for the thread whose status file begins with `status`, from its
/// `SigPnd` line, in which bit `n - 1` stands for signal `n`.
fn thread_pending_signals(status: &[u8]) -> Option<u64> {
    const LABEL: &[u8] = b"\nSigPnd:\t";
    let start = status
        .windows(LABEL.len())
        .position(|window| window == LABEL)?
        + LABEL.len();
    let digits = status[start..].split(|&byte| byte == b'\n').next()?;
    u64::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
}

/// Restores the default `SIGCHLD` disposition, which an inherited `SIG_IGN` would replace with
/// the kernel's automatic reaping of children, before any `wait` sees them.
pub(crate) fn reset_child_signal() {
    // SAFETY: `SIG_DFL` installs no Rust callback.
    let _ = unsafe { nix::sys::signal::signal(Signal::SIGCHLD, SigHandler::SigDfl) };
}

/// Sends `SIGUSR1` to `process` with `value`, which a [`SignalEvent`] returns.
///
/// `nix` does not wrap `sigqueue`, so this calls it through libc.
pub(crate) fn queue_user_signal(process: ProcessId, value: u32) -> Result<(), Errno> {
    let value = nix::libc::sigval {
        sival_ptr: value as usize as *mut nix::libc::c_void,
    };
    // SAFETY: `sigqueue` passes the integer payload to the kernel and dereferences nothing.
    Errno::result(unsafe { nix::libc::sigqueue(process.raw(), Signal::SIGUSR1 as i32, value) })
        .map(drop)
}

/// A wait for `SIGCHLD`, which blocks the signal in the calling thread while it lives and reads
/// it from a signal descriptor. Dropping it closes the descriptor and restores the thread's
/// previous signal mask.
#[derive(Debug)]
pub(crate) struct ChildSignalWait {
    /// The thread's signal mask before the wait blocked `SIGCHLD`.
    previous: SigSet,
    /// The descriptor that reads `SIGCHLD`.
    descriptor: SignalFd,
}

impl ChildSignalWait {
    /// Blocks `SIGCHLD` in the calling thread before the caller forks the child that it waits
    /// for, so that the signal of a child that ends at once stays pending.
    pub(crate) fn new() -> Result<Self, Errno> {
        let mask = SigSet::from(Signal::SIGCHLD);
        let previous = mask.thread_swap_mask(SigmaskHow::SIG_BLOCK)?;
        match signal_descriptor(&mask) {
            Ok(descriptor) => Ok(Self {
                previous,
                descriptor,
            }),
            Err(error) => {
                let _ = previous.thread_set_mask();
                Err(error)
            }
        }
    }

    /// Waits at most `timeout` for `SIGCHLD`, and consumes every pending one.
    ///
    /// A signal that arrived earlier ends the wait at once. A failed poll ends it too, and the
    /// caller, which checks its child after each wait, treats it as a spurious wakeup.
    pub(crate) fn wait(&self, timeout: Duration) {
        let _ = poll([Pollable::ChildSignal(self)], Some(timeout));
        while let Ok(Some(_)) = self.descriptor.read_signal() {}
    }

    /// Restores the previous signal mask, as a forked child does before it runs, and reports a
    /// failure.
    pub(crate) fn restore(self) -> Result<(), Errno> {
        self.previous.thread_set_mask()
    }

    /// Borrows the descriptor for `poll` in this module tree.
    pub(super) fn borrowed(&self) -> BorrowedFd<'_> {
        self.descriptor.as_fd()
    }
}

impl Drop for ChildSignalWait {
    /// Restores the previous signal mask, best effort, if `restore` has not.
    fn drop(&mut self) {
        let _ = self.previous.thread_set_mask();
    }
}

#[cfg(test)]
pub(super) mod tests;
