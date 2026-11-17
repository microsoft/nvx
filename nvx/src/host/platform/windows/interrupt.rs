//! The CLI's Ctrl-C, which it records instead of letting it terminate the process.

use std::io;

use windows_sys::Win32::System::Console::{CTRL_C_EVENT, SetConsoleCtrlHandler};

/// Whether Ctrl-C reached the console since [`watch_interrupts`] installed its handler.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The console control handler, which records Ctrl-C and leaves every other event, such as
/// Ctrl-Break, to the next handler, which terminates the process.
unsafe extern "system" fn record_interrupt(control: u32) -> windows_sys::core::BOOL {
    if control == CTRL_C_EVENT {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        1
    } else {
        0
    }
}

/// Records each Ctrl-C for [`interrupted`] instead of letting it terminate the process, as
/// Python's `SIGINT` handler, which raises `KeyboardInterrupt`, does. Ctrl-Break still
/// terminates it, as it terminates Python.
pub(crate) fn watch_interrupts() -> io::Result<()> {
    // SAFETY: the handler only stores to an atomic, and stays valid for the process's lifetime.
    if unsafe { SetConsoleCtrlHandler(Some(record_interrupt), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Returns whether Ctrl-C reached the console since [`watch_interrupts`].
pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}
