//! The terminal on which a guest agent serves its control session, such as the VM's control
//! console.
//!
//! [`Terminal::open`] opens the terminal for reading and writing, waiting while the kernel sets it
//! up during boot. [`Terminal::configure_raw`] puts it in raw mode, so that it passes the
//! session's bytes as they are, and [`Terminal::drain`] waits until it has sent what the agent
//! wrote, such as the confirmation of a stop, before the agent ends the VM. NVX's managed agent
//! serves its session on its control terminal through them, and any other guest agent can too:
//!
//! ```no_run
//! use std::io::Write;
//! use std::path::Path;
//!
//! use nvx::guest::terminal::Terminal;
//!
//! # fn main() -> std::io::Result<()> {
//! let mut terminal = Terminal::open(Path::new("/dev/hvc2"))?;
//! terminal.configure_raw()?;
//! // Serve the session on `terminal`, which reads and writes the terminal as a byte stream.
//! terminal.write_all(b"the session's bytes")?;
//! terminal.drain()?;
//! # Ok(())
//! # }
//! ```

use std::ffi::CStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::thread;
use std::time::Duration;

use super::sys::{self, Errno, FileDescriptor, Mode, OpenFlags};

/// How many times [`Terminal::open`] tries to open a terminal that the kernel is still setting
/// up.
const OPEN_ATTEMPTS: usize = 600;
/// The delay between those attempts, which span 30 seconds.
const OPEN_DELAY: Duration = Duration::from_millis(50);

/// A terminal, open for reading and writing.
///
/// It reads and writes as a byte stream through [`io::Read`] and [`io::Write`], with one system
/// call for each call and no buffer, so that a caller that serves a session on it, such as a
/// [`GuestConnection`](super::nvxs::GuestConnection), sees each byte as soon as it arrives.
/// Dropping it closes the terminal.
#[derive(Debug)]
pub struct Terminal(FileDescriptor);

impl Terminal {
    /// Opens the terminal at `path` for reading and writing, closed on `exec`.
    ///
    /// A terminal that the kernel has not set up yet during boot fails to open with `ENOENT`,
    /// `ENXIO`, `ENODEV`, or `EIO`, so `open` tries again every 50 milliseconds while it does, and
    /// fails with `ETIMEDOUT` once 30 seconds have passed. It fails at once with any other error,
    /// such as `EISDIR` for a directory, or `EINVAL` for a path that holds a NUL byte.
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = sys::c_string(path.as_os_str().as_bytes())?;
        Self::open_c(&path).map_err(io::Error::from)
    }

    /// Opens the terminal at `path` as [`open`](Self::open) does, and fails with the `errno`
    /// that NVX's managed agent reports.
    pub(crate) fn open_c(path: &CStr) -> Result<Self, Errno> {
        open_with(path, OPEN_ATTEMPTS, OPEN_DELAY).map(Self)
    }

    /// Puts the terminal in raw mode, as `cfmakeraw` defines it: without echo, line editing, or
    /// signal characters, without translation or flow control, and with 8-bit characters. The
    /// terminal also ignores the modem's status lines (`CLOCAL`), which a console does not have.
    /// The settings apply at once.
    ///
    /// Fails with `ENOTTY` if the file is no terminal.
    pub fn configure_raw(&self) -> io::Result<()> {
        sys::configure_raw(&self.0).map_err(io::Error::from)
    }

    /// Waits until the terminal has sent everything written to it.
    pub fn drain(&self) -> io::Result<()> {
        sys::drain(&self.0).map_err(io::Error::from)
    }

    /// Returns the terminal's descriptor, on which NVX's managed agent serves its session.
    pub(crate) fn into_descriptor(self) -> FileDescriptor {
        self.0
    }
}

impl io::Read for Terminal {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.0, bytes)
    }
}

impl io::Write for Terminal {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.0, bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.0)
    }
}

/// Opens the terminal at `path` for reading and writing, closed on `exec`, and tries `attempts`
/// times, `delay` apart, while it fails as a terminal that the kernel is still setting up does.
fn open_with(path: &CStr, attempts: usize, delay: Duration) -> Result<FileDescriptor, Errno> {
    for attempt in 0..attempts {
        match sys::open(
            path,
            OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => return Ok(descriptor),
            Err(Errno::ENOENT | Errno::ENXIO | Errno::ENODEV | Errno::EIO) => {}
            Err(error) => return Err(error),
        }
        if attempt + 1 != attempts {
            thread::sleep(delay);
        }
    }
    Err(Errno::ETIMEDOUT)
}

#[cfg(test)]
mod tests;
