//! Safe path, file, mount, ownership, mode, and working-directory operations.

use std::ffi::{CStr, CString, OsStr};
use std::fmt::Display;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::str::FromStr;

pub(crate) use nix::mount::MsFlags as MountFlags;
use nix::sched::CloneFlags;
use nix::sys::stat::{FchmodatFlags, Mode};
use nix::unistd::{AccessFlags, Gid, Uid};

use super::{Errno, OpenFlags, errno_of};
use crate::guest::parse::parse_decimal;

/// Linux's `PATH_MAX`: the size of the longest path that the kernel takes, with its terminating
/// NUL.
pub(crate) const PATH_MAX: usize = nix::libc::PATH_MAX as usize;

/// Converts a Unix path to a NUL-terminated string, or `EINVAL` if it contains a NUL.
pub(crate) fn path_c_string(path: &Path) -> Result<CString, Errno> {
    super::c_string(path.as_os_str().as_bytes())
}

/// Returns `path` as the standard library's path.
fn std_path(path: &CStr) -> &Path {
    Path::new(OsStr::from_bytes(path.to_bytes()))
}

/// Reads the whole of the file at `path` through the standard library.
pub(crate) fn read_file(path: &CStr) -> Result<Vec<u8>, Errno> {
    std::fs::read(std_path(path)).map_err(|error| errno_of(&error))
}

/// Reads the first line of the file at `path`, without its newline, such as the value of a
/// sysfs attribute or a sysctl.
pub(crate) fn read_line(path: &CStr) -> Result<Vec<u8>, Errno> {
    let mut text = read_file(path)?;
    if let Some(end) = text.iter().position(|&byte| byte == b'\n') {
        text.truncate(end);
    }
    Ok(text)
}

/// Reads the number on the first line of the file at `path`, such as a sysctl's, between
/// optional whitespace, or fails with `EINVAL` for a file that holds anything else.
pub(crate) fn read_number<T: FromStr>(path: &CStr) -> Result<T, Errno> {
    let line = read_line(path)?;
    std::str::from_utf8(&line)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .ok_or(Errno::EINVAL)
}

/// Writes `text` to the existing file at `path`, as procfs and sysfs files take a value.
pub(crate) fn write_file(path: &CStr, text: &[u8]) -> Result<(), Errno> {
    OpenOptions::new()
        .write(true)
        .open(std_path(path))
        .and_then(|mut file| file.write_all(text))
        .map_err(|error| errno_of(&error))
}

/// Writes `value` and a newline to the existing file at `path`, as a sysctl takes a number.
pub(crate) fn write_number(path: &CStr, value: impl Display) -> Result<(), Errno> {
    write_file(path, format!("{value}\n").as_bytes())
}

/// Mounts `source` on `target`, with the file system's own options `data`, if any, such as an
/// overlay's layers.
pub(crate) fn mount(
    source: Option<&CStr>,
    target: &CStr,
    filesystem: Option<&CStr>,
    flags: MountFlags,
    data: Option<&CStr>,
) -> Result<(), Errno> {
    nix::mount::mount(source, target, filesystem, flags, data)
}

/// Unmounts the file system mounted at `target`. A busy one stays mounted: `EBUSY`.
pub(crate) fn unmount(target: &CStr) -> Result<(), Errno> {
    nix::mount::umount(target)
}

