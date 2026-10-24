//! The files that the template modules read: regular files that are not links, which they read
//! within a bound.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

use crate::host::{filesystem, platform};

/// Returns the error of a file that is a link, or not a regular file.
fn not_regular() -> io::Error {
    io::Error::other("it is a link, or not a regular file")
}

/// Returns whether `error`, with which an open or a removal of the file at `path` failed, means
/// that the file was removed, by a transaction of another process, say: on Windows, a file system
/// without POSIX deletion keeps a removed file under its name while other handles hold it, such as
/// a scanner's, and refuses every open of it until they close, which Win32 reports as a denied
/// access. So a denied access met a removed file if the file is deleted now, or its deletion is
/// pending, as [`platform::is_deleted`] tells, which a file whose access control list refuses the
/// access is not. No open reaches a removed file again, so it is gone for its readers, and for any
/// other removal of it.
pub(super) fn was_removed(path: &Path, error: &io::Error) -> bool {
    cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied && platform::is_deleted(path)
}

/// Opens the file at `path` for reading, without following a link, or on Windows any other
/// reparse point, if it is a regular file.
///
/// The open does not wait, as it would on Unix for a FIFO without a writer: a file that is not a
/// regular file is refused before anything is read from it.
pub(super) fn open_regular(path: &Path) -> io::Result<File> {
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
    let file = match options.open(path) {
        Ok(file) => file,
        // `O_NOFOLLOW` refuses a link, and an open that does not wait refuses a socket.
        #[cfg(unix)]
        Err(error)
            if [nix::errno::Errno::ELOOP, nix::errno::Errno::ENXIO]
                .into_iter()
                .any(|errno| error.raw_os_error() == Some(errno as i32)) =>
        {
            return Err(not_regular());
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || filesystem::is_reparse_point(&metadata) {
        return Err(not_regular());
    }
    Ok(file)
}

/// Reads `file` to its end, failing if it holds more than `limit` bytes, the most that `what` may
/// hold, without reading more than one byte past the limit.
pub(super) fn read_at_most(file: File, limit: u64, what: &str) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(format!(
            "it holds more than the {limit} bytes that {what} may hold"
        )));
    }
    Ok(bytes)
}
