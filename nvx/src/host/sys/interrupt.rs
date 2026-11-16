//! Ctrl-C on Unix hosts: a `SIGINT` handler that records the interrupt, which the CLI polls, and
//! writes that the interrupt ends.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::BorrowedFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::errno::Errno;
use nix::libc;
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

/// Whether a `SIGINT` arrived since [`watch_interrupts`] installed its handler.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// The longest, in milliseconds, that [`write_interruptibly`] waits for room in its stream
/// before it checks for a `SIGINT` again.
const WAIT_SLICE_MS: u16 = 250;

/// The `SIGINT` handler, which only records the signal.
extern "C" fn record(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Records each `SIGINT` for [`interrupted`] instead of letting it terminate the process, as
/// Python's handler, which raises `KeyboardInterrupt`, does.
///
/// Like Python, it keeps a `SIGINT` that the process started with ignored: a caller that ignores
/// the signal for its children keeps it from interrupting them.
pub(crate) fn watch_interrupts() -> io::Result<()> {
    let mut current = MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: a null action only reads the current disposition into `current`.
    Errno::result(unsafe { libc::sigaction(libc::SIGINT, ptr::null(), current.as_mut_ptr()) })?;
    // SAFETY: sigaction filled `current` in.
    if unsafe { current.assume_init() }.sa_sigaction == libc::SIG_IGN {
        return Ok(());
    }
    let action = SigAction::new(
        SigHandler::Handler(record),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe { sigaction(Signal::SIGINT, &action) }?;
    Ok(())
}

/// Returns whether a `SIGINT` arrived since [`watch_interrupts`].
pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Writes all of `data` to `stream`, such as standard output, and fails with
/// [`io::ErrorKind::Interrupted`] once a `SIGINT` has arrived, as a write of Python raises
/// `KeyboardInterrupt`, also while the stream waits for a reader that does not read.
///
/// The handler of [`watch_interrupts`] restarts the system calls that a `SIGINT` interrupts, so
/// a write that blocks would resume after it. Instead, this waits for room with `poll`, which a
/// signal ends, in slices of at most 250 ms, and writes at most `PIPE_BUF` bytes at once, which a
/// pipe with room takes without blocking. Like the standard streams of the standard library, it
/// discards what it writes to a descriptor that is not open.
pub(crate) fn write_interruptibly(stream: BorrowedFd<'_>, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        if interrupted() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        }
        let mut descriptors = [PollFd::new(stream, PollFlags::POLLOUT)];
        match poll(&mut descriptors, WAIT_SLICE_MS) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
        // An error or hang-up that `poll` reports surfaces as the write's error.
        match nix::unistd::write(stream, &data[..data.len().min(libc::PIPE_BUF)]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => data = &data[count..],
            Err(Errno::EINTR | Errno::EAGAIN) => {}
            Err(Errno::EBADF) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
