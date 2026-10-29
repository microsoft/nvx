//! The files that the template modules read, and that a sandbox's store reads as its clone
//! binding: regular files that are not links, which they read within a bound, and the identities
//! by which the template modules and the stores tell files apart: of a file that one holds open,
//! and of the file that a path names.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

use crate::host::seal::FileSeal;
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
pub(crate) fn read_at_most(file: File, limit: u64, what: &str) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(format!(
            "it holds more than the {limit} bytes that {what} may hold"
        )));
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

/// Returns the identity of the file or directory that `path` names, following links, as
/// [`identity_of`] gives it for the object open.
///
/// On Windows, the object is opened without access rights, as [`platform::file_identity`] opens
/// it, which succeeds even while another process holds it open without sharing it. A file whose
/// deletion is pending, which a file system without POSIX deletion keeps under its name until its
/// last handle closes, refuses even that open, with a denied access.
pub(crate) fn identity_at(path: &Path) -> io::Result<(u64, u128)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(path)?;
        Ok((metadata.dev(), u128::from(metadata.ino())))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        /// `FILE_FLAG_BACKUP_SEMANTICS`, which opens a directory too.
        const BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let file = OpenOptions::new()
            .access_mode(0)
            .custom_flags(BACKUP_SEMANTICS)
            .open(path)?;
        identity_of(&file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        platform::file_identity(path).map(|(device, index)| (device, u128::from(index)))
    }
}
