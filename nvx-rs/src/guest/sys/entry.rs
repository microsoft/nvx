//! The kernel interfaces of an execution's entry, which [`crate::guest::entry`] prepares and the
//! child of [`crate::guest::spawn`] runs.
//!
//! The parent prepares everything that the entry's stages use before it starts the child: the
//! root's path, the hostname, and the identity. [`clone_into_namespaces`] then starts the child
//! with `clone3`, in new
//! mount, PID, and UTS namespaces, so that the child itself is PID 1 of its PID namespace, with no
//! process between it and its caller. The child makes only system calls over that storage, in
//! the order of [`EntryChild`]'s stages, and reports the first that fails.
//!
//! libc does not know of a child that `clone3` starts: the child's copy of libc's thread state
//! still names its parent's thread, and libc's wrappers that change credentials signal every
//! thread that libc believes the process has. So the child calls no libc function that relies
//! on that state. It changes its credentials, capabilities, hostname, and umask, and uses the new
//! mount API (`fsopen`, `fsconfig`, `fsmount`, `open_tree`, `move_mount`, and `mount_setattr`)
//! and `openat2`, through libc's `syscall`, with the kernel's structures that the libc crate does
//! not define laid out here from the kernel's UAPI. It makes its other calls through `nix`, or
//! libc's plain wrappers of one system call each, as a forked child does. No step can panic: a
//! panic would abort through libc's `raise`, which signals the thread that libc names, its
//! parent's.

use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;

use nix::libc::{self, c_int, c_long, c_uint, c_ulong};
use nix::mount::MsFlags;
use nix::unistd::Pid;

use super::{Errno, FileDescriptor};
use crate::guest::spawn::LaunchStage;

/// The `clone3` flags of an entry's child: new mount, PID, and UTS namespaces.
const NAMESPACES: u64 = (libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWUTS) as u64;
/// `open_tree`'s `OPEN_TREE_CLONE`: a detached copy of the mount tree at the path.
const OPEN_TREE_CLONE: c_uint = 1;
/// `open_tree`'s `OPEN_TREE_CLOEXEC`: the descriptor closes on `exec`.
const OPEN_TREE_CLOEXEC: c_uint = libc::O_CLOEXEC as c_uint;
/// `mount_setattr`'s `AT_RECURSIVE`: the change applies to every mount below the path's too.
const AT_RECURSIVE: c_uint = 0x8000;
/// `move_mount`'s `MOVE_MOUNT_F_EMPTY_PATH`: the source is the descriptor itself.
const MOVE_MOUNT_F_EMPTY_PATH: c_uint = 0x04;
/// `move_mount`'s `MOVE_MOUNT_T_EMPTY_PATH`: the target is the descriptor itself.
const MOVE_MOUNT_T_EMPTY_PATH: c_uint = 0x40;
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
/// The attributes of every read-only bind of an entry.
const READ_ONLY_BIND: u64 =
    MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC;
/// The attributes that every mount of an entry's root takes, as every mount of a layered root's
/// tree does before its runtime file systems: `nosuid` and `nodev`.
const TREE_ATTRIBUTES: u64 = MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV;
/// `openat2`'s resolution of an entry's paths: below the starting directory, through no symbolic
/// link or magic link, and on the starting directory's mount.
const BENEATH: u64 = libc::RESOLVE_BENEATH
    | libc::RESOLVE_NO_SYMLINKS
    | libc::RESOLVE_NO_MAGICLINKS
    | libc::RESOLVE_NO_XDEV;
/// The capability header's `_LINUX_CAPABILITY_VERSION_3`, which takes two data words of 32 bits.
const CAPABILITY_VERSION: u32 = 0x2008_0522;
/// The mode of the directories that an entry creates in its root, whatever its umask.
const DIRECTORY_MODE: u32 = 0o755;
/// The mode of the device nodes that an entry creates in its `/dev`.
const DEVICE_MODE: u32 = 0o666;

