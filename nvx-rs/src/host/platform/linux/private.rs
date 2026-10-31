//! Directories and files that only the current user may access: their creation, the checks of
//! their owners and modes, and the sparse files and the renames without replacement of the
//! stores.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use nix::sys::stat::Mode;
use nix::unistd::Uid;

use super::{GROUP_AND_OTHER_BITS, PRIVATE_DIR_MODE, STORE_DIR_MODE};
use crate::host::sys::{self, Errno};

/// Creates `path` if needed and restricts it to the current user.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(PRIVATE_DIR_MODE.bits()).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a plain directory",
            path.display()
        )));
    }
    fs::set_permissions(path, Permissions::from_mode(PRIVATE_DIR_MODE.bits()))
}

/// Creates the directory `path` of a sandbox store with mode 0700, or checks that an existing one
/// is private to the current user, as [`check_store_dir`] does, and returns why it is not, if it
/// is not.
///
/// A new directory takes the setgid bit of its parent, and loses the bits that the umask clears,
/// even the current user's permission to read it, so its mode is set again, to the 0700 that
/// OpenVMM requires of the directory of its control socket. An existing directory is never
/// changed: others could have placed records in it, or still hold its files open, which a change
/// of its mode would not revoke.
pub(crate) fn create_store_dir(path: &Path) -> io::Result<Option<String>> {
    create_dir_of_mode(path, STORE_DIR_MODE)?;
    check_store_dir(path)
}

/// Creates the directory `path` with mode 0700, or checks that an existing one is private to the
/// current user, as [`check_owner_only_dir`] does, and returns why it is not, if it is not.
///
/// A new directory takes the setgid bit of its parent, and loses the bits that the umask clears,
/// even the current user's permission to read it, so its mode is set again. An existing directory
/// is never changed: others could have placed files in it, or still hold them open, which a
/// change of its mode would not revoke.
pub(crate) fn create_owner_only_dir(path: &Path) -> io::Result<Option<String>> {
    create_dir_of_mode(path, PRIVATE_DIR_MODE)?;
    check_owner_only_dir(path)
}

