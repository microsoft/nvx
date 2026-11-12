//! The kernel interfaces of a layered root's runtime file systems, which
//! [`Root::mount_runtime`](crate::guest::root::Root::mount_runtime) mounts, of the VM's hostname,
//! and of the identity that the child of [`PreparedChild`](super::PreparedChild) takes for a
//! workload.
//!
//! The runtime's file systems are mounted with the new mount API (`fsopen`, `fsconfig`,
//! `fsmount`, and `mount_setattr`), the hostname is set, and the child changes its credentials and
//! capabilities, through libc's `syscall`, with the kernel's structures that the libc crate does
//! not define laid out here from the kernel's UAPI. The child's steps allocate nothing, and none
//! can panic.

use std::ffi::CStr;
use std::os::fd::RawFd;

use nix::libc::{self, c_int, c_long, c_uint, c_ulong};

use super::spawn::LaunchStage;
use super::{Errno, FileDescriptor};

/// `mount_setattr`'s `AT_RECURSIVE`: the change applies to every mount below the path's too.
const AT_RECURSIVE: c_uint = 0x8000;
/// `fsopen`'s `FSOPEN_CLOEXEC`.
const FSOPEN_CLOEXEC: c_uint = 1;
/// `fsconfig`'s `FSCONFIG_SET_STRING`: sets a parameter to a string.
const FSCONFIG_SET_STRING: c_uint = 1;
/// `fsconfig`'s `FSCONFIG_CMD_CREATE`: creates the file system's superblock.
const FSCONFIG_CMD_CREATE: c_uint = 6;
/// `fsmount`'s `FSMOUNT_CLOEXEC`.
const FSMOUNT_CLOEXEC: c_uint = 1;
/// The mount attribute that makes a mount read-only.
const MOUNT_ATTR_RDONLY: u64 = 0x1;
/// The mount attribute that ignores set-user-ID and set-group-ID bits and file capabilities.
const MOUNT_ATTR_NOSUID: u64 = 0x2;
/// The mount attribute that refuses to open device files.
const MOUNT_ATTR_NODEV: u64 = 0x4;
/// The mount attribute that refuses to execute files.
const MOUNT_ATTR_NOEXEC: u64 = 0x8;
/// The attributes of a read-only mount: read-only, `nosuid`, `nodev`, and `noexec`.
const READ_ONLY_BIND: u64 =
    MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC;
/// The attributes that every mount of a layered root's tree takes before its runtime file
/// systems: `nosuid` and `nodev`.
const TREE_ATTRIBUTES: u64 = MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV;
/// The capability header's `_LINUX_CAPABILITY_VERSION_3`, which takes two data words of 32 bits.
const CAPABILITY_VERSION: u32 = 0x2008_0522;
/// The mode of the device nodes of a layered root's `/dev`.
const DEVICE_MODE: u32 = 0o666;

/// The device nodes of a layered root's `/dev`: the name, and the major and minor numbers of the
/// character device. Bit `n` of [`populate_devices`]'s selection selects node `n`.
pub(crate) const DEVICE_NODES: [(&CStr, u32, u32); 6] = [
    (c"null", 1, 3),
    (c"zero", 1, 5),
    (c"full", 1, 7),
    (c"random", 1, 8),
    (c"urandom", 1, 9),
    (c"tty", 5, 0),
];
/// The symbolic links in a layered root's `/dev`: the name and the target.
const DEVICE_LINKS: [(&CStr, &CStr); 5] = [
    (c"ptmx", c"pts/ptmx"),
    (c"fd", c"/proc/self/fd"),
    (c"stdin", c"/proc/self/fd/0"),
    (c"stdout", c"/proc/self/fd/1"),
    (c"stderr", c"/proc/self/fd/2"),
];

/// A file system that a layered root's
/// [`mount_runtime`](crate::guest::root::Root::mount_runtime) mounts: its type, its string
/// parameters, and its mount's attributes.
#[derive(Debug)]
pub(crate) struct FileSystem {
    /// The type, which `fsopen` takes.
    pub(crate) kind: &'static CStr,
    /// The parameters, which `fsconfig` sets.
    pub(crate) parameters: &'static [(&'static CStr, &'static CStr)],
    /// The mount's attributes, `MOUNT_ATTR_*`, which `fsmount` sets.
    pub(crate) attributes: u64,
}

