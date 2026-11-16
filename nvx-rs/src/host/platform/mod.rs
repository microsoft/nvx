//! Operating-system primitives of NVX hosts.
//!
//! Each platform module provides the same functions. Linux and Windows are supported hosts;
//! other platforms compile but report NVX as unavailable.
//!
//! The private `hypervisor` module defines the [`Hypervisor`](crate::host::runtime::Hypervisor)
//! that the platforms' probes take, and that [`host::runtime`](crate::host::runtime) exports.
//!
//! The crate-private `ProcessHandle` of each platform holds a process that `open_process` found
//! with its recorded identity, or a child that the caller has not reaped yet: a pidfd on Linux,
//! when the host provides them, and a process handle on Windows. Waits and termination go through
//! it, so they reach that process and never another that reuses its ID.
//!
//! Its crate-private reads of regular files open a file without following a link, or on Windows
//! any other reparse point, refuse one that is not a regular file before reading anything from
//! it, and read it within a bound.
//!
//! Its crate-private writes create files that only the current user may read, replace a file
//! atomically, and make the changes to a directory's entries durable.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};

pub(crate) mod hypervisor;
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

/// Why this module refused a file for what the file is.
#[derive(Debug)]
struct Refused(String);

impl fmt::Display for Refused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Returns the error of a file that is a link, or not a regular file.
fn not_regular() -> io::Error {
    io::Error::other(Refused("it is a link, or not a regular file".to_owned()))
}

/// Opens the file at `path` for reading, without following a link, or on Windows any other
/// reparse point, if it is a regular file.
///
/// The open does not wait, as it would on Unix for a FIFO without a writer: a file that is not a
/// regular file is refused before anything is read from it.
pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // Reads of a regular file do not depend on `O_NONBLOCK`.
        options
            .custom_flags((nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_NONBLOCK).bits());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        /// `FILE_FLAG_OPEN_REPARSE_POINT`, which opens a link as itself.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(refused_open)?;
    regular(file)
}

/// Returns `error`, with which an open failed, as the error of a file that is not a regular file if
/// the open refused one: on Unix, a link, which `O_NOFOLLOW` refuses, or a socket, which an open
/// that does not wait refuses.
pub(crate) fn refused_open(error: io::Error) -> io::Error {
    #[cfg(unix)]
    if [nix::errno::Errno::ELOOP, nix::errno::Errno::ENXIO]
        .into_iter()
        .any(|errno| error.raw_os_error() == Some(errno as i32))
    {
        return not_regular();
    }
    error
}

#[cfg(windows)]
/// Returns whether Windows marks `metadata` as a reparse point.
pub(crate) fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
/// Reports no reparse points on hosts whose metadata has no such Windows attribute.
pub(crate) fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

/// Returns `file`, which an open that follows no link opened, if it is a regular file.
pub(crate) fn regular(file: File) -> io::Result<File> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || is_reparse_point(&metadata) {
        return Err(not_regular());
    }
    Ok(file)
}

/// Reads `file` to its end, failing if it holds more than `limit` bytes, the most that `what` may
/// hold, without reading more than one byte past the limit.
pub(crate) fn read_at_most(file: File, limit: u64, what: &str) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(Refused(format!(
            "it holds more than the {limit} bytes that {what} may hold"
        ))));
    }
    Ok(bytes)
}

/// Mode of a private file on Unix: readable and writable by the current user only.
#[cfg(unix)]
pub(crate) const PRIVATE_FILE_MODE: u32 = 0o600;

/// Returns options that create a file readable only by the current user (mode 0600 on Unix)
/// and that open no link that its path names on Unix.
pub(crate) fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    options
}

/// Creates the file at `path`, which must not exist, readable only by the current user.
///
/// On Unix, the umask may clear bits of the mode that the file is created with, even the
/// current user's permission to read it, so the mode is set again through the new file.
pub(crate) fn create_new(path: &Path) -> io::Result<File> {
    let file = private_options().create_new(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    }
    Ok(file)
}

/// Atomically replaces `path` with `contents`, readable only by the current user.
///
/// The contents go to a fresh temporary file beside `path`, which is flushed to disk and then
/// renamed over it, so a reader sees the old contents or the new ones, and a crash leaves at most
/// the temporary file.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix)
        .map_err(|error| Error::other("cannot name a temporary file").with_source(error))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", u64::from_le_bytes(suffix)));
    let written = (|| -> io::Result<()> {
        let mut file = create_new(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
        .map_err(|error| Error::io(format!("cannot write {}", path.display())).with_source(error))
}

/// Makes the changes to the entries of the directory `dir` so far durable, such as files that
/// were renamed into it or removed from it: a crash that keeps a later change keeps them too, even
/// one on another filesystem.
///
/// On Unix, it syncs the directory. Windows has no portable way to flush a directory, so there a
/// rename or a removal is as durable as the filesystem makes it, as OpenVMM's publication of a
/// snapshot assumes.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                Error::io(format!("cannot sync the directory {}", dir.display())).with_source(error)
            })
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
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
pub(crate) trait Transport: Send {
    /// Duplicates the connected endpoint, for the CLI, which reads and writes it on separate
    /// threads, or ends its pending transfers from another thread.
    ///
    /// Returns `Ok(None)` by default for transports without a native handle. The duplicate
    /// owns a handle to the same already peer-verified connection, not a new connection.
    /// The caller must drop this transport before reading or writing through the duplicate, and
    /// must not change the shared blocking mode while this transport lives.
    #[cfg(all(any(target_os = "linux", windows), feature = "cli"))]
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

    /// Returns the connected socket, whose readiness tells when the endpoint sent data, or `None`
    /// for transports without one.
    #[cfg(target_os = "linux")]
    fn as_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        None
    }
}

/// An owned duplicate of a verified control socket connection.
#[cfg(target_os = "linux")]
pub(crate) type TransportHandle = std::os::fd::OwnedFd;

/// An owned duplicate of a verified, overlapped control pipe connection.
#[cfg(windows)]
pub(crate) type TransportHandle = std::os::windows::io::OwnedHandle;

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
