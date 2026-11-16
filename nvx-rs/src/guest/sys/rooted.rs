//! Paths that resolve beneath a directory's descriptor, files and directories created there, the
//! kinds, owners, modes, and mounts of open files, and changes of their modes, the roots of
//! mounts, and mounts that are prepared before they are attached at a descriptor.
//!
//! `nix` wraps `openat2`, `mkdirat`, and `umount2`. It wraps neither `statx`, whose structure
//! and flags its `libc` dependency defines for musl only from musl 1.2.3 on, nor the mount API's
//! `open_tree`, `mount_setattr`, `move_mount`, and `statmount`, so this module calls them
//! through libc's `syscall`, with the structures and flags of the kernel's UAPI headers
//! `linux/stat.h` and `linux/mount.h`. The unique mount IDs of `statx`, and `statmount`, need
//! Linux 6.8 or later.

use std::ffi::{CStr, c_uint};
use std::mem::size_of;
use std::ops::BitOr;
use std::os::fd::RawFd;

use nix::fcntl::{OpenHow, ResolveFlag};
use nix::mount::MntFlags;
use nix::sys::stat::FchmodatFlags;

use super::{Errno, FileDescriptor, Mode, OpenFlags};

/// `statx`'s `STATX_TYPE`: the file's type, in `stx_mode`.
const STATX_TYPE: c_uint = 0x0001;
/// `statx`'s `STATX_MODE`: the file's permissions, in `stx_mode`.
const STATX_MODE: c_uint = 0x0002;
/// `statx`'s `STATX_UID`: the file's owner, in `stx_uid`.
const STATX_UID: c_uint = 0x0008;
/// `statx`'s `STATX_MNT_ID_UNIQUE`: the unique ID of the file's mount, which the kernel never
/// reuses, in `stx_mnt_id`, from Linux 6.8 on.
const STATX_MNT_ID_UNIQUE: c_uint = 0x4000;
/// `statx`'s `STATX_ATTR_MOUNT_ROOT`: the file is the root of its mount, from Linux 5.8 on.
const STATX_ATTR_MOUNT_ROOT: u64 = 0x2000;
/// `statx`'s `AT_STATX_SYNC_AS_STAT`: the attributes that `stat` would return.
const AT_STATX_SYNC_AS_STAT: c_uint = 0;
/// `open_tree`'s `OPEN_TREE_CLONE`: a detached copy of the mount, rather than the mount itself.
const OPEN_TREE_CLONE: c_uint = 1;
/// `open_tree`'s `OPEN_TREE_CLOEXEC`, which is `O_CLOEXEC`.
const OPEN_TREE_CLOEXEC: c_uint = nix::libc::O_CLOEXEC as c_uint;
/// `move_mount`'s `MOVE_MOUNT_F_EMPTY_PATH`: the mount to move is the descriptor's own.
const MOVE_MOUNT_F_EMPTY_PATH: c_uint = 0x0004;
/// `move_mount`'s `MOVE_MOUNT_T_EMPTY_PATH`: the mount goes onto the descriptor's own path.
const MOVE_MOUNT_T_EMPTY_PATH: c_uint = 0x0040;
/// `statmount`'s number in the kernel's common table of system calls, which every architecture
/// but Alpha and MIPS numbers its calls by from Linux 6.8 on; `libc` defines it for none of the
/// guests' targets.
const SYS_STATMOUNT: nix::libc::c_long = 457;
/// `statmount`'s `STATMOUNT_MNT_ROOT`: the path of the mount's root in its file system, in
/// `mnt_root`.
const STATMOUNT_MNT_ROOT: u64 = 0x0008;
/// `MNT_ID_REQ_SIZE_VER0`: the size of the first `struct mnt_id_req`, of Linux 6.8, which every
/// later kernel accepts too.
const MNT_ID_REQ_SIZE_VER0: u32 = 24;
/// The room for the strings after a `struct statmount`, which the root's path, `/`, and the
/// empty string that later kernels put first, fit in many times over.
const STATMOUNT_STRINGS: usize = 256;
/// The directory of the calling thread's descriptors in procfs, whose entries lead to the files
/// that the descriptors opened: the thread's own table, which `/proc/self/fd` does not show once
/// the thread has a table of its own, as after `unshare(CLONE_FILES)`, since that directory is
/// the table of the process's first thread.
const DESCRIPTORS: &str = "/proc/thread-self/fd";

