//! Checks of the descriptor-rooted calls on the build host, without privileges; the
//! mappings' privileged tests cover the mount calls that need them.

use std::os::unix::fs::PermissionsExt;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::guest::sys::{open, path_c_string};

/// Exchanges the files at `first` and `second` atomically, with `renameat2`'s
/// `RENAME_EXCHANGE`, for the tests' rename races; `nix` wraps `renameat2` only for glibc.
pub(crate) fn exchange_paths(first: &CStr, second: &CStr) -> Result<(), Errno> {
    /// `renameat2`'s `RENAME_EXCHANGE`, in `linux/fs.h`.
    const RENAME_EXCHANGE: c_uint = 1 << 1;
    // SAFETY: `renameat2` reads the two NUL-terminated paths, which outlive the call.
    Errno::result(unsafe {
        nix::libc::syscall(
            nix::libc::SYS_renameat2,
            nix::libc::AT_FDCWD,
            first.as_ptr(),
            nix::libc::AT_FDCWD,
            second.as_ptr(),
            RENAME_EXCHANGE,
        )
    })
    .map(drop)
}

/// Gives the calling thread a descriptor table of its own that holds only the standard input,
/// output, and error, with `close_range`'s `CLOSE_RANGE_UNSHARE`, for the tests of a thread whose
/// table `/proc/self/fd` does not show. Unlike `unshare(CLONE_FILES)`, it copies no other
/// descriptor of the process, which would keep the files of other threads open, and their mounts
/// busy, after those threads closed them. The thread must own no descriptor above standard error
/// yet, since its new table holds none of them.
pub(crate) fn unshare_standard_descriptors() -> Result<(), Errno> {
    let first: c_uint = 3;
    // SAFETY: `close_range` takes no pointer, and it leaves out of the thread's new table only
    // descriptors that the caller does not own.
    Errno::result(unsafe {
        nix::libc::syscall(
            nix::libc::SYS_close_range,
            first,
            c_uint::MAX,
            nix::libc::CLOSE_RANGE_UNSHARE,
        )
    })
    .map(drop)
}

/// Runs `test` in a thread of its own whose umask is `umask`, which no other thread shares, and
/// returns what it returns, for the tests of creations whatever the umask.
pub(crate) fn with_thread_umask<T: Send>(umask: u32, test: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
                nix::sys::stat::umask(Mode::from_bits_truncate(umask));
                test()
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// Returns the calling thread's umask, which procfs tells without changing it.
pub(crate) fn thread_umask() -> u32 {
    let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))
        .and_then(|umask| u32::from_str_radix(umask.trim(), 8).ok())
        .unwrap()
}

/// Gives the directory at `path` the default ACL `u::rwx,g::---,o::---`, which withholds the
/// group's and others' permissions from whatever is created in it, whatever the mode that the
/// creation asks for, and returns whether it did: not on a file system without POSIX ACLs, for
/// the tests of creations whatever a default ACL.
pub(crate) fn restrict_new_entries(path: &std::path::Path) -> bool {
    // The value of `system.posix_acl_default`, as `linux/posix_acl_xattr.h` lays it out: the
    // version, 2, then an entry for the owner, `ACL_USER_OBJ`, with every permission, then one
    // for the owning group, `ACL_GROUP_OBJ`, and one for others, `ACL_OTHER`, with none, each
    // with an undefined ID.
    let mut acl = 2_u32.to_le_bytes().to_vec();
    for (tag, permissions) in [(0x01_u16, 0o7_u16), (0x04, 0), (0x20, 0)] {
        acl.extend(tag.to_le_bytes());
        acl.extend(permissions.to_le_bytes());
        acl.extend(u32::MAX.to_le_bytes());
    }
    let name = c"system.posix_acl_default";
    let path_name = path_c_string(path).unwrap();
    // SAFETY: `setxattr` reads the NUL-terminated path and name, and `acl.len()` bytes of `acl`,
    // which all outlive the call.
    let set = Errno::result(unsafe {
        nix::libc::setxattr(
            path_name.as_ptr(),
            name.as_ptr(),
            acl.as_ptr().cast(),
            acl.len(),
            0,
        )
    });
    match set {
        Ok(_) => true,
        Err(Errno::EOPNOTSUPP) => {
            use std::io::Write;

            let message = format!("{} has no POSIX ACLs: skipped", path.display());
            let _ = writeln!(std::io::stderr(), "{message}");
            false
        }
        Err(error) => panic!("default ACL of {}: {error}", path.display()),
    }
}

/// The attempts at a resolution that fails with `EAGAIN` before its result counts: a `..`
/// after a descent fails with it while a rename or a mount anywhere in the system, such as
/// another test's, races its lookup.
const ATTEMPTS: usize = 100;

/// Opens `path` with `O_PATH`.
fn path_descriptor(path: &std::path::Path) -> FileDescriptor {
    let flags = OpenFlags::O_PATH | OpenFlags::O_CLOEXEC;
    open(&path_c_string(path).unwrap(), flags, Mode::empty()).unwrap()
}

