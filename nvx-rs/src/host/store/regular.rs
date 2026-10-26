//! The files that the template modules read, and that a sandbox's store reads as its clone
//! binding: regular files that are not links, which they read within a bound, and the identity of
//! a file that a template module holds open.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

use crate::host::seal::FileSeal;
use crate::host::{filesystem, platform};

/// Why a read of this module refused a file for what the file is, as [`is_refused`] tells.
#[derive(Debug)]
struct Refused(String);

impl fmt::Display for Refused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Returns whether `error` is one with which a read of this module refused a file for what the
/// file is, rather than an operation of the host's that failed: a link, or a file that is not a
/// regular file, which [`open_regular`] and [`open_regular_in`] refuse, and a file that holds
/// more than its bound, which [`read_at_most`] refuses.
pub(crate) fn is_refused(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Refused>())
}

/// Returns the error of a file that is a link, or not a regular file.
fn not_regular() -> io::Error {
    io::Error::other(Refused("it is a link, or not a regular file".to_owned()))
}

/// Returns whether `error`, with which an open or a removal of the file at `path` failed, means
/// that the file was removed, by a transaction of another process, say: on Windows, a file system
/// without POSIX deletion keeps a removed file under its name while other handles hold it, such as
/// a scanner's, and refuses every open of it until they close, which Win32 reports as a denied
/// access. So a denied access met a removed file if the file is deleted now, or its deletion is
/// pending, as [`platform::is_deleted`] tells, which a file whose access control list refuses the
/// access is not. No open reaches a removed file again, so it is gone for its readers, and for any
/// other removal of it.
pub(crate) fn was_removed(path: &Path, error: &io::Error) -> bool {
    cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied && platform::is_deleted(path)
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

/// Opens the file `name` of `directory` for reading, as [`open_regular`] opens the file at a
/// path, if it is a regular file, or returns `None` if the directory has no such entry, or, on
/// Windows, one whose deletion is pending, which is removed. The open names the entry through the
/// handle, so that it reaches the entry of the directory that the handle holds, whatever the
/// directory's path names meanwhile.
pub(crate) fn open_regular_in(
    directory: &platform::Directory,
    name: &str,
) -> io::Result<Option<File>> {
    match directory.open_file(name) {
        Ok(Some(file)) => regular(file).map(Some),
        Ok(None) => Ok(None),
        Err(error) => Err(refused_open(error)),
    }
}

/// Returns `error`, with which an open failed, as the error of a file that is not a regular file if
/// the open refused one: on Unix, a link, which `O_NOFOLLOW` refuses, or a socket, which an open
/// that does not wait refuses.
fn refused_open(error: io::Error) -> io::Error {
    #[cfg(unix)]
    if [nix::errno::Errno::ELOOP, nix::errno::Errno::ENXIO]
        .into_iter()
        .any(|errno| error.raw_os_error() == Some(errno as i32))
    {
        return not_regular();
    }
    error
}

/// Returns `file`, which an open that follows no link opened, if it is a regular file.
fn regular(file: File) -> io::Result<File> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || filesystem::is_reparse_point(&metadata) {
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

/// Returns the identity of the file that `file` holds open, as its [`FileSeal`] records it: its
/// device or volume, and its file ID, the 128-bit one of `FILE_ID_INFO` on Windows, which a file
/// system such as ReFS keeps unique where the 64-bit file index is not.
pub(crate) fn identity_of(file: &File) -> io::Result<(u64, u128)> {
    let seal = FileSeal::of(file)?;
    Ok((seal.device(), seal.file_id()))
}
