//! Text lines for the standard streams and sandbox configuration, as `nvx.py` writes them.
//!
//! Python's `print` ends lines with `os.linesep`, `\r\n` on Windows and `\n` elsewhere, and
//! translates every `\n` of the text it writes, so [`print_stdout`] and [`print_stderr`] do too.
//! The output of a workload or of a guest's console passes through a [`ConsoleOutput`]
//! unchanged, as `sys.stdout.buffer.write` passes it, except to a Windows console, which shows
//! text: there each sequence of it that is not UTF-8 shows as U+FFFD, as CPython's console
//! streams show it.
//!
//! Like Python's writes, which raise `KeyboardInterrupt`, Ctrl-C ends a write to standard output
//! or to standard error, also one that waits for a pipe's reader, which may never read, except
//! [`print_stderr`], which reports how the command ended, Ctrl-C included. On Windows, Python
//! cannot end a write that waits either, so such a write runs to its end there.

#[cfg(windows)]
use std::io::IsTerminal;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::AsFd;

use crate::host::platform;

/// The line separator of this host, `os.linesep`.
const LINE_SEPARATOR: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// Returns `text` and a line end, with each `\n` translated to [`LINE_SEPARATOR`].
pub(super) fn line(text: &str) -> String {
    let mut output = text.replace('\n', LINE_SEPARATOR);
    output.push_str(LINE_SEPARATOR);
    output
}

/// Prints `text` and a line end to standard output, and flushes it.
pub(super) fn print_stdout(text: &str) -> io::Result<()> {
    write_stdout(line(text).as_bytes())
}

/// Prints `text` and a line end to standard error, even after Ctrl-C. Like Python, which prints
/// errors on a stream that it cannot write anyway, it ignores a failure.
pub(super) fn print_stderr(text: &str) {
    let mut stderr = io::stderr().lock();
    let _ = stderr
        .write_all(line(text).as_bytes())
        .and_then(|()| stderr.flush());
}

/// Ends the line that `written`, a workload's output that went to standard error last, left
/// open, so that what [`print_stderr`] prints next starts a line of its own. It writes even after
/// Ctrl-C, as [`print_stderr`] does, and ignores a failure.
pub(super) fn end_stderr_line(written: &[u8]) {
    let end = line_end_after(written);
    if !end.is_empty() {
        let mut stderr = io::stderr().lock();
        let _ = stderr
            .write_all(end.as_bytes())
            .and_then(|()| stderr.flush());
    }
}

/// Returns what ends the line that `written` left open: nothing after no output or a line end,
/// and [`LINE_SEPARATOR`] otherwise.
fn line_end_after(written: &[u8]) -> &'static str {
    match written.last() {
        Some(&byte) if byte != b'\n' => LINE_SEPARATOR,
        _ => "",
    }
}

/// Writes `data` to standard output as it is, and flushes it.
pub(super) fn write_stdout(data: &[u8]) -> io::Result<()> {
    write_all(&mut io::stdout().lock(), data)
}

/// Writes `data` to standard error as it is, and flushes it.
pub(super) fn write_stderr(data: &[u8]) -> io::Result<()> {
    write_all(&mut io::stderr().lock(), data)
}

/// Output to standard output or standard error, which reaches the stream as it is, unless the
/// stream is a Windows console.
///
/// A Windows console shows text, and the standard library refuses to write bytes that are not
/// UTF-8 to one whose output code page is not UTF-8, as by default. So to a Windows console, each
/// sequence that is not UTF-8 goes as U+FFFD, as CPython's console streams write it, and an
/// incomplete sequence that ends a write waits for the next, so that a character that two writes
/// split shows whole; [`finish`](Self::finish) writes one that the output ends with as U+FFFD.
pub(super) struct ConsoleOutput {
    /// Writes to the stream: [`write_stdout`] or [`write_stderr`].
    pub(super) write: fn(&[u8]) -> io::Result<()>,
    /// Whether the stream is a console.
    #[cfg(windows)]
    console: bool,
    /// The incomplete sequence that ended the last write to a console.
    #[cfg(windows)]
    pending: [u8; 3],
    /// The length of `pending`.
    #[cfg(windows)]
    pending_len: usize,
}

impl ConsoleOutput {
    /// Returns the output to standard output.
    pub(super) fn new() -> Self {
        Self {
            write: write_stdout,
            #[cfg(windows)]
            console: io::stdout().is_terminal(),
            #[cfg(windows)]
            pending: [0; 3],
            #[cfg(windows)]
            pending_len: 0,
        }
    }

    /// Returns the output to standard error.
    pub(super) fn stderr() -> Self {
        Self {
            write: write_stderr,
            #[cfg(windows)]
            console: io::stderr().is_terminal(),
            #[cfg(windows)]
            pending: [0; 3],
            #[cfg(windows)]
            pending_len: 0,
        }
    }

    /// Writes `data`, as the [type](Self) describes.
    pub(super) fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        #[cfg(windows)]
        if self.console {
            let combined;
            let bytes = if self.pending_len == 0 {
                data
            } else {
                combined = [&self.pending[..self.pending_len], data].concat();
                combined.as_slice()
            };
            // Only a final incomplete sequence crosses reads; other invalid bytes render now.
            let incomplete =
                bytes.utf8_chunks().last().map_or(0, |chunk| {
                    match std::str::from_utf8(chunk.invalid()) {
                        Err(error) if error.error_len().is_none() => chunk.invalid().len(),
                        _ => 0,
                    }
                });
            let ready = bytes.len() - incomplete;
            (self.write)(String::from_utf8_lossy(&bytes[..ready]).as_bytes())?;
            self.pending[..incomplete].copy_from_slice(&bytes[ready..]);
            self.pending_len = incomplete;
            return Ok(());
        }
        (self.write)(data)
    }

    /// Ends the output, writing an incomplete sequence that ended it on a console as U+FFFD.
    pub(super) fn finish(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        if self.pending_len != 0 {
            (self.write)("\u{fffd}".as_bytes())?;
            self.pending_len = 0;
        }
        Ok(())
    }
}

/// Writes `data` to `stream` after what it buffered, and fails with
/// [`io::ErrorKind::Interrupted`] once Ctrl-C has arrived, even while the stream waits for room.
#[cfg(unix)]
fn write_all(stream: &mut (impl Write + AsFd), data: &[u8]) -> io::Result<()> {
    stream.flush()?;
    platform::write_interruptibly(stream.as_fd(), data)
}

/// Writes `data` to `stream` and flushes it, unless Ctrl-C has arrived, in which case it fails
/// with [`io::ErrorKind::Interrupted`].
#[cfg(not(unix))]
fn write_all(stream: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if platform::interrupted() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
    }
    stream.write_all(data)?;
    stream.flush()
}

#[cfg(test)]
mod tests;
