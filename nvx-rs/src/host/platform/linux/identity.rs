//! The identities of files and directories, whether a file is deleted, and the open directories
//! through which operations name their entries.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use nix::sys::stat::Mode;
use nix::unistd::Uid;

use super::private::{owner_and_mode, store_dir_problem};
use crate::host::sys::Errno;

/// Returns the device and inode of a file or directory.
pub fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Returns whether the file at `path` is deleted: whether no file has that name, which a removal
/// takes at once.
pub(crate) fn is_deleted(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Returns the device and inode of the file that `file` holds open.
pub(crate) fn opened_file_identity(file: &File) -> io::Result<(u64, u64)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Returns the identity by which the stores tell a file or directory apart from every other,
/// following its links: its device and inode, as [`opened_file_id`] gives them.
pub(crate) fn file_id(path: &Path) -> io::Result<(u64, u128)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), u128::from(metadata.ino())))
}

/// Returns the identity by which the stores tell the file that `file` holds open apart from every
/// other: its device and inode, as its [`FileSeal`](crate::host::seal::FileSeal) holds them.
pub(crate) fn opened_file_id(file: &File) -> io::Result<(u64, u128)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), u128::from(metadata.ino())))
}

/// Returns the number of names, its hard links, of the file that `file` holds open.
pub(crate) fn link_count(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.nlink())
}

/// An open directory, through which operations name its entries, so that they reach the entries
/// of the directory that it opened, whatever its path names meanwhile.
pub(crate) struct Directory(File);

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

    /// Opens its entry `name` for reading, not following a link, and without waiting, as an open
    /// of a FIFO without a writer would, or returns `None` if it has no such entry. The caller
    /// checks what the entry is.
    pub(crate) fn open_file(&self, name: &str) -> io::Result<Option<File>> {
        use nix::fcntl::{OFlag, openat};

        match openat(
            &self.0,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => Ok(Some(File::from(file))),
            Err(Errno::ENOENT) => Ok(None),
            Err(errno) => Err(errno.into()),
        }
    }

    /// Opens the directory `path` with the right to search it alone, without following a link at
    /// its own name, though links on the way to it are followed, or returns `None` if it does not
    /// exist. Fails if it is a link, or not a directory.
    ///
    /// The handle names the directory's entries, and needs no right to read the directory, which
    /// an owner-only directory may withhold; it cannot [sync](Self::sync) it.
    pub(crate) fn search(path: &Path) -> io::Result<Option<Self>> {
        use nix::fcntl::OFlag;

        match OpenOptions::new()
            .read(true)
            .custom_flags((OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW).bits())
            .open(path)
        {
            Ok(dir) => Ok(Some(Self(dir))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error)
                if [Errno::ELOOP, Errno::ENOTDIR]
                    .map(|errno| Some(errno as i32))
                    .contains(&error.raw_os_error()) =>
            {
                Err(io::Error::other(format!(
                    "{} is a link, or not a directory",
                    path.display()
                )))
            }
            Err(error) => Err(error),
        }
    }

    /// Opens its subdirectory `name` with the right to search it alone, not following a link, as
    /// [`search`](Self::search) opens a directory, or returns `None` if it has no such entry.
    /// Fails if the entry is a link, or not a directory.
    pub(crate) fn search_dir(&self, name: &str) -> io::Result<Option<Self>> {
        self.open_dir_with(name, nix::fcntl::OFlag::O_PATH)
    }

    /// Opens its subdirectory `name` for reading, not following a link, or returns `None` if it
    /// has no such entry. Fails if the entry is a link, or not a directory.
    pub(crate) fn open_dir(&self, name: &str) -> io::Result<Option<Self>> {
        self.open_dir_with(name, nix::fcntl::OFlag::O_RDONLY)
    }

    /// Opens its subdirectory `name` with the access `access`, not following a link, as
    /// [`open_dir`](Self::open_dir) and [`search_dir`](Self::search_dir) do.
    fn open_dir_with(&self, name: &str, access: nix::fcntl::OFlag) -> io::Result<Option<Self>> {
        use nix::fcntl::{OFlag, openat};

        match openat(
            &self.0,
            name,
            access | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(dir) => Ok(Some(Self(File::from(dir)))),
            Err(Errno::ENOENT) => Ok(None),
            Err(Errno::ELOOP | Errno::ENOTDIR) => Err(io::Error::other(format!(
                "{name} is a link, or not a directory"
            ))),
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

    /// Returns why the directory cannot hold a sandbox store, as [`check_store_dir`] returns it for
    /// a path, if it cannot: unless it belongs to the current user with mode 0700.
    ///
    /// [`check_store_dir`]: crate::host::platform::linux::check_store_dir
    pub(crate) fn store_dir_problem(&self) -> io::Result<Option<String>> {
        let (uid, mode) = owner_and_mode(&self.0.metadata()?);
        Ok(store_dir_problem(uid, mode, Uid::effective()))
    }
}
