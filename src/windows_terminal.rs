// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Shared Windows terminal mode handling for VM console backends.

use ::windows::Win32::Foundation::HANDLE;
use ::windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_OUTPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleMode,
};

/// Restores terminal modes on drop after enabling raw input and ANSI output.
pub struct ConsoleGuard {
    stdin: Option<(HANDLE, CONSOLE_MODE)>,
    stdout: Option<(HANDLE, CONSOLE_MODE)>,
}

impl ConsoleGuard {
    pub fn new() -> Self {
        let stdin: Option<(HANDLE, CONSOLE_MODE)> = configure_console(STD_INPUT_HANDLE, |mode| {
            CONSOLE_MODE(mode.0 & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0))
        });
        let stdout: Option<(HANDLE, CONSOLE_MODE)> = configure_console(STD_OUTPUT_HANDLE, |mode| {
            CONSOLE_MODE(mode.0 | ENABLE_PROCESSED_OUTPUT.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0)
        });
        Self { stdin, stdout }
    }

    pub fn stdin_is_console(&self) -> bool {
        self.stdin.is_some()
    }
}

impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        for entry in [self.stdin.take(), self.stdout.take()]
            .into_iter()
            .flatten()
        {
            // SAFETY: `entry.0` is a console handle previously returned by `GetStdHandle`.
            unsafe {
                let _ = SetConsoleMode(entry.0, entry.1);
            }
        }
    }
}

fn configure_console(
    which: ::windows::Win32::System::Console::STD_HANDLE,
    update: impl FnOnce(CONSOLE_MODE) -> CONSOLE_MODE,
) -> Option<(HANDLE, CONSOLE_MODE)> {
    // SAFETY: All three console calls take a valid handle and a stack-allocated mode.
    unsafe {
        let handle: HANDLE = GetStdHandle(which).ok()?;
        let mut mode: CONSOLE_MODE = CONSOLE_MODE(0);
        GetConsoleMode(handle, &mut mode).ok()?;
        if SetConsoleMode(handle, update(mode)).is_ok() {
            Some((handle, mode))
        } else {
            None
        }
    }
}