/// The layout of `struct statx` in `linux/stat.h`, 256 bytes, which the kernel fills.
#[repr(C)]
#[derive(Default)]
struct Statx {
    /// `stx_mask`: the fields that the kernel filled.
    mask: u32,
    /// `stx_blksize`.
    _block_size: u32,
    /// `stx_attributes`: the file's attribute flags.
    attributes: u64,
    /// `stx_nlink`.
    _links: u32,
    /// `stx_uid`: the file's owner.
    uid: u32,
    /// `stx_gid`.
    _group: u32,
    /// `stx_mode`: the file's type and permissions.
    mode: u16,
    /// `__spare0`.
    _spare: u16,
    /// `stx_ino`, `stx_size`, and `stx_blocks`.
    _numbers: [u64; 3],
    /// `stx_attributes_mask`: the attribute flags that the file system supports.
    attributes_mask: u64,
    /// `stx_atime`, `stx_btime`, `stx_ctime`, and `stx_mtime`, of 16 bytes each.
    _times: [u64; 8],
    /// `stx_rdev_major`, `stx_rdev_minor`, `stx_dev_major`, and `stx_dev_minor`.
    _devices: [u32; 4],
    /// `stx_mnt_id`: the ID of the file's mount, unique with `STATX_MNT_ID_UNIQUE`.
    mount_id: u64,
    /// The fields after `stx_mnt_id`, and the kernel's spare space.
    _reserved: [u64; 13],
}

const _: () = assert!(size_of::<Statx>() == 0x100);
const _: () = assert!(std::mem::offset_of!(Statx, attributes_mask) == 0x38);
const _: () = assert!(std::mem::offset_of!(Statx, mount_id) == 0x90);

/// The layout of the first `struct mnt_id_req` in `linux/mount.h`, which `statmount` reads.
#[repr(C)]
struct MountIdRequest {
    /// `size`: the structure's size, [`MNT_ID_REQ_SIZE_VER0`].
    size: u32,
    /// `spare`, which must be 0.
    spare: u32,
    /// `mnt_id`: the unique ID of the mount.
    mount_id: u64,
    /// `param`: the fields that the caller wants, `STATMOUNT_*`.
    param: u64,
}

const _: () = assert!(size_of::<MountIdRequest>() == MNT_ID_REQ_SIZE_VER0 as usize);

/// The layout of `struct statmount` in `linux/mount.h`, 512 bytes, which the kernel fills, with
/// the room for its strings after it.
#[repr(C)]
struct Statmount {
    /// `size` and `mnt_opts`.
    _size: [u32; 2],
    /// `mask`: the fields that the kernel filled.
    mask: u64,
    /// The fields from `sb_dev_major` to `propagate_from`.
    _fields: [u64; 11],
    /// `mnt_root`: the offset in `strings` of the path of the mount's root in its file system.
    mount_root: u32,
    /// `mnt_point`.
    _mount_point: u32,
    /// The fields after `mnt_point`, and the kernel's spare space.
    _reserved: [u64; 50],
    /// `str`: the strings, each terminated by a NUL.
    strings: [u8; STATMOUNT_STRINGS],
}

const _: () = assert!(std::mem::offset_of!(Statmount, mount_root) == 104);
const _: () = assert!(std::mem::offset_of!(Statmount, strings) == 512);

/// The layout of `struct mount_attr` in `linux/mount.h`, which `mount_setattr` reads.
#[repr(C)]
struct MountAttr {
    /// `attr_set`: the attributes to set.
    attr_set: u64,
    /// `attr_clr`: the attributes to clear.
    attr_clr: u64,
    /// `propagation`: the propagation type to set, or 0 to leave it.
    propagation: u64,
    /// `userns_fd`: the user namespace of an ID-mapped mount, unused without `MOUNT_ATTR_IDMAP`.
    userns_fd: u64,
}

/// What kind of object an open file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FileKind {
    /// A directory.
    Directory,
    /// A regular file.
    Regular,
    /// Any other kind, such as a symbolic link, a device, a FIFO, or a socket.
    Other,
}