/// Returns the ID of the mount that `path`, after its symbolic links, is on, which procfs, at
/// `/proc/thread-self/fdinfo`, shows for a descriptor in the calling thread's table, as
/// `mountinfo` numbers the mounts. Two paths are on one mount exactly when their IDs are equal,
/// whatever file systems the mounts show: a bind mount of a directory shows the file system of
/// its source, but has an ID of its own.
pub(crate) fn mount_id(path: &CStr) -> Result<u64, Errno> {
    let descriptor = super::open(
        path,
        OpenFlags::O_PATH | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    let information = read_file(&super::c_string(
        format!("/proc/thread-self/fdinfo/{}", descriptor.raw()).as_bytes(),
    )?)?;
    information
        .split(|&byte| byte == b'\n')
        .find_map(|line| line.strip_prefix(b"mnt_id:"))
        .and_then(|value| parse_decimal(value.trim_ascii()))
        .ok_or(Errno::EINVAL)
}

/// Changes the owner and group of `path`, following the final symlink like C `chown`.
pub(crate) fn chown(path: &CStr, uid: u32, gid: u32) -> Result<(), Errno> {
    nix::unistd::chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
}

/// Changes the permission bits of `path`, following the final symlink like C `chmod`.
///
/// `fchmodat` relative to the working directory, without `AT_SYMLINK_NOFOLLOW`, is the system
/// call that `chmod` makes.
pub(crate) fn chmod(path: &CStr, mode: Mode) -> Result<(), Errno> {
    nix::sys::stat::fchmodat(
        nix::fcntl::AT_FDCWD,
        path,
        mode,
        FchmodatFlags::FollowSymlink,
    )
}

/// Checks that `path` exists with `access` and `F_OK`, which does not check permissions, and
/// returns `access`'s error when it does not.
pub(crate) fn check_exists(path: &CStr) -> Result<(), Errno> {
    nix::unistd::access(path, AccessFlags::F_OK)
}

/// Returns whether `path` exists; see [`check_exists`].
pub(crate) fn path_exists(path: &CStr) -> bool {
    check_exists(path).is_ok()
}

/// Moves the calling thread into a new mount namespace whose mounts, starting with the root's,
/// stop propagating to and from the namespace that it leaves, so that its mounts stay private.
pub(crate) fn enter_private_mount_namespace() -> Result<(), Errno> {
    nix::sched::unshare(CloneFlags::CLONE_NEWNS)?;
    nix::mount::mount(
        None::<&CStr>,
        c"/",
        None::<&CStr>,
        MountFlags::MS_REC | MountFlags::MS_PRIVATE,
        None::<&CStr>,
    )
}

/// A directory at a fixed path, which dropping the owner removes, best effort.
///
/// It also removes a directory that existed already, such as one that an interrupted earlier
/// owner left behind, which the owner then reuses.
#[derive(Debug)]
pub(crate) struct ScratchDirectory<'a>(&'a CStr);

impl<'a> ScratchDirectory<'a> {
    /// Creates `path` as a directory with `mode` unless it exists already, ignoring other
    /// failures as the C program did, because a later use of the directory reports them.
    pub(crate) fn create(path: &'a CStr, mode: Mode) -> Self {
        let _ = nix::unistd::mkdir(path, mode);
        Self(path)
    }

    /// Returns the directory's path.
    pub(crate) fn path(&self) -> &'a CStr {
        self.0
    }
}

impl Drop for ScratchDirectory<'_> {
    /// Removes the directory, which must be empty, best effort.
    fn drop(&mut self) {
        let _ = nix::unistd::unlinkat(
            nix::fcntl::AT_FDCWD,
            self.0,
            nix::unistd::UnlinkatFlags::RemoveDir,
        );
    }
}

/// A tmpfs mount, which dropping the owner unmounts, best effort, unless
/// [`unmount`](Self::unmount) did.
#[derive(Debug)]
pub(crate) struct TmpfsMount<'a>(Option<&'a CStr>);

impl<'a> TmpfsMount<'a> {
    /// Mounts a tmpfs named `source` on `target`, without set-user-ID files, device files, or
    /// executables, and with the filesystem's mount options `options`.
    pub(crate) fn mount(source: &CStr, target: &'a CStr, options: &CStr) -> Result<Self, Errno> {
        nix::mount::mount(
            Some(source),
            target,
            Some(c"tmpfs"),
            MountFlags::MS_NOSUID | MountFlags::MS_NODEV | MountFlags::MS_NOEXEC,
            Some(options),
        )?;
        Ok(Self(Some(target)))
    }

    /// Unmounts the filesystem, and reports a failure.
    pub(crate) fn unmount(mut self) -> Result<(), Errno> {
        self.0.take().map_or(Ok(()), nix::mount::umount)
    }
}

impl Drop for TmpfsMount<'_> {
    /// Unmounts the filesystem, best effort, unless [`unmount`](Self::unmount) did.
    fn drop(&mut self) {
        if let Some(target) = self.0.take() {
            let _ = nix::mount::umount(target);
        }
    }
}
