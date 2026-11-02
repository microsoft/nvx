//! The standard streams, written as `nvx.py` writes them.
//!
//! Python's `print` ends lines with `os.linesep`, `\r\n` on Windows and `\n` elsewhere, and
//! translates every `\n` of the text it writes, so [`print_stdout`] and [`print_stderr`] do too.
//! Workload output passes through [`write_stdout`] and [`write_stderr`] unchanged, as
//! `sys.stdout.buffer.write` passes it.
//!
//! Like Python's writes, which raise `KeyboardInterrupt`, Ctrl-C ends a write to standard output
//! or to standard error, also one that waits for a pipe's reader, which may never read, except
//! [`print_stderr`], which reports how the command ended, Ctrl-C included. On Windows, Python
//! cannot end a write that waits either, so such a write runs to its end there.

use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::AsFd;

use crate::host::platform;

/// The line separator of this host, `os.linesep`.
pub(crate) const LINE_SEPARATOR: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// Returns `text` and a line end, with each `\n` translated to [`LINE_SEPARATOR`].
pub(crate) fn line(text: &str) -> String {
    let mut output = text.replace('\n', LINE_SEPARATOR);
    output.push_str(LINE_SEPARATOR);
    output
}

/// Prints `text` and a line end to standard output, and flushes it.
pub(crate) fn print_stdout(text: &str) -> io::Result<()> {
    write_stdout(line(text).as_bytes())
}

/// Prints `text` and a line end to standard error, even after Ctrl-C. Like Python, which prints
/// errors on a stream that it cannot write anyway, it ignores a failure.
pub(crate) fn print_stderr(text: &str) {
    let mut stderr = io::stderr().lock();
    let _ = stderr
        .write_all(line(text).as_bytes())
        .and_then(|()| stderr.flush());
}

/// Ends the line that `written`, a workload's output that went to standard error last, left
/// open, so that what [`print_stderr`] prints next starts a line of its own. It writes even after
/// Ctrl-C, as [`print_stderr`] does, and ignores a failure.
pub(crate) fn end_stderr_line(written: &[u8]) {
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
pub(crate) fn write_stdout(data: &[u8]) -> io::Result<()> {
    write_all(&mut io::stdout().lock(), data)
}

/// Writes `data` to standard error as it is, and flushes it.
pub(crate) fn write_stderr(data: &[u8]) -> io::Result<()> {
    write_all(&mut io::stderr().lock(), data)
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