/// What `statx` tells of an open file: its kind, its owner, its mode, its mount, and whether it
/// is the mount's root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileStatus {
    /// The file's kind.
    pub(crate) kind: FileKind,
    /// The file's owner, its user ID.
    pub(crate) owner: u32,
    /// The file's mode without its type: its permissions, and its set-user-ID, set-group-ID, and
    /// sticky bits.
    pub(crate) mode: u32,
    /// The unique ID of the file's mount. A bind mount has an ID of its own, though it shows the
    /// file system, and the device, of its source.
    pub(crate) mount: u64,
    /// Whether the file is the root of its mount, such as a directory or a file that a mount
    /// covers.
    pub(crate) mount_root: bool,
}

/// The attributes of a mount that [`set_mount_attributes`] sets or clears: `MOUNT_ATTR_*` in
/// `linux/mount.h`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct MountAttributes(u64);

impl MountAttributes {
    /// `MOUNT_ATTR_RDONLY`: nothing is written through the mount.
    pub(crate) const READ_ONLY: Self = Self(0x1);
    /// `MOUNT_ATTR_NOSUID`: set-user-ID and set-group-ID bits take no effect.
    pub(crate) const NO_SET_ID: Self = Self(0x2);
    /// `MOUNT_ATTR_NODEV`: device nodes cannot be opened.
    pub(crate) const NO_DEVICES: Self = Self(0x4);

    /// Returns no attributes.
    pub(crate) const fn empty() -> Self {
        Self(0)
    }
}

impl BitOr for MountAttributes {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Opens `path` beneath the directory that `directory` refers to, with `openat2`, adding
/// `O_CLOEXEC` to `flags`, and creating a file with `mode` when `flags` hold `O_CREAT`.
///
/// The resolution never leaves the directory, through `..` or an absolute path, and follows no
/// symbolic link and no magic link, such as procfs's `/proc/self/root`, at any component, the
/// last included: such a path fails with `EXDEV` or `ELOOP`. It crosses into the mounts below
/// the directory only when `cross_mounts` is set, and fails with `EXDEV` otherwise.
///
/// A path with a `..` after a descent, such as `a/..`, can also fail with `EAGAIN` while a
/// rename or a mount anywhere in the system races its lookup: the kernel can then not tell that
/// the `..` stayed beneath the directory, and the caller may retry, as openat2(2) allows. No
/// caller in NVX passes such a path.
pub(crate) fn open_beneath(
    directory: &FileDescriptor,
    path: &CStr,
    flags: OpenFlags,
    mode: Mode,
    cross_mounts: bool,
) -> Result<FileDescriptor, Errno> {
    let mut resolve = ResolveFlag::RESOLVE_BENEATH
        | ResolveFlag::RESOLVE_NO_SYMLINKS
        | ResolveFlag::RESOLVE_NO_MAGICLINKS;
    if !cross_mounts {
        resolve |= ResolveFlag::RESOLVE_NO_XDEV;
    }
    let how = OpenHow::new()
        .flags(flags | OpenFlags::O_CLOEXEC)
        .mode(mode)
        .resolve(resolve);
    nix::fcntl::openat2(directory.borrowed(), path, how).map(FileDescriptor::from_owned)
}

#[cfg(target_arch = "x86_64")]
impl FileDescriptor {
    /// Opens a directory inside this root, following ordinary ancestor links but not a final
    /// link or a magic link. Absolute links and `..` remain inside this root.
    pub(crate) fn open_directory_in_root(&self, path: &CStr) -> Result<Self, Errno> {
        let mut bytes = path.to_bytes();
        while bytes.len() > 1 && bytes.ends_with(b"/") {
            bytes = &bytes[..bytes.len() - 1];
        }
        let path = super::c_string(bytes)?;
        let how = OpenHow::new()
            .flags(
                OpenFlags::O_PATH
                    | OpenFlags::O_DIRECTORY
                    | OpenFlags::O_NOFOLLOW
                    | OpenFlags::O_CLOEXEC,
            )
            .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_MAGICLINKS);
        nix::fcntl::openat2(self.borrowed(), path.as_c_str(), how).map(Self::from_owned)
    }