/// The device nodes that an entry may create in its `/dev`: the name, and the major and minor
/// numbers of the character device. Bit `n` of [`EntryChild::devices`] selects node `n`.
pub(crate) const DEVICE_NODES: [(&CStr, u32, u32); 6] = [
    (c"null", 1, 3),
    (c"zero", 1, 5),
    (c"full", 1, 7),
    (c"random", 1, 8),
    (c"urandom", 1, 9),
    (c"tty", 5, 0),
];
/// The symbolic links in every entry's `/dev`: the name and the target.
const DEVICE_LINKS: [(&CStr, &CStr); 5] = [
    (c"ptmx", c"pts/ptmx"),
    (c"fd", c"/proc/self/fd"),
    (c"stdin", c"/proc/self/fd/0"),
    (c"stdout", c"/proc/self/fd/1"),
    (c"stderr", c"/proc/self/fd/2"),
];

/// A file system that an entry mounts in its root, as a layered root's
/// [`mount_runtime`](crate::guest::root::Root::mount_runtime) does in its own: its type, its
/// string parameters, and its mount's attributes.
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
/// The tmpfs at `/run`, which a layered root mounts, and an entry does not: `nosuid` and
/// `nodev`, with mode 0755.
pub(crate) const RUN: FileSystem = FileSystem {
    kind: c"tmpfs",
    parameters: &[(c"source", c"tmpfs"), (c"mode", c"0755")],
    attributes: MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV,
};
/// The tmpfs of the source of the masks of an execution as user ID 0: `nosuid`, `nodev`, and
/// `noexec`, with mode 0555.
const MASK_SOURCE: FileSystem = FileSystem {
    kind: c"tmpfs",
    parameters: &[(c"source", c"tmpfs"), (c"mode", c"0555")],
    attributes: MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC,
};
/// The procfs entries that an execution as user ID 0 sees read-only.
const PROC_READ_ONLY: [&CStr; 5] = [c"sys", c"sysrq-trigger", c"bus", c"fs", c"irq"];
/// The procfs entries that an execution as user ID 0 sees masked: as an empty, read-only file or
/// directory.
const PROC_MASKED: [&CStr; 9] = [
    c"kcore",
    c"keys",
    c"timer_list",
    c"timer_stats",
    c"latency_stats",
    c"sched_debug",
    c"acpi",
    c"asound",
    c"scsi",
];
/// The sysfs entries that an execution as user ID 0 sees masked.
const SYS_MASKED: [&CStr; 1] = [c"firmware"];
/// The empty file of the mask source, which masks a file.
const MASK_FILE: &CStr = c"file";
/// The empty directory of the mask source, which masks a directory.
const MASK_DIRECTORY: &CStr = c"directory";

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

/// The kernel's `struct clone_args`, in its first version, which `clone3` reads.
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    /// The `CLONE_*` flags.
    flags: u64,
    /// Where the kernel stores a pidfd, with `CLONE_PIDFD`.
    pidfd: u64,
    /// Where the kernel stores the child's thread ID in the child, with `CLONE_CHILD_SETTID`.
    child_tid: u64,
    /// Where the kernel stores the child's thread ID in the parent, with `CLONE_PARENT_SETTID`.
    parent_tid: u64,
    /// The signal that the child's end sends its parent.
    exit_signal: u64,
    /// The lowest address of the child's stack, or 0 to keep the parent's.
    stack: u64,
    /// The size of the child's stack.
    stack_size: u64,
    /// The child's thread-local storage, with `CLONE_SETTLS`.
    tls: u64,
}

/// The kernel's `struct open_how`, which `openat2` reads.
#[repr(C)]
struct OpenHow {
    /// The `O_*` flags.
    flags: u64,
    /// The mode of a file that the call creates.
    mode: u64,
    /// The `RESOLVE_*` flags.
    resolve: u64,
}

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