/// procfs at `/proc`: `nosuid`, `nodev`, and `noexec`.
pub(crate) const PROC: FileSystem = FileSystem {
    kind: c"proc",
    parameters: &[(c"source", c"proc")],
    attributes: MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC,
};
/// sysfs at `/sys`: read-only, `nosuid`, `nodev`, and `noexec`.
pub(crate) const SYS: FileSystem = FileSystem {
    kind: c"sysfs",
    parameters: &[(c"source", c"sysfs")],
    attributes: READ_ONLY_BIND,
};
/// The tmpfs at `/dev`: `nosuid`, with mode 0755.
pub(crate) const DEV: FileSystem = FileSystem {
    kind: c"tmpfs",
    parameters: &[(c"source", c"tmpfs"), (c"mode", c"0755")],
    attributes: MOUNT_ATTR_NOSUID,
};
/// devpts at `/dev/pts`, an instance of its own: `nosuid` and `noexec`, with mode 0666 for
/// `ptmx` and 0620 for the terminals.
pub(crate) const DEV_PTS: FileSystem = FileSystem {
    kind: c"devpts",
    parameters: &[
        (c"source", c"devpts"),
        (c"ptmxmode", c"0666"),
        (c"mode", c"0620"),
    ],
    attributes: MOUNT_ATTR_NOSUID | MOUNT_ATTR_NOEXEC,
};
/// The tmpfs at `/dev/shm`: `nosuid` and `nodev`, with mode 1777.
pub(crate) const DEV_SHM: FileSystem = FileSystem {
    kind: c"tmpfs",
    parameters: &[(c"source", c"tmpfs"), (c"mode", c"1777")],
    attributes: MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV,
};
/// The tmpfs at `/run`, which a layered root mounts: `nosuid` and `nodev`, with mode 0755.
pub(crate) const RUN: FileSystem = FileSystem {
    kind: c"tmpfs",
    parameters: &[(c"source", c"tmpfs"), (c"mode", c"0755")],
    attributes: MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV,
};

/// The system calls that change credentials, which take 32-bit IDs on this architecture.
#[cfg(not(any(target_arch = "x86", target_arch = "arm")))]
use libc::{
    SYS_setgroups as SYS_SETGROUPS, SYS_setresgid as SYS_SETRESGID, SYS_setresuid as SYS_SETRESUID,
};
/// The system calls that change credentials, in their forms that take 32-bit IDs.
#[cfg(any(target_arch = "x86", target_arch = "arm"))]
use libc::{
    SYS_setgroups32 as SYS_SETGROUPS, SYS_setresgid32 as SYS_SETRESGID,
    SYS_setresuid32 as SYS_SETRESUID,
};

/// The kernel's `struct mount_attr`, in its first version, which `mount_setattr` reads.
#[repr(C)]
struct MountAttr {
    /// The `MOUNT_ATTR_*` attributes to set.
    attr_set: u64,
    /// The `MOUNT_ATTR_*` attributes to clear.
    attr_clr: u64,
    /// The propagation type to set, or 0 to keep it.
    propagation: u64,
    /// The user namespace of an ID-mapped mount.
    userns_fd: u64,
}

/// The kernel's `struct __user_cap_header_struct`.
#[repr(C)]
struct CapabilityHeader {
    /// The interface's version.
    version: u32,
    /// The process whose capabilities change: 0 for the caller.
    pid: c_int,
}

/// The kernel's `struct __user_cap_data_struct`: one 32-bit word of each capability set.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CapabilityData {
    /// The word of the effective set.
    effective: u32,
    /// The word of the permitted set.
    permitted: u32,
    /// The word of the inheritable set.
    inheritable: u32,
}

/// Takes a plain workload's IDs with empty groups and capabilities.
pub(super) fn take_workload_identity(uid: u32, gid: u32) -> Result<(), (LaunchStage, Errno)> {
    drop_bounding(0).map_err(|error| (LaunchStage::Bounding, error))?;
    set_identity(uid, gid, &[]).map_err(|error| (LaunchStage::Identity, error))?;
    set_capabilities(0).map_err(|error| (LaunchStage::Capabilities, error))?;
    prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0).map_err(|error| (LaunchStage::NoNewPrivileges, error))
}

/// Returns whether `file` is a regular file, and its size.
pub(crate) fn file_size(file: &FileDescriptor) -> Result<(bool, u64), Errno> {
    let status = nix::sys::stat::fstat(file.borrowed())?;
    Ok((
        status.st_mode & libc::S_IFMT == libc::S_IFREG,
        u64::try_from(status.st_size).unwrap_or(0),
    ))
}

