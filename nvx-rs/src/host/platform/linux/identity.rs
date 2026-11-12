//! The identities of files and directories, whether a file is deleted, and the open directories
//! through which operations name their entries.

use std::fs;
#[cfg(feature = "cli")]
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
#[cfg(feature = "cli")]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[cfg(feature = "cli")]
use crate::host::sys::Errno;

/// Returns the device and inode of a file or directory.
pub fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Returns whether the file at `path` is deleted: whether no file has that name, which a removal
/// takes at once.
#[cfg(feature = "cli")]
pub(crate) fn is_deleted(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Returns the device and inode of the file that `file` holds open.
#[cfg(feature = "cli")]
pub(crate) fn opened_file_identity(file: &File) -> io::Result<(u64, u64)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Returns the identity by which the stores tell a file or directory apart from every other,
/// following its links: its device and inode, as [`opened_file_id`] gives them.
#[cfg(feature = "cli")]
pub(crate) fn file_id(path: &Path) -> io::Result<(u64, u128)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), u128::from(metadata.ino())))
}

/// Returns the identity by which the stores tell the file that `file` holds open apart from every
/// other: its device and inode, as its [`FileSeal`](crate::host::seal::FileSeal) holds them.
#[cfg(feature = "cli")]
pub(crate) fn opened_file_id(file: &File) -> io::Result<(u64, u128)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), u128::from(metadata.ino())))
}

/// An open directory, through which operations name its entries, so that they reach the entries
/// of the directory that it opened, whatever its path names meanwhile.
#[cfg(feature = "cli")]
pub(crate) struct Directory(File);

#[cfg(feature = "cli")]
impl Directory {
    /// Opens the directory `path`, following its links.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        use nix::fcntl::OFlag;

        OpenOptions::new()
            .read(true)
            .custom_flags(OFlag::O_DIRECTORY.bits())
            .open(path)
            .map(Self)
    }

    /// Returns the identity of its entry `name`, not following a link, as [`opened_file_id`] gives
    /// it, or `None` if it has no such entry.
    pub(crate) fn entry_id(&self, name: &str) -> io::Result<Option<(u64, u128)>> {
        use nix::fcntl::AtFlags;
        use nix::sys::stat::fstatat;

        match fstatat(&self.0, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(status) => Ok(Some((status.st_dev, u128::from(status.st_ino)))),
            Err(Errno::ENOENT) => Ok(None),
            Err(errno) => Err(errno.into()),
        }
    }

    /// Removes its file `name`, if it has one.
    pub(crate) fn remove_file(&self, name: &str) -> io::Result<()> {
        use nix::unistd::{UnlinkatFlags, unlinkat};

        match unlinkat(&self.0, name, UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => Ok(()),
            Err(errno) => Err(errno.into()),
        }
    }

    /// Makes the changes to its entries so far durable, such as a file that was removed from it.
    pub(crate) fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}