/// Returns what `attempt` returns, retrying it while it fails with `EAGAIN`, as openat2(2)
/// lets a caller, up to [`ATTEMPTS`] attempts in all.
fn retried<T>(mut attempt: impl FnMut() -> Result<T, Errno>) -> Result<T, Errno> {
    let mut attempts = 1;
    loop {
        match attempt() {
            Err(Errno::EAGAIN) if attempts < ATTEMPTS => attempts += 1,
            result => return result,
        }
    }
}

/// Returns the status of `descriptor`, or `None` on a kernel without unique mount IDs.
fn status(descriptor: &FileDescriptor) -> Option<FileStatus> {
    match file_status(descriptor) {
        Err(Errno::EOPNOTSUPP) => None,
        status => Some(status.unwrap()),
    }
}

#[test]
fn paths_resolve_only_beneath_their_directory_and_through_no_link() {
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path();
    std::fs::create_dir_all(base.join("a/b")).unwrap();
    std::fs::write(base.join("a/file"), b"file").unwrap();
    std::os::unix::fs::symlink("b", base.join("a/link")).unwrap();
    std::os::unix::fs::symlink(base, base.join("a/absolute")).unwrap();
    let directory = path_descriptor(base);
    let flags = OpenFlags::O_PATH;
    let opened =
        |path: &CStr| retried(|| open_beneath(&directory, path, flags, Mode::empty(), true));
    opened(c"a/b").unwrap();
    opened(c"a/file").unwrap();
    for (path, error) in [
        (c"..", Errno::EXDEV),
        (c"a/../..", Errno::EXDEV),
        (c"/", Errno::EXDEV),
        (c"a/link", Errno::ELOOP),
        (c"a/link/x", Errno::ELOOP),
        (c"a/absolute", Errno::ELOOP),
        (c"a/file/x", Errno::ENOTDIR),
        (c"a/missing", Errno::ENOENT),
    ] {
        assert_eq!(opened(path).map(drop), Err(error), "{path:?}");
    }

    // Neither magic links nor the mounts below a directory are crossed when they may not
    // be.
    let root = path_descriptor(std::path::Path::new("/"));
    let proc = |path: &CStr, cross| open_beneath(&root, path, flags, Mode::empty(), cross);
    assert_eq!(proc(c"proc/self", true).map(drop), Err(Errno::ELOOP));
    assert_eq!(proc(c"proc/1/root", true).map(drop), Err(Errno::ELOOP));
    assert_eq!(proc(c"proc", false).map(drop), Err(Errno::EXDEV));
    proc(c"proc", true).unwrap();
}

#[test]
fn a_dot_dot_after_a_descent_is_refused_while_renames_race_it() {
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path();
    std::fs::create_dir(base.join("a")).unwrap();
    std::fs::write(base.join("file"), b"file").unwrap();
    let directory = path_descriptor(base);
    let flags = OpenFlags::O_PATH;
    let resolve = || open_beneath(&directory, c"a/../..", flags, Mode::empty(), true);
    let (started, stop) = (Barrier::new(2), AtomicBool::new(false));
    let unexpected = std::thread::scope(|scope| {
        // Each rename advances the system's rename count, whose change since the lookup
        // began fails the `..` after `a` with `EAGAIN`.
        scope.spawn(|| {
            started.wait();
            let (file, renamed) = (base.join("file"), base.join("renamed"));
            while !stop.load(Ordering::Relaxed) {
                std::fs::rename(&file, &renamed).unwrap();
                std::fs::rename(&renamed, &file).unwrap();
            }
        });
        started.wait();
        let unexpected = (0..10_000)
            .map(|_| retried(resolve).map(drop))
            .find(|result| *result != Err(Errno::EXDEV));
        stop.store(true, Ordering::Relaxed);
        unexpected
    });
    assert_eq!(unexpected, None);
}