/// Returns the capability data words that give a process exactly `capabilities` as its
/// permitted and effective sets, and an empty inheritable set: bit `n` of the set is bit `n % 32`
/// of word `n / 32`.
fn capability_data(capabilities: u64) -> [CapabilityData; 2] {
    let word = |bits: u64| CapabilityData {
        effective: bits as u32,
        permitted: bits as u32,
        inheritable: 0,
    };
    [word(capabilities), word(capabilities >> 32)]
}

/// Sets the hostname of the current UTS namespace.
pub(crate) fn set_hostname(hostname: &[u8]) -> Result<(), Errno> {
    // SAFETY: the kernel reads `hostname.len()` bytes from the slice.
    unit(unsafe { libc::syscall(libc::SYS_sethostname, hostname.as_ptr(), hostname.len()) })
}

/// Creates, in the directory `dev`, the device nodes of [`DEVICE_NODES`] that `devices` selects,
/// with mode 0666 but for what the umask withholds, and the symbolic links of [`DEVICE_LINKS`].
fn populate_devices(dev: RawFd, devices: u8) -> Result<(), Errno> {
    for (bit, (name, major, minor)) in DEVICE_NODES.iter().enumerate() {
        if devices & (1 << bit) == 0 {
            continue;
        }
        // SAFETY: the name is a NUL-terminated string, and the mode and device integers.
        unit(
            unsafe {
                libc::mknodat(
                    dev,
                    name.as_ptr(),
                    libc::S_IFCHR | DEVICE_MODE,
                    libc::makedev(*major, *minor),
                )
            }
            .into(),
        )?;
    }
    for (name, link) in DEVICE_LINKS {
        // SAFETY: both strings are NUL-terminated.
        unit(unsafe { libc::symlinkat(link.as_ptr(), dev, name.as_ptr()) }.into())?;
    }
    Ok(())
}

/// Mounts a new instance of `file_system`, detached, and returns its mount's descriptor, which a
/// layered root owns.
pub(crate) fn mount_detached(file_system: &FileSystem) -> Result<FileDescriptor, Errno> {
    let mount = new_mount(file_system)?;
    // SAFETY: `fsmount` returned a new descriptor that nothing else owns.
    Ok(unsafe { FileDescriptor::from_raw(mount) })
}

/// Sets `nosuid` and `nodev` on the mount whose root `directory` refers to, and on every mount
/// below it.
pub(crate) fn restrict_tree(directory: &FileDescriptor) -> Result<(), Errno> {
    let flags = AT_RECURSIVE | libc::AT_EMPTY_PATH as c_uint;
    mount_setattr(directory.raw(), c"", flags, TREE_ATTRIBUTES)
}

/// Creates in the directory `dev` every device node of [`DEVICE_NODES`], with mode 0666 whatever
/// the umask, which a layered root's caller keeps, and the symbolic links of [`DEVICE_LINKS`].
pub(crate) fn create_devices(dev: &FileDescriptor) -> Result<(), Errno> {
    populate_devices(dev.raw(), u8::MAX)?;
    let mode = nix::sys::stat::Mode::from_bits_truncate(DEVICE_MODE);
    let follow = nix::sys::stat::FchmodatFlags::FollowSymlink;
    for (name, _, _) in DEVICE_NODES {
        nix::sys::stat::fchmodat(dev.borrowed(), name, mode, follow)?;
    }
    Ok(())
}