/// What the child of an entry needs, prepared before it starts.
#[derive(Debug, Default)]
pub(crate) struct EntryChild {
    /// The absolute path of the root, a mount that the child moves over `/`.
    pub(crate) root: std::ffi::CString,
    /// The hostname of the child's UTS namespace, at most 64 bytes.
    pub(crate) hostname: Box<[u8]>,
    /// The device nodes of the child's `/dev`, as bits of [`DEVICE_NODES`].
    pub(crate) devices: u8,
    /// The user ID that the child takes.
    pub(crate) uid: u32,
    /// The group ID that the child takes.
    pub(crate) gid: u32,
    /// The supplementary groups that the child takes.
    pub(crate) groups: Box<[libc::gid_t]>,
    /// The capabilities that the child keeps, bit `n` for capability `n`.
    pub(crate) capabilities: u64,
    /// Test-only simulation of the entry stages, without privileges.
    #[cfg(test)]
    pub(crate) simulated: Option<tests::Simulation>,
}

impl EntryChild {
    /// Returns the `clone3` flags of the child, which tests that simulate the stages, without
    /// privileges, leave empty.
    pub(super) fn clone_flags(&self) -> u64 {
        #[cfg(test)]
        if self.simulated.is_some() {
            return 0;
        }
        NAMESPACES
    }

    /// Makes the child's mounts private, sets its hostname, moves its root over `/`, and mounts
    /// its view: procfs, sysfs, and `/dev` with its devpts and shm, in the order of their stages.
    /// It creates its directories and nodes with exact modes, whatever the process's umask, which
    /// it clears before it mounts the view and restores before it returns: a failure to clear it
    /// is the [`Proc`](LaunchStage::Proc) stage's, and one to restore it the
    /// [`Devices`](LaunchStage::Devices) stage's.
    ///
    /// Each failure names its stage. Only the post-clone child of an entry may call this.
    pub(super) fn enter_view(&self) -> Result<(), (LaunchStage, Errno)> {
        self.stage(LaunchStage::Propagation, make_mounts_private)?;
        self.stage(LaunchStage::Hostname, || set_hostname(&self.hostname))?;
        self.stage(LaunchStage::MoveRoot, || move_root(&self.root))?;
        let umask = set_umask(0).map_err(|error| (LaunchStage::Proc, error))?;
        let view = self.mount_view();
        let restored = set_umask(umask).map_err(|error| (LaunchStage::Devices, error));
        view.and(restored.map(drop))
    }

    /// Mounts the view's file systems, as [`enter_view`](Self::enter_view) describes.
    fn mount_view(&self) -> Result<(), (LaunchStage, Errno)> {
        let root_user = self.uid == 0;
        let mut mask = None;
        self.stage(LaunchStage::Proc, || {
            let proc = mount_proc()?;
            if root_user {
                let source = mask_source()?;
                mask = Some(source);
                restrict_proc(proc, source)?;
            }
            Ok(())
        })?;
        self.stage(LaunchStage::Sys, || {
            let sys = mount_sys()?;
            if let Some(source) = mask {
                for name in SYS_MASKED {
                    mask_entry(sys, name, source)?;
                }
            }
            Ok(())
        })?;
        self.stage(LaunchStage::Devices, || mount_devices(self.devices))
    }

    /// Takes the child's identity: drops every capability but its own from the bounding set,
    /// takes its supplementary groups, group ID, and user ID, applies its capabilities to the
    /// permitted and effective sets, with empty inheritable and ambient sets, and sets
    /// `no_new_privs`, in the order of their stages.
    ///
    /// Each failure names its stage. Only the post-clone child of an entry may call this.
    pub(super) fn take_identity(&self) -> Result<(), (LaunchStage, Errno)> {
        self.stage(LaunchStage::Bounding, || drop_bounding(self.capabilities))?;
        self.stage(LaunchStage::Identity, || {
            set_identity(self.uid, self.gid, &self.groups)
        })?;
        self.stage(LaunchStage::Capabilities, || {
            set_capabilities(self.capabilities)
        })?;
        self.stage(LaunchStage::NoNewPrivileges, || {
            prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0)
        })
    }

    /// Runs the system calls of `stage`, and names it in their failure. A test that simulates
    /// the stages skips them, and fails the stage that it names.
    fn stage(
        &self,
        stage: LaunchStage,
        calls: impl FnOnce() -> Result<(), Errno>,
    ) -> Result<(), (LaunchStage, Errno)> {
        #[cfg(test)]
        if let Some(simulation) = self.simulated {
            return match simulation {
                tests::Simulation::Fail(failing, error) if failing == stage => Err((stage, error)),
                _ => Ok(()),
            };
        }
        calls().map_err(|error| (stage, error))
    }
}

