//! Operating-system primitives of NVX hosts.
//!
//! Each platform module provides the same functions. Linux and Windows are supported hosts;
//! other platforms compile but report NVX as unavailable.
//!
//! The crate-private `ProcessHandle` of each platform holds a process that `open_process` found
//! with its recorded identity, or a child that the caller has not reaped yet: a pidfd on Linux,
//! when the host provides them, and a process handle on Windows. Waits and termination go through
//! it, so they reach that process and never another that reuses its ID.

use std::io;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::hypervisor::Hypervisor;

pub(crate) mod inherit;
#[cfg(target_os = "linux")]
/// Linux implementations of host process, socket, and hypervisor primitives.
mod linux;
#[cfg(not(any(target_os = "linux", windows)))]
mod other;
#[cfg(windows)]
/// Windows implementations of host process, pipe, and hypervisor primitives.
#[allow(unsafe_code)]
mod windows;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(not(any(target_os = "linux", windows)))]
use other as imp;
#[cfg(windows)]
use windows as imp;

pub(crate) use imp::*;
pub use imp::{SUPPORTED, create_private_dir, file_identity, process_start_time};

/// Checks that `hypervisor` is present and accessible on this host: its device on Linux, and the
/// Windows Hypervisor Platform on Windows.
///
/// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) otherwise.
pub fn probe_hypervisor(hypervisor: Hypervisor) -> Result<()> {
    imp::probe_hypervisor(hypervisor).map_err(Error::unavailable)
}

/// Refuses a VMM working directory that exceeds Windows' process-creation limit, without
/// accessing the filesystem.
#[cfg(windows)]
pub(crate) fn check_working_dir(path: &std::path::Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::MAX_PATH;

    let absolute = std::path::absolute(path).map_err(|error| {
        Error::io(format!(
            "cannot resolve VMM working directory {}",
            path.display()
        ))
        .with_source(error)
    })?;
    // The bound is unchanged when the path already ends in a separator.
    let limit = MAX_PATH as usize - 2;
    if absolute.as_os_str().encode_wide().count() > limit {
        return Err(Error::unsupported(format!(
            "VMM working directory {} exceeds the Windows limit of {limit} UTF-16 code units, \
             including any verbatim prefix and trailing separator",
            absolute.display()
        )));
    }
    Ok(())
}

/// Byte stream to an OpenVMM control endpoint.
pub trait Transport: Send {
    /// Duplicates the connected endpoint for an adapter that uses native asynchronous I/O.
    ///
    /// Returns `Ok(None)` by default for transports without a native handle. The duplicate
    /// owns a handle to the same already peer-verified connection, not a new connection.
    /// The caller must drop this transport before using the duplicate or registering it with
    /// an I/O runtime, and must not change the shared blocking mode while this transport lives.
    #[cfg(any(target_os = "linux", windows))]
    fn try_clone_handle(&self) -> io::Result<Option<TransportHandle>> {
        Ok(None)
    }

    /// Reads at least one byte, waiting at most `timeout` (`None` waits indefinitely).
    ///
    /// Returns `Ok(0)` at end of stream and an [`io::ErrorKind::TimedOut`] error on timeout.
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize>;

    /// Writes all of `data` within `timeout` (`None` waits indefinitely), and fails with an
    /// [`io::ErrorKind::TimedOut`] error if it has not by then, however much of it the endpoint
    /// reads meanwhile.
    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()>;

    /// Returns what the platform tells of the data that this end wrote, and that the endpoint has
    /// not read yet, or `None` where it tells nothing.
    ///
    /// On Linux, it is what `SIOCOUTQ` reports of the Unix socket: the memory that the kernel
    /// charges the socket for the buffers that hold that data, each write's buffers of their own,
    /// until the endpoint read each buffer whole. The charge exceeds the data's bytes, and a read
    /// of part of a buffer leaves it as it was, so only data written a byte at a time shows the
    /// read of each byte. It is zero once the endpoint read everything. A named pipe tells its
    /// writer nothing of the kind, so on Windows, and by default, it is `None`.
    fn unread(&self) -> io::Result<Option<usize>> {
        Ok(None)
    }
}

/// An owned duplicate of a verified control socket connection.
#[cfg(target_os = "linux")]
pub type TransportHandle = std::os::fd::OwnedFd;

/// An owned duplicate of a verified, overlapped control pipe connection.
#[cfg(windows)]
pub type TransportHandle = std::os::windows::io::OwnedHandle;

/// Returns the time left until `deadline`, `None` without one, or an [`io::ErrorKind::TimedOut`]
/// error once it has passed.
#[cfg(any(target_os = "linux", windows))]
fn time_left(deadline: Option<std::time::Instant>) -> io::Result<Option<Duration>> {
    match deadline {
        None => Ok(None),
        Some(deadline) => deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .map(Some)
            .ok_or_else(|| io::ErrorKind::TimedOut.into()),
    }
}

/// Returns the outcome of ending a held process, given the `result` of signaling it: a failure
/// stands unless `exited` shows that the process has exited, which needs no ending.
///
/// Hosts refuse to signal some processes that exited: Windows those that this user may not
/// terminate or that already ended, and Linux another user's process until its parent reaps it.
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn unless_exited(
    result: io::Result<()>,
    exited: impl FnOnce() -> io::Result<bool>,
) -> io::Result<()> {
    let Err(error) = result else {
        return Ok(());
    };
    if exited()? { Ok(()) } else { Err(error) }
}

/// Parses exactly 32 hexadecimal digits, in either case, into the 16 bytes that they spell.
pub(crate) fn parse_hex_id(digits: &str) -> Option<[u8; 16]> {
    let digits = digits.as_bytes();
    if digits.len() != 32 {
        return None;
    }
    let mut id = [0; 16];
    for (byte, pair) in id.iter_mut().zip(digits.chunks_exact(2)) {
        let nibble = |digit: u8| char::from(digit).to_digit(16);
        *byte = u8::try_from(nibble(pair[0])? * 16 + nibble(pair[1])?).ok()?;
    }
    Some(id)
}

/// Parses 64 hexadecimal digits, in either case, into the 32 bytes that they spell.
pub(crate) fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
        let nibble = |digit: u8| char::from(digit).to_digit(16);
        *byte = u8::try_from(nibble(pair[0])? * 16 + nibble(pair[1])?).ok()?;
    }
    Some(bytes)
}

/// Parses the identity of a host as OpenVMM reads it: 32 hexadecimal digits, in either case,
/// between optional braces and surrounding whitespace, ignoring dashes, as in `/etc/machine-id`
/// and the Windows machine GUID.
#[cfg(any(target_os = "linux", windows))]
fn parse_host_id(text: &str) -> Option<[u8; 16]> {
    let digits: String = text
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .chars()
        .filter(|&character| character != '-')
        .collect();
    parse_hex_id(&digits)
}

#[cfg(test)]
pub(crate) mod tests;