/// Drops from the bounding set every capability that `capabilities` does not hold, up to the
/// last capability of the kernel, which then refuses the next with `EINVAL`.
fn drop_bounding(capabilities: u64) -> Result<(), Errno> {
    for capability in 0..u64::BITS {
        if capabilities & (1 << capability) != 0 {
            continue;
        }
        match prctl(libc::PR_CAPBSET_DROP, c_ulong::from(capability), 0) {
            Ok(()) => {}
            Err(Errno::EINVAL) => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Takes the supplementary groups `groups`, then the real, effective, and saved group ID `gid`,
/// then the real, effective, and saved user ID `uid`, for the calling thread alone.
fn set_identity(uid: u32, gid: u32, groups: &[libc::gid_t]) -> Result<(), Errno> {
    // SAFETY: the kernel reads `groups.len()` group IDs from the slice.
    unit(unsafe { libc::syscall(SYS_SETGROUPS, groups.len(), groups.as_ptr()) })?;
    // SAFETY: `setresgid` takes three integers.
    unit(unsafe { libc::syscall(SYS_SETRESGID, gid, gid, gid) })?;
    // SAFETY: `setresuid` takes three integers.
    unit(unsafe { libc::syscall(SYS_SETRESUID, uid, uid, uid) })
}

/// Makes `capabilities` the permitted and effective sets, empties the inheritable set, and
/// clears the ambient set.
fn set_capabilities(capabilities: u64) -> Result<(), Errno> {
    let header = CapabilityHeader {
        version: CAPABILITY_VERSION,
        pid: 0,
    };
    let data = capability_data(capabilities);
    // SAFETY: the header names version 3, whose two data words the array holds.
    unit(unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) })?;
    prctl(
        libc::PR_CAP_AMBIENT,
        libc::PR_CAP_AMBIENT_CLEAR_ALL as c_ulong,
        0,
    )
}

/// Makes the `prctl` call `option` with two arguments, and zeros for the others, which the
/// options here require.
fn prctl(option: c_int, first: c_ulong, second: c_ulong) -> Result<(), Errno> {
    // SAFETY: `prctl` takes integers for these options, and dereferences none of them.
    unit(unsafe {
        libc::syscall(
            libc::SYS_prctl,
            option,
            first,
            second,
            0 as c_ulong,
            0 as c_ulong,
        )
    })
}

/// Creates the file system `file_system` with its string parameters, mounts it detached with its
/// mount attributes, and returns the mount's descriptor.
fn new_mount(file_system: &FileSystem) -> Result<RawFd, Errno> {
    // SAFETY: the name is a NUL-terminated string.
    let context = descriptor(unsafe {
        libc::syscall(libc::SYS_fsopen, file_system.kind.as_ptr(), FSOPEN_CLOEXEC)
    })?;
    let mount = configure_and_mount(context, file_system.parameters, file_system.attributes);
    close(context);
    mount
}

/// Sets `parameters` on the file system context `context`, creates its superblock, and mounts
/// it detached with `attributes`.
fn configure_and_mount(
    context: RawFd,
    parameters: &[(&CStr, &CStr)],
    attributes: u64,
) -> Result<RawFd, Errno> {
    for (key, value) in parameters {
        // SAFETY: the key and the value are NUL-terminated strings.
        unit(unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                context,
                FSCONFIG_SET_STRING,
                key.as_ptr(),
                value.as_ptr(),
                0 as c_int,
            )
        })?;
    }
    // SAFETY: the command takes no key or value.
    unit(unsafe {
        libc::syscall(
            libc::SYS_fsconfig,
            context,
            FSCONFIG_CMD_CREATE,
            std::ptr::null::<libc::c_char>(),
            std::ptr::null::<libc::c_void>(),
            0 as c_int,
        )
    })?;
    // SAFETY: `fsmount` takes integers.
    descriptor(unsafe {
        libc::syscall(
            libc::SYS_fsmount,
            context,
            FSMOUNT_CLOEXEC,
            attributes as c_uint,
        )
    })
}

/// Sets the mount attributes `attributes` on the mount at `path` in `directory`, with
/// `mount_setattr`'s `flags`.
fn mount_setattr(
    directory: RawFd,
    path: &CStr,
    flags: c_uint,
    attributes: u64,
) -> Result<(), Errno> {
    let attributes = MountAttr {
        attr_set: attributes,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    // SAFETY: the path is a NUL-terminated string, and the attributes a valid `mount_attr` of
    // its size.
    unit(unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            directory,
            path.as_ptr(),
            flags,
            &attributes,
            std::mem::size_of::<MountAttr>(),
        )
    })
}

/// Closes a descriptor that this module opened, ignoring a failure, as the descriptor is gone
/// either way.
fn close(descriptor: RawFd) {
    // SAFETY: this module opened `descriptor`, and no owner of it exists.
    unsafe { libc::close(descriptor) };
}

/// Returns a system call's descriptor, or its `errno`.
fn descriptor(result: c_long) -> Result<RawFd, Errno> {
    if result < 0 {
        Err(Errno::last())
    } else {
        // A descriptor fits a `RawFd`.
        Ok(result as RawFd)
    }
}

/// Returns a system call's success, or its `errno`.
fn unit(result: c_long) -> Result<(), Errno> {
    if result < 0 {
        Err(Errno::last())
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests;
