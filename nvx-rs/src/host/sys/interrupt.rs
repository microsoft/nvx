//! Ctrl-C on Unix hosts: a `SIGINT` handler that records the interrupt, which the CLI polls.

use std::io;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

/// Whether a `SIGINT` arrived since [`watch_interrupts`] installed its handler.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

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