/// Creates the directory `path`, whose parent must exist, with exactly the mode `mode`, unless it
/// exists already, which leaves it as it is.
fn create_dir_of_mode(path: &Path, mode: Mode) -> io::Result<()> {
    match DirBuilder::new().mode(mode.bits()).create(path) {
        Ok(()) => {
            if let Some(directory) = open_plain_dir(path)? {
                // A descriptor that only locates the directory cannot change its mode itself, but
                // its link in /proc names exactly the directory that it opened.
                fs::set_permissions(
                    Path::new("/proc/self/fd").join(directory.as_raw_fd().to_string()),
                    Permissions::from_mode(mode.bits()),
                )?;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Returns the owner of the file that `metadata` describes and the permission bits of its mode:
/// those of the owner, group, and others, and the setuid, setgid, and sticky bits, without the
/// bits of the file's type.
///
/// The checks of ownership and permissions take these types, rather than the raw integers of
/// [`MetadataExt`], so that a mode passed as a user ID, or a user ID as a mode, fails to compile.
pub(super) fn owner_and_mode(metadata: &fs::Metadata) -> (Uid, Mode) {
    (
        Uid::from_raw(metadata.uid()),
        Mode::from_bits_truncate(metadata.mode()),
    )
}

/// Returns why the directory `path` cannot hold a sandbox store, if it cannot: unless it is
/// private to the current user, as [`create_store_dir`] makes it, belonging to the user with mode
/// 0700.
pub(crate) fn check_store_dir(path: &Path) -> io::Result<Option<String>> {
    let Some(directory) = open_plain_dir(path)? else {
        return Ok(Some("is not a plain directory".to_owned()));
    };
    let (uid, mode) = owner_and_mode(&directory.metadata()?);
    Ok(store_dir_problem(uid, mode, Uid::effective()))
}

/// Returns why the directory `path` is not private to the current user, if it is not: unless it
/// is a directory, not a link to one, that belongs to the user and grants no one else any access,
/// which [`create_owner_only_dir`] makes it.
pub(crate) fn check_owner_only_dir(path: &Path) -> io::Result<Option<String>> {
    let Some(directory) = open_plain_dir(path)? else {
        return Ok(Some("is not a plain directory".to_owned()));
    };
    let (uid, mode) = owner_and_mode(&directory.metadata()?);
    Ok(owner_only_problem(uid, mode, Uid::effective()))
}

/// Returns why a directory of a sandbox store, which the user `uid` owns with the permission bits
/// `mode`, is not private to the user `euid`, if it is not: it must belong to that user, grant no
/// access to anyone else, and have mode 0700, which OpenVMM requires of the directory of its
/// control socket.
pub(crate) fn store_dir_problem(uid: Uid, mode: Mode, euid: Uid) -> Option<String> {
    owner_only_problem(uid, mode, euid).or_else(|| {
        (mode != STORE_DIR_MODE).then(|| {
            format!(
                "has mode {mode:03o}, not the mode 700 that the directory of a control socket \
                 must have"
            )
        })
    })
}

/// Returns why a directory that the user `uid` owns with the permission bits `mode` is not private
/// to the user `euid`, if it is not: it must belong to that user, and grant no access to anyone
/// else.
pub(crate) fn owner_only_problem(uid: Uid, mode: Mode, euid: Uid) -> Option<String> {
    if uid != euid {
        Some(format!(
            "belongs to user {uid}, not to the current user {euid}"
        ))
    } else if mode.intersects(GROUP_AND_OTHER_BITS) {
        Some(format!("is accessible to other users (mode {mode:03o})"))
    } else {
        None
    }
}

/// Returns why the file that `file` holds open, the lock, a record, or the capability of a
/// sandbox store, is not private to the current user, if it is not: never, on Linux, where no
/// one else may look up a file in the store's directory, which grants them no access.
pub(crate) fn private_file_problem(_file: &File) -> io::Result<Option<String>> {
    Ok(None)
}

/// Returns why the directory `path`, a snapshot that OpenVMM published, is not private to the
/// current user, if it is not, as [`check_owner_only_dir`] does: OpenVMM creates it with mode 0700,
/// and its files with mode 0600.
pub(crate) fn check_snapshot_dir(path: &Path) -> io::Result<Option<String>> {
    check_owner_only_dir(path)
}

/// Returns why another user than the current one, or root, may change the file that `file` holds
/// open, if one may: unless it belongs to the current user or to root, and grants neither its
/// group nor others the permission to write it.
pub(crate) fn foreign_writer_problem(file: &File) -> io::Result<Option<String>> {
    let (uid, mode) = owner_and_mode(&file.metadata()?);
    Ok(foreign_writer(uid, mode, Uid::effective()))
}

/// Returns why another user than `euid`, or root, may change a file that the user `uid` owns with
/// the permission bits `mode`, if one may.
pub(crate) fn foreign_writer(uid: Uid, mode: Mode, euid: Uid) -> Option<String> {
    if uid != euid && !uid.is_root() {
        Some(format!(
            "belongs to user {uid}, neither the current user {euid} nor root"
        ))
    } else if mode.intersects(Mode::S_IWGRP | Mode::S_IWOTH) {
        Some(format!("is writable by other users (mode {mode:03o})"))
    } else {
        None
    }
}

/// Lets the regular file that `file` holds open for writing take no space for the blocks that
/// nobody writes: nothing to do on Linux, whose filesystems allocate no block that a file's
/// extension leaves unwritten.
pub(crate) fn make_sparse(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Renames `from` to `to` unless `to` exists, which it leaves as it is, failing with
/// [`io::ErrorKind::AlreadyExists`]: `renameat2` with `RENAME_NOREPLACE`, which a filesystem that
/// cannot rename without replacing refuses.
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    sys::rename_no_replace(from, to)
}

/// Gives the regular file `path` the mode `mode` if it belongs to the current user, and leaves
/// anything else, which includes a symbolic link or a missing file, unchanged.
///
/// The file is opened only to locate it, which needs no permission on the file itself.
pub(crate) fn restore_private_file_mode(path: &Path, mode: Mode) -> io::Result<()> {
    use nix::fcntl::OFlag;

    let opened = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_PATH | OFlag::O_NOFOLLOW).bits())
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::ELOOP as i32) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || Uid::from_raw(metadata.uid()) != Uid::effective() {
        return Ok(());
    }
    // A descriptor that only locates the file cannot change its mode itself, but its link in
    // /proc names exactly the file that it opened.
    fs::set_permissions(
        Path::new("/proc/self/fd").join(file.as_raw_fd().to_string()),
        Permissions::from_mode(mode.bits()),
    )
}

/// Opens the directory `path` only to locate it, which needs no permission on the directory
/// itself, or returns `None` if it is a symbolic link or not a directory.
fn open_plain_dir(path: &Path) -> io::Result<Option<File>> {
    use nix::fcntl::OFlag;

    let opened = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW).bits())
        .open(path);
    match opened {
        Ok(directory) => Ok(Some(directory)),
        Err(error)
            if error.raw_os_error() == Some(Errno::ELOOP as i32)
                || error.raw_os_error() == Some(Errno::ENOTDIR as i32) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
