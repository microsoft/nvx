//! The terminal mode that the foreground console of `run` holds while it forwards input.

use std::io::{self, IsTerminal};

use crate::host::cli::{Exit, Result, output};

/// Keeps standard input, when it is a terminal, in raw mode with terminal interrupts, and restores
/// the terminal's entry mode when dropped, on success and error paths alike.
pub(super) struct TerminalGuard(Option<EntryMode>);

/// The console input mode on entry, which [`TerminalGuard`] restores exactly: on Windows,
/// crossterm's `disable_raw_mode` sets line input, echo and processed input instead of restoring
/// their entry values.
#[cfg(windows)]
type EntryMode = windows_sys::Win32::System::Console::CONSOLE_MODE;

/// Nothing: crossterm saves the terminal's entry mode, which its `disable_raw_mode` restores.
#[cfg(not(windows))]
struct EntryMode;

impl TerminalGuard {
    pub(super) fn new() -> Result<Self> {
        if !io::stdin().is_terminal() {
            return Ok(Self(None));
        }
        let entry = Self::entry_mode().map_err(Exit::from_os_error)?;
        crossterm::terminal::enable_raw_mode().map_err(Exit::from_os_error)?;
        let guard = Self(Some(entry));
        Self::enable_interrupts().map_err(Exit::from_os_error)?;
        Ok(guard)
    }

    /// Reads the console input mode that the guard restores.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn entry_mode() -> io::Result<EntryMode> {
        use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE};

        let mut mode = 0;
        // SAFETY: stdin is this process's console input, and `mode` outlives the call.
        if unsafe { GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(mode)
    }

    /// Restores the console input mode that [`Self::entry_mode`] read.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn restore(entry: EntryMode) -> io::Result<()> {
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};

        // SAFETY: stdin is this process's console input, whose entry mode the guard saved.
        if unsafe { SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), entry) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Reads nothing: crossterm saves the terminal's entry mode when it enables raw mode.
    #[cfg(not(windows))]
    fn entry_mode() -> io::Result<EntryMode> {
        Ok(EntryMode)
    }

    /// Restores the terminal mode that crossterm saved when it enabled raw mode.
    #[cfg(not(windows))]
    fn restore(_entry: EntryMode) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }

    #[cfg(unix)]
    fn enable_interrupts() -> io::Result<()> {
        use nix::sys::termios::{
            LocalFlags, SetArg, SpecialCharacterIndices, tcgetattr, tcsetattr,
        };

        let stdin = io::stdin();
        let mut mode = tcgetattr(&stdin)?;
        mode.local_flags.insert(LocalFlags::ISIG);
        // Only the interrupt key is a host signal; quit/suspend remain guest input.
        for key in [
            SpecialCharacterIndices::VQUIT,
            SpecialCharacterIndices::VSUSP,
            #[cfg(any(
                target_vendor = "apple",
                target_os = "freebsd",
                target_os = "dragonfly",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "solaris",
                target_os = "illumos",
                target_os = "aix"
            ))]
            SpecialCharacterIndices::VDSUSP,
        ] {
            mode.control_chars[key as usize] = nix::libc::_POSIX_VDISABLE as _;
        }
        tcsetattr(&stdin, SetArg::TCSANOW, &mode)?;
        Ok(())
    }

    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn enable_interrupts() -> io::Result<()> {
        use windows_sys::Win32::System::Console::{
            ENABLE_PROCESSED_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
        };

        let mut mode = 0;
        // SAFETY: stdin is this process's terminal, whose saved entry mode the guard restores.
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            if GetConsoleMode(input, &mut mode) == 0
                || SetConsoleMode(input, mode | ENABLE_PROCESSED_INPUT) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn enable_interrupts() -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(entry) = self.0.take()
            && let Err(error) = Self::restore(entry)
        {
            output::print_stderr(&format!("cannot restore terminal mode: {error}"));
        }
    }
}