/// Starts a child in the namespaces that `flags` names, as `fork` starts one otherwise, and
/// returns its process ID in the parent, or `None` in the child. The child's end sends its
/// parent `SIGCHLD`, and the child keeps a copy of its parent's memory and descriptors.
///
/// # Safety
///
/// The child may only make system calls over storage that the parent prepared, as the
/// [module](self) describes, and must end in `execve` or `_exit`.
pub(super) unsafe fn clone_into_namespaces(flags: u64) -> Result<Option<Pid>, Errno> {
    let mut arguments = CloneArgs {
        flags,
        exit_signal: libc::SIGCHLD as u64,
        ..CloneArgs::default()
    };
    // SAFETY: the arguments are a valid first-version `clone_args` without a stack, so the child
    // runs on its copy of the parent's, as after `fork`; the caller keeps the child's promise.
    let result = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &mut arguments as *mut CloneArgs,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    match result {
        0 => Ok(None),
        child if child > 0 => Ok(Some(Pid::from_raw(child as libc::pid_t))),
        _ => Err(Errno::last()),
    }
}

/// Makes the directory at `path` the child's working directory, resolving it in the child's root
/// through no magic link: neither a `/proc` link to a descriptor, a working directory, or a root,
/// nor a symbolic link to one, may then place the child outside its root, or in a directory that
/// a mount of the view covers, such as the writable `/proc/sys` below its read-only bind.
pub(super) fn enter_directory(path: &CStr) -> Result<(), Errno> {
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: libc::RESOLVE_NO_MAGICLINKS,
    };
    // SAFETY: the path is a NUL-terminated string, and `how` a valid `open_how` of its size.
    let directory = descriptor(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    })?;
    // SAFETY: `fchdir` takes a descriptor, which the call above opened.
    let entered = unit(unsafe { libc::fchdir(directory) }.into());
    close(directory);
    entered
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

/// Makes every mount of the child's new mount namespace private, recursively, so that no mount
/// propagates to or from another namespace.
fn make_mounts_private() -> Result<(), Errno> {
    nix::mount::mount(
        None::<&CStr>,
        c"/",
        None::<&CStr>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&CStr>,
    )
}

/// Sets the hostname of the child's new UTS namespace.
fn set_hostname(hostname: &[u8]) -> Result<(), Errno> {
    // SAFETY: the kernel reads `hostname.len()` bytes from the slice.
    unit(unsafe { libc::syscall(libc::SYS_sethostname, hostname.as_ptr(), hostname.len()) })
}

/// Moves the root mount at `root` over `/` and makes it the child's root and working directory.
///
/// It first makes every mount of the root's tree `nosuid` and `nodev` in the child's namespace,
/// whatever its options elsewhere. The root then hides the namespace's former root, which no
/// path reaches from it: a `..` from the root's top stays there, as the root's mount point is the
/// top of the former root.
fn move_root(root: &CStr) -> Result<(), Errno> {
    nix::unistd::chdir(root)?;
    mount_setattr(libc::AT_FDCWD, c".", AT_RECURSIVE, TREE_ATTRIBUTES)?;
    nix::mount::mount(
        Some(c"."),
        c"/",
        None::<&CStr>,
        MsFlags::MS_MOVE,
        None::<&CStr>,
    )?;
    nix::unistd::chroot(c".")?;
    nix::unistd::chdir(c"/")
}

/// Sets the calling process's umask to `mask`, and returns the umask that it replaces.
///
/// The child makes the system call itself, because `nix`'s `umask` panics when the call fails,
/// as a seccomp policy can make it.
fn set_umask(mask: libc::mode_t) -> Result<libc::mode_t, Errno> {
    // SAFETY: `umask` takes an integer and dereferences nothing.
    let previous = unsafe { libc::syscall(libc::SYS_umask, mask) };
    if previous < 0 {
        Err(Errno::last())
    } else {
        // A umask fits a `mode_t`.
        Ok(previous as libc::mode_t)
    }
}

