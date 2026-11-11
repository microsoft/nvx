//! The files that the template modules read, and that a sandbox's store reads as its clone
//! binding: regular files that are not links, which they read within a bound, and the identity of
//! a file that a template module holds open.
//!
//! The reads themselves are [`platform`]'s: this module re-exports its `open_regular` and
//! `read_at_most` for its callers until #1121 deletes them.

use std::fs::File;
use std::io;
use std::path::Path;

use crate::host::platform;
pub(crate) use crate::host::platform::{open_regular, read_at_most};
use crate::host::seal::FileSeal;

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
        Ok(Some(file)) => platform::regular(file).map(Some),
        Ok(None) => Ok(None),
        Err(error) => Err(platform::refused_open(error)),
    }
}

/// Returns the identity of the file that `file` holds open, as its [`FileSeal`] records it: its
/// device or volume, and its file ID, the 128-bit one of `FILE_ID_INFO` on Windows, which a file
/// system such as ReFS keeps unique where the 64-bit file index is not.
pub(crate) fn identity_of(file: &File) -> io::Result<(u64, u128)> {
    let seal = FileSeal::of(file)?;
    Ok((seal.device(), seal.file_id()))
}