    /// Replaces one regular file of this directory atomically, retaining its owner and mode.
    /// No path component or temporary file is resolved against the process's working directory.
    pub(crate) fn replace_file(&self, name: &CStr, bytes: &[u8]) -> Result<(), Errno> {
        use nix::sys::stat::SFlag;
        use nix::unistd::{Gid, Uid};

        if name.to_bytes().contains(&b'/') || matches!(name.to_bytes(), b"" | b"." | b"..") {
            return Err(Errno::EINVAL);
        }
        let original = open_beneath(
            self,
            name,
            OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_NOCTTY,
            Mode::empty(),
            false,
        )?;
        let status = nix::sys::stat::fstat(original.borrowed())?;
        if SFlag::from_bits_truncate(status.st_mode) != SFlag::S_IFREG {
            return Err(Errno::EINVAL);
        }
        let random = super::open(
            c"/dev/urandom",
            OpenFlags::O_RDONLY | OpenFlags::O_NOFOLLOW | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        let mut nonce = [0; 8];
        random.read_exact(&mut nonce)?;
        let temporary_name =
            super::c_string(format!(".nvx-{:016x}", u64::from_ne_bytes(nonce)).as_bytes())?;
        let temporary = open_beneath(
            self,
            &temporary_name,
            OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_EXCL,
            Mode::from_bits_truncate(0o600),
            false,
        )?;
        let renamed = (|| {
            temporary.write_all(bytes)?;
            nix::unistd::fchown(
                temporary.borrowed(),
                Some(Uid::from_raw(status.st_uid)),
                Some(Gid::from_raw(status.st_gid)),
            )?;
            temporary.set_mode(Mode::from_bits_truncate(status.st_mode))?;
            temporary.sync()?;
            temporary.close()?;
            nix::fcntl::renameat(
                self.borrowed(),
                temporary_name.as_c_str(),
                self.borrowed(),
                name,
            )
        })();
        if renamed.is_err() {
            // The original error wins; unlinking only our exclusive sibling is best effort.
            let _ = nix::unistd::unlinkat(
                self.borrowed(),
                temporary_name.as_c_str(),
                nix::unistd::UnlinkatFlags::NoRemoveDir,
            );
        }
        renamed?;
        self.sync()
    }
}

/// Returns the kind of the file that `descriptor` opened, its owner and mode, its mount's unique
/// ID, and whether it is the mount's root, from `statx`.
///
/// It fails with `EOPNOTSUPP` on a kernel that does not report them all, such as one before
/// Linux 6.8, which has no unique mount IDs.
pub(crate) fn file_status(descriptor: &FileDescriptor) -> Result<FileStatus, Errno> {
    let mut status = Statx::default();
    let flags = nix::libc::AT_EMPTY_PATH as c_uint | AT_STATX_SYNC_AS_STAT;
    let wanted = STATX_TYPE | STATX_MODE | STATX_UID | STATX_MNT_ID_UNIQUE;
    // SAFETY: `statx` reads the NUL-terminated empty path and writes at most one `struct statx`
    // to `status`, which has its layout and outlives the call.
    Errno::result(unsafe {
        nix::libc::syscall(
            nix::libc::SYS_statx,
            descriptor.raw(),
            c"".as_ptr(),
            flags,
            wanted,
            &raw mut status,
        )
    })?;
    if status.mask & wanted != wanted || status.attributes_mask & STATX_ATTR_MOUNT_ROOT == 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let mode = nix::libc::mode_t::from(status.mode);
    let kind = match mode & nix::libc::S_IFMT {
        nix::libc::S_IFDIR => FileKind::Directory,
        nix::libc::S_IFREG => FileKind::Regular,
        _ => FileKind::Other,
    };
    Ok(FileStatus {
        kind,
        owner: status.uid,
        mode: mode & !nix::libc::S_IFMT,
        mount: status.mount_id,
        mount_root: status.attributes & STATX_ATTR_MOUNT_ROOT != 0,
    })
}

/// Creates the directory `name` in the directory that `directory` refers to, with `mode`, but
/// for the permissions that the calling thread's umask, or a default ACL of the directory,
/// withholds. A symbolic link at `name` is not followed: the call fails with `EEXIST`.
pub(crate) fn create_directory_at(
    directory: &FileDescriptor,
    name: &CStr,
    mode: Mode,
) -> Result<(), Errno> {
    nix::sys::stat::mkdirat(directory.borrowed(), name, mode)
}

/// Creates the empty regular file `name` in the directory that `directory` refers to, with
/// exactly `mode`, whatever the calling thread's umask or a default ACL of the directory, which
/// it sets through the descriptor that the creation returns: a descriptor of that file alone,
/// not of one that a rename put in its place since. It resolves `name` as [`open_beneath`] does,
/// without crossing into another mount, and fails with `EEXIST` on any entry of that name, a
/// symbolic link included.
pub(crate) fn create_file_at(
    directory: &FileDescriptor,
    name: &CStr,
    mode: Mode,
) -> Result<(), Errno> {
    let flags = OpenFlags::O_RDONLY | OpenFlags::O_CREAT | OpenFlags::O_EXCL;
    let file = open_beneath(directory, name, flags, mode, false)?;
    nix::sys::stat::fchmod(file.borrowed(), mode)
}

/// Sets the mode of the file that `descriptor` refers to, which may be an `O_PATH` descriptor,
/// to `mode`.
///
/// `fchmod` takes no `O_PATH` descriptor, so the call names the file through the descriptor's
/// entry in procfs, which leads to exactly that file, whatever path names it now; procfs must be
/// mounted at `/proc`.
pub(crate) fn change_mode(descriptor: &FileDescriptor, mode: Mode) -> Result<(), Errno> {
    let path = super::c_string(format!("{DESCRIPTORS}/{}", descriptor.raw()).as_bytes())?;
    let follow = FchmodatFlags::FollowSymlink;
    nix::sys::stat::fchmodat(nix::fcntl::AT_FDCWD, path.as_c_str(), mode, follow)
}

/// Returns whether the mount whose unique ID is `mount`, in the caller's mount namespace, shows
/// its whole file system: whether its root, from `statmount`, is that file system's root, `/`,
/// rather than a directory or a file below it, as the root of a bind of one is.
///
/// It fails with `ENOENT` for a mount that is not in the caller's mount namespace, such as one
/// that was detached, and with `EPERM` for one that the caller's root does not reach, unless it
/// has `CAP_SYS_ADMIN`.
pub(crate) fn is_whole_file_system(mount: u64) -> Result<bool, Errno> {
    let request = MountIdRequest {
        size: MNT_ID_REQ_SIZE_VER0,
        spare: 0,
        mount_id: mount,
        param: STATMOUNT_MNT_ROOT,
    };
    let mut status = Statmount {
        _size: [0; 2],
        mask: 0,
        _fields: [0; 11],
        mount_root: 0,
        _mount_point: 0,
        _reserved: [0; 50],
        strings: [0; STATMOUNT_STRINGS],
    };
    // SAFETY: `statmount` reads `request`, which has the layout of the `struct mnt_id_req` of its
    // `size`, and writes at most `size_of::<Statmount>()` bytes to `status`; both outlive the
    // call.
    let flags: c_uint = 0;
    let read = Errno::result(unsafe {
        nix::libc::syscall(
            SYS_STATMOUNT,
            &raw const request,
            &raw mut status,
            size_of::<Statmount>(),
            flags,
        )
    });
    match read {
        Ok(_) => {}
        // A root's path that does not fit is longer than `/`.
        Err(Errno::EOVERFLOW) => return Ok(false),
        Err(error) => return Err(error),
    }
    if status.mask & STATMOUNT_MNT_ROOT == 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let root = usize::try_from(status.mount_root)
        .ok()
        .and_then(|start| status.strings.get(start..))
        .and_then(|rest| CStr::from_bytes_until_nul(rest).ok());
    Ok(root.is_some_and(|root| root.to_bytes() == b"/"))
}

/// Returns a descriptor of the mount whose root, or of the file in it, `descriptor` refers to,
/// with `open_tree`: of a detached copy of that mount alone, without the mounts below it, when
/// `clone` is set, which needs `CAP_SYS_ADMIN`, and of the mount itself otherwise.
///
/// A detached copy is in no mount namespace, so no path leads to it until [`move_mount`]
/// attaches it, and closing its last descriptor first unmounts it.
pub(crate) fn open_tree(descriptor: &FileDescriptor, clone: bool) -> Result<FileDescriptor, Errno> {
    let mut flags = nix::libc::AT_EMPTY_PATH as c_uint | OPEN_TREE_CLOEXEC;
    if clone {
        flags |= OPEN_TREE_CLONE;
    }
    // SAFETY: `open_tree` reads the NUL-terminated empty path and the integers.
    let opened = Errno::result(unsafe {
        nix::libc::syscall(
            nix::libc::SYS_open_tree,
            descriptor.raw(),
            c"".as_ptr(),
            flags,
        )
    })?;
    let opened = RawFd::try_from(opened).map_err(|_| Errno::EBADF)?;
    // SAFETY: a successful `open_tree` returns a new descriptor that nothing else owns.
    Ok(unsafe { FileDescriptor::from_raw(opened) })
}

/// Sets the attributes `set` of the mount whose root `tree` refers to, and clears `clear`, with
/// `mount_setattr`, on that mount alone, without the mounts below it. It needs `CAP_SYS_ADMIN`.
pub(crate) fn set_mount_attributes(
    tree: &FileDescriptor,
    set: MountAttributes,
    clear: MountAttributes,
) -> Result<(), Errno> {
    let attributes = MountAttr {
        attr_set: set.0,
        attr_clr: clear.0,
        propagation: 0,
        userns_fd: 0,
    };
    // SAFETY: `mount_setattr` reads the NUL-terminated empty path and `size_of::<MountAttr>()`
    // bytes of `attributes`, which has `struct mount_attr`'s layout and outlives the call.
    Errno::result(unsafe {
        nix::libc::syscall(
            nix::libc::SYS_mount_setattr,
            tree.raw(),
            c"".as_ptr(),
            nix::libc::AT_EMPTY_PATH as c_uint,
            &raw const attributes,
            size_of::<MountAttr>(),
        )
    })
    .map(drop)
}

/// Moves the mount whose root `tree` refers to onto the file that `target` refers to, with
/// `move_mount`, whatever path names that file now. A detached copy from [`open_tree`] is
/// attached there. It needs `CAP_SYS_ADMIN`, and `target` must be in the caller's mount
/// namespace.
pub(crate) fn move_mount(tree: &FileDescriptor, target: &FileDescriptor) -> Result<(), Errno> {
    // SAFETY: `move_mount` reads the two NUL-terminated empty paths and the integers.
    Errno::result(unsafe {
        nix::libc::syscall(
            nix::libc::SYS_move_mount,
            tree.raw(),
            c"".as_ptr(),
            target.raw(),
            c"".as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        )
    })
    .map(drop)
}

/// Unmounts the mount on the entry `name` of the directory that `directory` refers to, without
/// following a symbolic link at `name`, and without detaching a busy mount: `EBUSY`.
///
/// Linux unmounts only by path, so the call names the entry through the directory's entry in
/// procfs, which leads to exactly that directory, whatever path names it now; procfs must be
/// mounted at `/proc`. Only `name`, one component, is looked up. The directory's descriptor
/// keeps the directory's own mount busy, not the one on `name`. An entry that is not the root of
/// a mount fails with `EINVAL`.
pub(crate) fn unmount_beneath(directory: &FileDescriptor, name: &CStr) -> Result<(), Errno> {
    let mut path = format!("{DESCRIPTORS}/{}/", directory.raw()).into_bytes();
    path.extend_from_slice(name.to_bytes());
    let path = super::c_string(&path)?;
    nix::mount::umount2(path.as_c_str(), MntFlags::UMOUNT_NOFOLLOW)
}

/// Detaches the mount whose root `tree` refers to from where it is attached, even if it is busy,
/// as a lazy unmount does: no path leads to it from then on, and the kernel releases it once its
/// last user, such as `tree`, lets it go. procfs must be mounted at `/proc`.
///
/// A descriptor of a mount keeps the mount busy, so only a lazy unmount takes the mount off
/// through it.
pub(crate) fn detach_mount(tree: &FileDescriptor) -> Result<(), Errno> {
    let path = super::c_string(format!("{DESCRIPTORS}/{}", tree.raw()).as_bytes())?;
    nix::mount::umount2(path.as_c_str(), MntFlags::MNT_DETACH)
}

#[cfg(test)]
pub(super) mod tests;