/// Mounts a new procfs instance of the child's PID namespace at `/proc`, `nosuid`, `nodev`, and
/// `noexec`, and returns its mount's descriptor.
fn mount_proc() -> Result<RawFd, Errno> {
    let target = directory_target(libc::AT_FDCWD, c"proc")?;
    let proc = new_mount(&PROC)?;
    move_mount(proc, target)?;
    Ok(proc)
}

/// Makes the procfs entries of [`PROC_READ_ONLY`] read-only, and masks those of [`PROC_MASKED`]
/// with `source`, in the procfs whose mount `proc` holds. An entry that this kernel lacks needs
/// neither.
fn restrict_proc(proc: RawFd, source: RawFd) -> Result<(), Errno> {
    for name in PROC_READ_ONLY {
        let Some(target) = existing_entry(proc, name)? else {
            continue;
        };
        let tree = open_tree(
            target,
            c"",
            OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | libc::AT_EMPTY_PATH as c_uint,
        )?;
        mount_setattr(tree, c"", libc::AT_EMPTY_PATH as c_uint, READ_ONLY_BIND)?;
        move_mount(tree, target)?;
    }
    for name in PROC_MASKED {
        mask_entry(proc, name, source)?;
    }
    Ok(())
}

/// Mounts a new sysfs at `/sys`, read-only, `nosuid`, `nodev`, and `noexec`, and returns its
/// mount's descriptor.
fn mount_sys() -> Result<RawFd, Errno> {
    let target = directory_target(libc::AT_FDCWD, c"sys")?;
    let sys = new_mount(&SYS)?;
    move_mount(sys, target)?;
    Ok(sys)
}

/// Creates the source of the masks of an execution as user ID 0: a tmpfs, `nosuid`, `nodev`,
/// and `noexec`, that holds one empty file and one empty directory, and that is read-only once
/// it holds them. It returns the tmpfs's mount descriptor, of which each mask binds one entry.
fn mask_source() -> Result<RawFd, Errno> {
    let source = new_mount(&MASK_SOURCE)?;
    let file = descriptor(
        // SAFETY: the path is a NUL-terminated string, and the mode an integer.
        unsafe {
            libc::openat(
                source,
                MASK_FILE.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                0o444 as c_uint,
            )
        }
        .into(),
    )?;
    close(file);
    make_directory(source, MASK_DIRECTORY, 0o555)?;
    mount_setattr(
        source,
        c"",
        libc::AT_EMPTY_PATH as c_uint,
        MOUNT_ATTR_RDONLY,
    )?;
    Ok(source)
}

/// Masks the entry `name` of the directory `directory` with the empty directory, or the empty
/// file, of the mask source `source`. An entry that does not exist needs no mask.
fn mask_entry(directory: RawFd, name: &CStr, source: RawFd) -> Result<(), Errno> {
    let Some(target) = existing_entry(directory, name)? else {
        return Ok(());
    };
    let mask = if file_kind(target)? == libc::S_IFDIR {
        MASK_DIRECTORY
    } else {
        MASK_FILE
    };
    let tree = open_tree(source, mask, OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC)?;
    move_mount(tree, target)
}