/// Returns the mode, without its type, of the file at `name` below `base`.
fn mode_at(base: &std::path::Path, name: &str) -> u32 {
    std::fs::metadata(base.join(name))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

#[test]
fn files_are_created_with_exactly_their_modes_and_directories_get_theirs_by_descriptor() {
    // Modes that the usual umask, 022, would narrow, under a stricter one, which stays.
    with_thread_umask(0o077, || {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path();
        let directory = path_descriptor(base);
        // A directory has the permissions that the umask leaves it, until its mode is set
        // through a descriptor of it, even an `O_PATH` one, which `fchmod` takes not.
        create_directory_at(&directory, c"new", Mode::from_bits_truncate(0o777)).unwrap();
        assert_eq!(mode_at(base, "new"), 0o700);
        assert_eq!(
            create_directory_at(&directory, c"new", Mode::empty()),
            Err(Errno::EEXIST)
        );
        let flags = OpenFlags::O_PATH;
        let new = open_beneath(&directory, c"new", flags, Mode::empty(), false).unwrap();
        change_mode(&new, Mode::from_bits_truncate(0o777)).unwrap();
        create_file_at(&directory, c"new/file", Mode::from_bits_truncate(0o666)).unwrap();
        assert_eq!(
            (mode_at(base, "new"), mode_at(base, "new/file")),
            (0o777, 0o666)
        );
        assert_eq!(thread_umask(), 0o077);
        // An exclusive creation fails on any entry, a link included.
        std::os::unix::fs::symlink("missing", base.join("link")).unwrap();
        let mode = Mode::from_bits_truncate(0o644);
        for name in [c"new", c"new/file", c"link"] {
            let result = create_file_at(&directory, name, mode);
            assert_eq!(result, Err(Errno::EEXIST), "{name:?}");
        }
    });
}

#[test]
fn a_default_acl_narrows_a_new_directory_but_not_a_new_file_nor_a_set_mode() {
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path();
    if !restrict_new_entries(base) {
        return;
    }
    let directory = path_descriptor(base);
    // The ACL, not the umask, withholds the group's and others' permissions from a
    // directory created there.
    create_directory_at(&directory, c"new", Mode::from_bits_truncate(0o755)).unwrap();
    assert_eq!(mode_at(base, "new"), 0o700);
    // Neither from a file's exact mode, nor from a mode set through a descriptor.
    create_file_at(&directory, c"file", Mode::from_bits_truncate(0o644)).unwrap();
    let flags = OpenFlags::O_PATH;
    let new = open_beneath(&directory, c"new", flags, Mode::empty(), false).unwrap();
    change_mode(&new, Mode::from_bits_truncate(0o755)).unwrap();
    assert_eq!(
        (mode_at(base, "new"), mode_at(base, "file")),
        (0o755, 0o644)
    );
}

#[test]
fn statuses_tell_kinds_owners_modes_mounts_and_mount_roots() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::create_dir(temporary.path().join("directory")).unwrap();
    std::fs::write(temporary.path().join("file"), b"").unwrap();
    let permissions = std::fs::Permissions::from_mode(0o1751);
    std::fs::set_permissions(temporary.path().join("directory"), permissions).unwrap();
    let Some(directory) = status(&path_descriptor(&temporary.path().join("directory"))) else {
        return;
    };
    let file = status(&path_descriptor(&temporary.path().join("file"))).unwrap();
    assert_eq!(
        (directory.kind, file.kind),
        (FileKind::Directory, FileKind::Regular)
    );
    let user = nix::unistd::geteuid().as_raw();
    assert_eq!((directory.owner, directory.mode), (user, 0o1751));
    assert_eq!(directory.mount, file.mount);
    assert!(!directory.mount_root && !file.mount_root);
    let null = status(&path_descriptor(std::path::Path::new("/dev/null"))).unwrap();
    assert_eq!(null.kind, FileKind::Other);
    let root = status(&path_descriptor(std::path::Path::new("/"))).unwrap();
    let proc = status(&path_descriptor(std::path::Path::new("/proc"))).unwrap();
    assert!(root.mount_root && proc.mount_root);
    assert_ne!(root.mount, proc.mount);
    // A descriptor of a mount itself refers to the same file and mount.
    let tree = open_tree(&path_descriptor(std::path::Path::new("/proc")), false).unwrap();
    assert_eq!(status(&tree), Some(proc));
}

#[test]
fn a_mount_of_a_whole_file_system_is_told_by_its_root() {
    let path = std::path::Path::new("/proc");
    let Some(proc) = status(&path_descriptor(path)) else {
        return;
    };
    // procfs is mounted whole, at its own root.
    assert_eq!(is_whole_file_system(proc.mount), Ok(true));
    // No mount of the caller's namespace has the largest unique ID.
    assert_eq!(is_whole_file_system(u64::MAX), Err(Errno::ENOENT));
}

#[test]
fn mount_calls_without_privileges_fail_and_mount_nothing() {
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let directory = path_descriptor(temporary.path());
    assert_eq!(open_tree(&directory, true).map(drop), Err(Errno::EPERM));
    let tree = open_tree(&directory, false).unwrap();
    let attributes = MountAttributes::READ_ONLY | MountAttributes::NO_DEVICES;
    assert_eq!(attributes, MountAttributes(0x5));
    let set = set_mount_attributes(&tree, attributes, MountAttributes::empty());
    assert_eq!(set, Err(Errno::EPERM));
    assert_eq!(move_mount(&tree, &directory), Err(Errno::EPERM));
    // An unmount looks its path up first.
    assert_eq!(unmount_beneath(&directory, c"x"), Err(Errno::ENOENT));
    std::fs::create_dir(temporary.path().join("x")).unwrap();
    assert_eq!(unmount_beneath(&directory, c"x"), Err(Errno::EPERM));
    assert_eq!(detach_mount(&tree), Err(Errno::EPERM));
}