/// Mounts the child's `/dev`: a tmpfs, `nosuid` and with mode 0755, which holds the device
/// nodes that `devices` selects, with mode 0666, the symbolic links of [`DEVICE_LINKS`], a
/// devpts instance of its own at `pts`, `nosuid` and `noexec`, and a tmpfs at `shm`, `nosuid`,
/// `nodev`, and with mode 1777.
fn mount_devices(devices: u8) -> Result<(), Errno> {
    let target = directory_target(libc::AT_FDCWD, c"dev")?;
    let dev = new_mount(&DEV)?;
    move_mount(dev, target)?;
    populate_devices(dev, devices)?;
    let pts = new_mount(&DEV_PTS)?;
    move_mount(pts, directory_target(dev, c"pts")?)?;
    let shm = new_mount(&DEV_SHM)?;
    move_mount(shm, directory_target(dev, c"shm")?)
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
/// layered root owns, unlike an entry's child.
pub(crate) fn mount_detached(file_system: &FileSystem) -> Result<FileDescriptor, Errno> {
    let mount = new_mount(file_system)?;
    // SAFETY: `fsmount` returned a new descriptor that nothing else owns.
    Ok(unsafe { FileDescriptor::from_raw(mount) })
}

/// Sets `nosuid` and `nodev` on the mount whose root `directory` refers to, and on every mount
/// below it, as an entry does on its root.
pub(crate) fn restrict_tree(directory: &FileDescriptor) -> Result<(), Errno> {
    let flags = AT_RECURSIVE | libc::AT_EMPTY_PATH as c_uint;
    mount_setattr(directory.raw(), c"", flags, TREE_ATTRIBUTES)
}

/// Creates in the directory `dev` every device node of [`DEVICE_NODES`], with mode 0666 whatever
/// the umask, which an entry's child clears and a layered root's caller keeps, and the symbolic
/// links of [`DEVICE_LINKS`].
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

/// Returns a descriptor of the directory `name` in `directory`, below it and on its mount,
/// through no symbolic link, and creates the directory, with mode 0755, if it is missing.
fn directory_target(directory: RawFd, name: &CStr) -> Result<RawFd, Errno> {
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    match open_target(directory, name, flags) {
        Err(Errno::ENOENT) => {
            match make_directory(directory, name, DIRECTORY_MODE) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(error) => return Err(error),
            }
            open_target(directory, name, flags)
        }
        result => result,
    }
}

/// Returns a descriptor of the entry `name` of `directory`, without following it, or `None` if
/// there is none.
fn existing_entry(directory: RawFd, name: &CStr) -> Result<Option<RawFd>, Errno> {
    match open_target(
        directory,
        name,
        libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    ) {
        Ok(target) => Ok(Some(target)),
        Err(Errno::ENOENT) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Returns the file type bits of `descriptor`'s file.
fn file_kind(descriptor: RawFd) -> Result<libc::mode_t, Errno> {
    let mut status = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the kernel fills the status, which is initialized once the call succeeds.
    unit(unsafe { libc::fstat(descriptor, status.as_mut_ptr()) }.into())?;
    // SAFETY: the successful call initialized the status.
    Ok(unsafe { status.assume_init() }.st_mode & libc::S_IFMT)
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

/// Opens `path` below `directory`, on its mount, and through no symbolic link, with `flags`.
fn open_target(directory: RawFd, path: &CStr, flags: c_int) -> Result<RawFd, Errno> {
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve: BENEATH,
    };
    // SAFETY: the path is a NUL-terminated string, and `how` a valid `open_how` of its size.
    descriptor(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    })
}

/// Opens the mount tree at `path` in `directory` with `open_tree`'s `flags`.
fn open_tree(directory: RawFd, path: &CStr, flags: c_uint) -> Result<RawFd, Errno> {
    // SAFETY: the path is a NUL-terminated string.
    descriptor(unsafe { libc::syscall(libc::SYS_open_tree, directory, path.as_ptr(), flags) })
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

/// Attaches the mount that `source` holds at the file or directory that `target` holds.
fn move_mount(source: RawFd, target: RawFd) -> Result<(), Errno> {
    // SAFETY: both paths are empty NUL-terminated strings, which the flags take as the
    // descriptors themselves.
    unit(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            source,
            c"".as_ptr(),
            target,
            c"".as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        )
    })
}

/// Creates the directory `name` in `directory` with `mode`.
fn make_directory(directory: RawFd, name: &CStr, mode: u32) -> Result<(), Errno> {
    // SAFETY: the name is a NUL-terminated string, and the mode an integer.
    unit(unsafe { libc::mkdirat(directory, name.as_ptr(), mode) }.into())
}

/// Closes a descriptor that the child opened, ignoring a failure, as the descriptor is gone
/// either way.
fn close(descriptor: RawFd) {
    // SAFETY: the child opened `descriptor`, and no owner of it exists.
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
