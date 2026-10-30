//! The kernel interfaces of the root operations, which tests replace.
//!
//! The operations read the kernel's text files and walk directories through the standard
//! library, at the paths of a [`Paths`], so that a test relocates them to a temporary directory.
//! What a test cannot do in a temporary directory, such as a mount, an unmount, a flush of a
//! block device's buffers, a sync of a mounted file system, a reread of a disk's partition table,
//! or a read of a GPT disk's partition table at the geometry that its block device reports, and
//! the writes to the kernel's control files, the check for a mount point, and the options of a
//! mount, whose effects and answers a test simulates, go through a [`Kernel`], which also performs
//! the mapper's operations for the root's live shares and mappings.

use std::ffi::CStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::gpt::{Disk, Expected};
use crate::guest::gpt::{self, DeviceError};
use crate::guest::mappings::Kernel as MappingKernel;
use crate::guest::parse::parse_decimal;
use crate::guest::sys::{self, Errno, Mode, MountFlags, OpenFlags};

pub(crate) use crate::guest::mappings::SystemKernel;

/// The kernel's table of the calling thread's mount namespace.
const MOUNT_TABLE: &str = "/proc/thread-self/mountinfo";

/// The kernel's operations that the root operations perform, other than reads: mounts and
/// unmounts, flushes of a block device's buffers, syncs of a mounted file system, rereads of a
/// disk's partition table, writes of a control file, and the sleeps between two polls for a block
/// device; the check for a mount point and the options of a mount, which depend on them; since
/// only a block device reports its geometry, the read of a GPT disk's partition table; and the
/// mapper's operations, through which the root mounts its live shares and mappings.
///
/// [`SystemKernel`] performs them. The tests' fake records the operations, simulates the effect
/// of a cgroup's membership file and its own mounts, reads a GPT disk's table from a regular
/// file, and fails the operations that a test chooses.
pub(super) trait Kernel: MappingKernel + Send + Sync {
    /// Mounts `source` at `target`, as `mount(2)` does, with the file system's own options
    /// `data`.
    fn mount(
        &mut self,
        source: Option<&CStr>,
        target: &CStr,
        filesystem: Option<&CStr>,
        flags: MountFlags,
        data: Option<&CStr>,
    ) -> Result<(), Errno>;

    /// Unmounts the file system mounted at `target`, without detaching a busy one.
    fn unmount(&mut self, target: &CStr) -> Result<(), Errno>;

    /// Writes the dirty buffers of the block device `device`, then drops its cached ones.
    fn flush(&mut self, device: &CStr) -> Result<(), Errno>;

    /// Writes the dirty data and metadata of the file system mounted at the mount point `target`
    /// to its storage, as `syncfs(2)` does.
    fn sync_file_system(&mut self, target: &CStr) -> Result<(), Errno>;

    /// Asks the kernel to reread the partition table of the disk whose whole-disk block device is
    /// `device`, as [`sys::reread_partition_table`] does.
    fn reread_partitions(&mut self, device: &CStr) -> Result<(), Errno>;

    /// Reads and checks the partition table of the GPT disk that `file` opens, the whole-disk
    /// block device whose node is `device`, with the caller's `expected` values, at the device's
    /// logical block size and size, as [`gpt::read_disk`] does.
    fn read_disk(
        &mut self,
        device: &Path,
        file: &File,
        expected: &Expected,
    ) -> Result<Disk, DeviceError>;

    /// Writes `value` whole to the existing control file `path`, such as a cgroup's or a
    /// sysctl's, and reports a failure to close it.
    fn write(&mut self, path: &CStr, value: &[u8]) -> Result<(), Errno>;

    /// Returns whether the directory `path` is a mount point: whether it is on another mount
    /// than its parent directory, as it is where a file system is mounted, or where a directory
    /// of the parent's own file system is bound.
    fn is_mount_point(&mut self, path: &Path) -> io::Result<bool>;

    /// Returns the options of the file system mounted at the mount point `path`, as the kernel's
    /// mount table shows them: `ro` or `rw`, then those of the file system's own options that
    /// differ from their defaults.
    fn mount_options(&mut self, path: &Path) -> io::Result<String>;

    /// Sleeps for `duration`.
    fn sleep(&mut self, duration: Duration);
}

impl Kernel for SystemKernel {
    fn mount(
        &mut self,
        source: Option<&CStr>,
        target: &CStr,
        filesystem: Option<&CStr>,
        flags: MountFlags,
        data: Option<&CStr>,
    ) -> Result<(), Errno> {
        sys::mount(source, target, filesystem, flags, data)
    }

    fn unmount(&mut self, target: &CStr) -> Result<(), Errno> {
        sys::unmount(target)
    }

    fn flush(&mut self, device: &CStr) -> Result<(), Errno> {
        let descriptor = sys::open(
            device,
            OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        sys::flush_block_buffers(&descriptor)?;
        descriptor.close()
    }

    fn sync_file_system(&mut self, target: &CStr) -> Result<(), Errno> {
        let descriptor = sys::open(
            target,
            OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        descriptor.sync_file_system()?;
        descriptor.close()
    }

    fn reread_partitions(&mut self, device: &CStr) -> Result<(), Errno> {
        let descriptor = sys::open(
            device,
            OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        sys::reread_partition_table(&descriptor)?;
        descriptor.close()
    }

    fn read_disk(
        &mut self,
        _device: &Path,
        file: &File,
        expected: &Expected,
    ) -> Result<Disk, DeviceError> {
        gpt::read_disk(file, expected)
    }

    fn write(&mut self, path: &CStr, value: &[u8]) -> Result<(), Errno> {
        let descriptor = sys::open(
            path,
            OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        descriptor.write_all(value)?;
        descriptor.close()
    }

    fn is_mount_point(&mut self, path: &Path) -> io::Result<bool> {
        let parent = path.parent().ok_or(Errno::EINVAL)?;
        let mount = sys::mount_id(&sys::path_c_string(path)?)?;
        Ok(mount != sys::mount_id(&sys::path_c_string(parent)?)?)
    }

    fn mount_options(&mut self, path: &Path) -> io::Result<String> {
        let mount = sys::mount_id(&sys::path_c_string(path)?)?;
        let table = fs::read(MOUNT_TABLE)?;
        let options = super_options(&table, mount).ok_or(Errno::ENOENT)?;
        Ok(String::from_utf8_lossy(options).into_owned())
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Returns the options of the file system of the mount whose ID is `mount`, the last field of its
/// line in the mount table `table`, or `None` if the table has no such line.
///
/// A line's fields are separated by spaces, which the kernel escapes within a field: the mount's
/// ID first, and its file system's type, source, and options after a field `-`.
pub(super) fn super_options(table: &[u8], mount: u64) -> Option<&[u8]> {
    table.split(|&byte| byte == b'\n').find_map(|line| {
        let mut fields = line.split(|&byte| byte == b' ');
        if parse_decimal(fields.next()?)? != mount {
            return None;
        }
        fields.skip_while(|field| *field != b"-").nth(3)
    })
}

/// The directories of the kernel's interfaces that the root operations read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Paths {
    /// The mount point of procfs.
    pub(crate) proc: PathBuf,
    /// The mount point of sysfs.
    pub(crate) sys: PathBuf,
    /// The directory of the device nodes.
    pub(crate) dev: PathBuf,
}

impl Default for Paths {
    /// Returns the paths at which the guest mounts the kernel's interfaces.
    fn default() -> Self {
        Self {
            proc: "/proc".into(),
            sys: "/sys".into(),
            dev: "/dev".into(),
        }
    }
}

#[cfg(test)]
pub(crate) use fake::{Call, FakeKernel, Operation, attach_calls};

/// The tests' kernel, which records each operation and performs no mount, unmount, flush, sync,
/// or reread.
#[cfg(test)]
mod fake {
    use std::ffi::{CStr, OsStr};
    use std::fs::File;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use super::Kernel;
    use crate::gpt::{Disk, Expected};
    use crate::guest::gpt::DeviceError;
    use crate::guest::mappings::{Access, Kernel as MappingKernel};
    use crate::guest::sys::{
        self, Errno, FileDescriptor, FileKind, FileStatus, Mode, MountAttributes, MountFlags,
        OpenFlags,
    };

    /// The unique ID of the mount that every path is on that none of the fake kernel's mounts
    /// covers, whose root is `/`.
    pub(crate) const BASE_MOUNT: u64 = 1;
    /// The start of the path that leads to the memory file that stands for a tree of
    /// `open_tree`, before the tree's number.
    const TREE_PREFIX: &[u8] = b"/memfd:nvx-fake-tree-";

    /// The logical block size of the disks whose tables the fake kernel reads, in bytes.
    const LOGICAL_BLOCK_SIZE: u32 = 512;

    /// An operation of the kernel, which a test fails.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Operation {
        /// A mount, other than a remount, which its target names.
        Mount,
        /// A remount, which its target names.
        Remount,
        /// An unmount, by path or by descriptor, which the mount point names.
        Unmount,
        /// A flush of a block device's buffers, which the device names.
        Flush,
        /// A sync of a mounted file system, which its mount point names.
        SyncFileSystem,
        /// A reread of a disk's partition table, which the disk's block device names.
        Reread,
        /// A write of a control file, which its path names.
        Write,
        /// An open beneath a directory, which the opened path names.
        Open,
        /// A creation of a directory or of a regular file, which its path names.
        Create,
        /// A change of an open file's mode, which its path names.
        ChangeMode,
        /// A read of a file's status, which its path names.
        Status,
        /// A read of a mount's root, which the mount point names, or `/` for the mount of every
        /// path that none of the fake kernel's mounts covers.
        MountRoot,
        /// A copy or an open of a file's mount by `open_tree`, which the file's path names.
        OpenTree,
        /// A change of a tree's attributes, which the path of the tree's file names.
        SetAttributes,
        /// A move of a tree, which its target names.
        MoveMount,
        /// A detachment of a tree, which its mount point names.
        Detach,
        /// A read of a mount's options, which its mount point names.
        Options,
    }

    /// One operation that the fake kernel recorded, with its paths and options as text.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Call {
        /// A mount.
        Mount {
            /// The source, if any.
            source: Option<String>,
            /// The target.
            target: String,
            /// The file system type, if any.
            filesystem: Option<String>,
            /// The flags.
            flags: MountFlags,
            /// The file system's own options, if any.
            data: Option<String>,
        },
        /// An unmount of a mount point, by path or by descriptor.
        Unmount(String),
        /// A flush of a block device's buffers.
        Flush(String),
        /// A sync of the file system mounted at a mount point.
        SyncFileSystem(String),
        /// A reread of the partition table of a disk, which its block device's node names.
        Reread(String),
        /// A read of the partition table of the GPT disk on a block device, which its node names.
        ReadDisk(String),
        /// A write of a value to a control file.
        Write(String, String),
        /// A copy, or an open, of the mount of a file by `open_tree`.
        OpenTree {
            /// The file's path.
            source: String,
            /// Whether the mount was copied, detached.
            clone: bool,
        },
        /// A change of a tree's attributes.
        SetAttributes {
            /// The path of the tree's file.
            source: String,
            /// The attributes that were set.
            set: MountAttributes,
            /// The attributes that were cleared.
            clear: MountAttributes,
        },
        /// A move of a tree onto a file.
        MoveMount {
            /// The path of the tree's file.
            source: String,
            /// The path of the file that the tree went onto.
            target: String,
        },
        /// A detachment of a tree from its mount point, as a lazy unmount does.
        Detach(String),
    }

    /// The type of the action that runs after each sleep.
    type SleepAction = Box<dyn FnMut(usize) + Send>;
    /// The type of the action that runs before each move of a tree, with its target's path.
    type MoveAction = Box<dyn FnMut(&Path) + Send>;
    /// The type of the action that runs after each creation of a file, with its path.
    type CreateAction = Box<dyn FnMut(&Path) + Send>;

    /// A mount that `open_tree` returned a descriptor of.
    #[derive(Clone, Debug)]
    struct Tree {
        /// The path of the file whose mount the tree is, or copies.
        source: PathBuf,
        /// The kind of that file.
        kind: FileKind,
        /// The mount's unique ID.
        mount: u64,
    }

    /// What the fake kernel recorded and what it fails.
    #[derive(Default)]
    struct State {
        /// The operations, in order.
        calls: Vec<Call>,
        /// The operations that fail, with their error, each at the path that names it.
        failures: Vec<(Operation, PathBuf, Errno)>,
        /// The processes that writes of their ID to a membership file leave where they are, each
        /// with the number of such writes left.
        unmovable: Vec<(u32, usize)>,
        /// The mounts that no unmount has undone, in mount order: each mount point, with the
        /// mount's unique ID.
        mounted: Vec<(PathBuf, u64)>,
        /// The unique IDs of the mounts whose root is a directory below their file system's
        /// root, as a bind of one has.
        subtrees: Vec<u64>,
        /// The trees of `open_tree`, by number.
        trees: Vec<Tree>,
        /// The last unique mount ID that the fake kernel gave, if any.
        last_mount: Option<u64>,
        /// The options that the kernel shows for the mounts at some mount points, in place of
        /// their own.
        shown: Vec<(PathBuf, String)>,
        /// The sleeps so far.
        sleeps: usize,
        /// What happens after each sleep, given the number of sleeps so far.
        on_sleep: Option<SleepAction>,
        /// What happens before each move of a tree, given its target's path.
        on_move: Option<MoveAction>,
        /// What happens after each creation of a file, given its path.
        on_create: Option<CreateAction>,
    }

    impl State {
        /// Returns a new unique mount ID.
        fn next_mount(&mut self) -> u64 {
            let mount = self.last_mount.map_or(BASE_MOUNT + 1, |last| last + 1);
            self.last_mount = Some(mount);
            mount
        }

        /// Returns the unique ID of the mount that `path` is on, the topmost of the fake
        /// kernel's mounts that cover it, and whether `path` is that mount's root.
        fn mount_of(&self, path: &Path) -> (u64, bool) {
            self.mounted
                .iter()
                .rev()
                .find(|(point, _)| path.starts_with(point))
                .map_or((BASE_MOUNT, path == Path::new("/")), |(point, mount)| {
                    (*mount, point == path)
                })
        }

        /// Returns the error of the failure of `operation` at `path`, if a test chose one.
        fn failure(&self, operation: Operation, path: &Path) -> Result<(), Errno> {
            self.failures
                .iter()
                .find(|(failing, failing_path, _)| *failing == operation && failing_path == path)
                .map_or(Ok(()), |&(_, _, error)| Err(error))
        }
    }

    /// A kernel that records each mount operation, performs no mount, unmount, flush, sync, or
    /// reread of a partition table, and fails the operations that a test chooses.
    ///
    /// A write of a control file writes the regular file at its path, which it creates if
    /// needed, except that a write of a process ID to a cgroup's `cgroup.procs` moves the ID
    /// there from the `cgroup.procs` of the cgroup's parent, as the kernel moves the process.
    ///
    /// It simulates its mounts by their mount points and unique IDs: a mount, a move of a tree
    /// onto a file, and a test's [`add_mount`](Self::add_mount) put a mount on a path, and an
    /// unmount takes it off. Each shows its whole file system, but one that a test adds with
    /// [`add_subtree_mount`](Self::add_subtree_mount). The mount points that it reports are the
    /// targets of those mounts, and, as the kernel does, it fails to check a missing directory.
    /// The mapper's operations on paths, the opens beneath a directory, the creations, and the
    /// changes of a mode, are the system's, in a test's temporary directory, but an open that may
    /// not cross into another mount fails with `EXDEV` where the fake kernel's mounts differ, and
    /// the status of a file tells the fake kernel's mount of its path. A tree of `open_tree` is a
    /// memory file, which stands for the copy of a file's mount, or for that mount itself. Its
    /// clones share what they record, so a test keeps one while a root owns another.
    ///
    /// It shows a mount's options as `ro` or `rw`, as its flags have `MS_RDONLY` or not, then the
    /// options that the mount passed, unless a test chose others, as for a kernel that applied
    /// other options than those passed, or that shows options at their defaults.
    ///
    /// It reads the table of a GPT disk, a regular file of 512-byte blocks, with [`Disk::read`].
    #[derive(Clone, Default)]
    pub(crate) struct FakeKernel(Arc<Mutex<State>>);

    impl FakeKernel {
        /// Runs `action` on the shared state.
        fn with<T>(&self, action: impl FnOnce(&mut State) -> T) -> T {
            action(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }

        /// Makes the operations of kind `operation` at `path` fail with `error`.
        pub(crate) fn fail(&self, operation: Operation, path: impl Into<PathBuf>, error: Errno) {
            self.with(|state| state.failures.push((operation, path.into(), error)));
        }

        /// Makes the operations of kind `operation` at `path` succeed again, after
        /// [`fail`](Self::fail).
        pub(crate) fn restore(&self, operation: Operation, path: &Path) {
            self.with(|state| {
                state.failures.retain(|(failing, failing_path, _)| {
                    *failing != operation || failing_path != path
                });
            });
        }

        /// Makes the next `writes` writes of the ID `process` to a membership file leave the
        /// process where it is, as for a process that reappears as fast as it is moved.
        pub(crate) fn pin(&self, process: u32, writes: usize) {
            self.with(|state| state.unmovable.push((process, writes)));
        }

        /// Makes the kernel show `options` as the options of the mounts at `path` from now on.
        pub(in crate::guest::root) fn show(&self, path: impl Into<PathBuf>, options: &str) {
            self.with(|state| state.shown.push((path.into(), options.to_owned())));
        }

        /// Runs `action` after each sleep, with the number of sleeps so far.
        pub(crate) fn on_sleep(&self, action: impl FnMut(usize) + Send + 'static) {
            self.with(|state| state.on_sleep = Some(Box::new(action)));
        }

        /// Runs `action` before each move of a tree, with the path of its target, as a process
        /// that renames a directory between a walk and its attach does.
        pub(crate) fn on_move(&self, action: impl FnMut(&Path) + Send + 'static) {
            self.with(|state| state.on_move = Some(Box::new(action)));
        }

        /// Runs `action` after each creation of a directory or of a regular file, with its path,
        /// as a process that renames what a walk created before the walk opens it does.
        pub(crate) fn on_create(&self, action: impl FnMut(&Path) + Send + 'static) {
            self.with(|state| state.on_create = Some(Box::new(action)));
        }

        /// Runs the action of [`on_create`](Self::on_create), if any, with `path`.
        fn created(&self, path: &Path) {
            let mut action = self.with(|state| state.on_create.take());
            if let Some(action) = action.as_mut() {
                action(path);
            }
            self.with(|state| {
                if state.on_create.is_none() {
                    state.on_create = action;
                }
            });
        }

        /// Puts a new mount on `path`, without recording an operation, as one that the system
        /// mounted before the test, and returns its unique ID.
        pub(crate) fn add_mount(&self, path: impl Into<PathBuf>) -> u64 {
            let path = path.into();
            self.with(|state| {
                let mount = state.next_mount();
                state.mounted.push((path, mount));
                mount
            })
        }

        /// Puts a new mount on `path` as [`add_mount`](Self::add_mount) does, but one whose root
        /// is a directory below its file system's root, as a bind of one has, and returns its
        /// unique ID.
        pub(crate) fn add_subtree_mount(&self, path: impl Into<PathBuf>) -> u64 {
            let mount = self.add_mount(path);
            self.with(|state| state.subtrees.push(mount));
            mount
        }

        /// Returns the unique ID of the topmost mount whose mount point is `path`, if any.
        pub(crate) fn mount_at(&self, path: &Path) -> Option<u64> {
            self.with(|state| {
                let (mount, root) = state.mount_of(path);
                (root && mount != BASE_MOUNT).then_some(mount)
            })
        }

        /// Returns the operations so far, in order.
        pub(crate) fn calls(&self) -> Vec<Call> {
            self.with(|state| state.calls.clone())
        }

        /// Returns the mount points of the unmounts so far, in order.
        pub(crate) fn unmounts(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Unmount(target) => Some(target),
                    _ => None,
                })
                .collect()
        }

        /// Returns the targets of the mounts so far, in order.
        pub(crate) fn mounts(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Mount { target, .. } => Some(target),
                    _ => None,
                })
                .collect()
        }

        /// Returns the targets of the moves of trees so far, in order.
        pub(crate) fn moves(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::MoveMount { target, .. } => Some(target),
                    _ => None,
                })
                .collect()
        }

        /// Returns the paths and values of the writes so far, in order.
        pub(crate) fn writes(&self) -> Vec<(String, String)> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Write(path, value) => Some((path, value)),
                    _ => None,
                })
                .collect()
        }

        /// Returns the number of sleeps so far.
        pub(crate) fn sleeps(&self) -> usize {
            self.with(|state| state.sleeps)
        }

        /// Records `call`, and returns the error of the failure of `operation` at `path`, if a
        /// test chose one.
        fn record(&self, call: Call, operation: Operation, path: &Path) -> Result<(), Errno> {
            self.with(|state| {
                state.calls.push(call);
                state.failure(operation, path)
            })
        }

        /// Returns the error of the failure of `operation` at `path`, if a test chose one,
        /// without recording anything.
        fn check(&self, operation: Operation, path: &Path) -> Result<(), Errno> {
            self.with(|state| state.failure(operation, path))
        }

        /// Returns the tree that `descriptor` refers to, with its number, if it refers to one.
        fn tree(&self, descriptor: &FileDescriptor) -> Result<Option<(usize, Tree)>, Errno> {
            let path = path_of(descriptor)?;
            let number = path
                .as_os_str()
                .as_bytes()
                .strip_prefix(TREE_PREFIX)
                .and_then(|rest| {
                    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
                    std::str::from_utf8(&rest[..digits])
                        .ok()?
                        .parse::<usize>()
                        .ok()
                });
            Ok(number.and_then(|number| {
                self.with(|state| state.trees.get(number).cloned())
                    .map(|tree| (number, tree))
            }))
        }

        /// Returns the status of the file that `descriptor` opened, with the fake kernel's
        /// mounts, without a test's failures.
        fn status_of(&self, descriptor: &FileDescriptor) -> Result<FileStatus, Errno> {
            let status = nix::sys::stat::fstat(descriptor.borrowed())?;
            let (owner, mode) = (status.st_uid, status.st_mode & !nix::libc::S_IFMT);
            if let Some((_, tree)) = self.tree(descriptor)? {
                return Ok(FileStatus {
                    kind: tree.kind,
                    owner,
                    mode,
                    mount: tree.mount,
                    mount_root: true,
                });
            }
            let path = path_of(descriptor)?;
            let kind = match status.st_mode & nix::libc::S_IFMT {
                nix::libc::S_IFDIR => FileKind::Directory,
                nix::libc::S_IFREG => FileKind::Regular,
                _ => FileKind::Other,
            };
            let (mount, mount_root) = self.with(|state| state.mount_of(&path));
            Ok(FileStatus {
                kind,
                owner,
                mode,
                mount,
                mount_root,
            })
        }
    }

    /// Returns `text` as a `String`, replacing what is not UTF-8.
    fn text(text: &CStr) -> String {
        text.to_string_lossy().into_owned()
    }

    /// Returns the operations with which the fake kernel records a mapping of `source`, a file of
    /// a source root, at `target` with `access`: the copy of the source's mount, its restriction,
    /// and its attach.
    pub(crate) fn attach_calls(source: &Path, target: &Path, access: Access) -> Vec<Call> {
        let source = source.display().to_string();
        let restricted = MountAttributes::NO_SET_ID | MountAttributes::NO_DEVICES;
        let (set, clear) = match access {
            Access::ReadOnly => (
                restricted | MountAttributes::READ_ONLY,
                MountAttributes::empty(),
            ),
            Access::ReadWrite => (restricted, MountAttributes::READ_ONLY),
        };
        vec![
            Call::OpenTree {
                source: source.clone(),
                clone: true,
            },
            Call::SetAttributes {
                source: source.clone(),
                set,
                clear,
            },
            Call::MoveMount {
                source,
                target: target.display().to_string(),
            },
        ]
    }

    /// Returns the path of `path`, as a mount's operation names it.
    fn path(path: &CStr) -> PathBuf {
        PathBuf::from(OsStr::from_bytes(path.to_bytes()))
    }

    /// Returns the path that leads to the file that `descriptor` opened now, from the calling
    /// thread's descriptors in procfs.
    fn path_of(descriptor: &FileDescriptor) -> Result<PathBuf, Errno> {
        use std::os::fd::AsRawFd;

        std::fs::read_link(format!(
            "/proc/thread-self/fd/{}",
            descriptor.borrowed().as_raw_fd()
        ))
        .map_err(|error| sys::errno_of(&error))
    }

    /// Moves the ID `process` into the membership file `path`, from that of the parent of
    /// `path`'s cgroup.
    fn move_process(path: &Path, process: u32) -> Result<(), Errno> {
        let source = path
            .parent()
            .and_then(Path::parent)
            .map(|parent| parent.join("cgroup.procs"))
            .ok_or(Errno::EINVAL)?;
        let listed = std::fs::read_to_string(&source).map_err(|_| Errno::ENOENT)?;
        let line = process.to_string();
        let remaining: String = listed
            .lines()
            .filter(|listed| *listed != line)
            .map(|listed| format!("{listed}\n"))
            .collect();
        std::fs::write(&source, remaining).map_err(|_| Errno::EIO)?;
        let mut members = std::fs::read_to_string(path).unwrap_or_default();
        members.push_str(&format!("{process}\n"));
        std::fs::write(path, members).map_err(|_| Errno::EIO)
    }

    impl Kernel for FakeKernel {
        fn mount(
            &mut self,
            source: Option<&CStr>,
            target: &CStr,
            filesystem: Option<&CStr>,
            flags: MountFlags,
            data: Option<&CStr>,
        ) -> Result<(), Errno> {
            let call = Call::Mount {
                source: source.map(text),
                target: text(target),
                filesystem: filesystem.map(text),
                flags,
                data: data.map(text),
            };
            let operation = if flags.contains(MountFlags::MS_REMOUNT) {
                Operation::Remount
            } else {
                Operation::Mount
            };
            self.record(call, operation, &path(target))?;
            if operation == Operation::Mount {
                self.add_mount(path(target));
            }
            Ok(())
        }

        fn unmount(&mut self, target: &CStr) -> Result<(), Errno> {
            let target = path(target);
            let call = Call::Unmount(target.display().to_string());
            self.record(call, Operation::Unmount, &target)?;
            self.with(|state| {
                if let Some(last) = state
                    .mounted
                    .iter()
                    .rposition(|(point, _)| *point == target)
                {
                    state.mounted.remove(last);
                }
            });
            Ok(())
        }

        fn flush(&mut self, device: &CStr) -> Result<(), Errno> {
            self.record(Call::Flush(text(device)), Operation::Flush, &path(device))
        }

        fn sync_file_system(&mut self, target: &CStr) -> Result<(), Errno> {
            let call = Call::SyncFileSystem(text(target));
            self.record(call, Operation::SyncFileSystem, &path(target))
        }

        fn reread_partitions(&mut self, device: &CStr) -> Result<(), Errno> {
            self.record(Call::Reread(text(device)), Operation::Reread, &path(device))
        }

        fn read_disk(
            &mut self,
            device: &Path,
            file: &File,
            expected: &Expected,
        ) -> Result<Disk, DeviceError> {
            self.with(|state| {
                state
                    .calls
                    .push(Call::ReadDisk(device.display().to_string()));
            });
            let size = file.metadata().map_err(DeviceError::Size)?.len();
            Disk::read(file, LOGICAL_BLOCK_SIZE, size, expected).map_err(DeviceError::Table)
        }

        fn write(&mut self, file: &CStr, value: &[u8]) -> Result<(), Errno> {
            let call = Call::Write(text(file), String::from_utf8_lossy(value).into_owned());
            let file = path(file);
            self.record(call, Operation::Write, &file)?;
            let process = std::str::from_utf8(value)
                .ok()
                .and_then(|value| value.trim_end().parse::<u32>().ok());
            match process {
                Some(process) if file.ends_with("cgroup.procs") => {
                    let pinned = self.with(|state| {
                        state
                            .unmovable
                            .iter_mut()
                            .find(|(pinned, writes)| *pinned == process && *writes > 0)
                            .map(|(_, writes)| *writes -= 1)
                            .is_some()
                    });
                    if pinned {
                        Ok(())
                    } else {
                        move_process(&file, process)
                    }
                }
                _ => std::fs::write(&file, value).map_err(|_| Errno::EIO),
            }
        }

        fn is_mount_point(&mut self, path: &Path) -> std::io::Result<bool> {
            // The kernel's answer needs the directory, too.
            std::fs::metadata(path)?;
            Ok(self.with(|state| state.mounted.iter().any(|(point, _)| point == path)))
        }

        fn mount_options(&mut self, path: &Path) -> std::io::Result<String> {
            self.with(|state| {
                let failure = state
                    .failures
                    .iter()
                    .find(|(failing, failing_path, _)| {
                        *failing == Operation::Options && failing_path == path
                    })
                    .map(|&(_, _, error)| error);
                if let Some(error) = failure {
                    return Err(error.into());
                }
                if !state.mounted.iter().any(|(point, _)| point == path) {
                    return Err(Errno::ENOENT.into());
                }
                if let Some((_, options)) =
                    state.shown.iter().rev().find(|(point, _)| point == path)
                {
                    return Ok(options.clone());
                }
                let (flags, data) = state
                    .calls
                    .iter()
                    .rev()
                    .find_map(|call| match call {
                        Call::Mount {
                            target,
                            flags,
                            data,
                            ..
                        } if Path::new(target) == path
                            && !flags.contains(MountFlags::MS_REMOUNT) =>
                        {
                            Some((*flags, data.clone()))
                        }
                        _ => None,
                    })
                    .ok_or(Errno::ENOENT)?;
                let access = if flags.contains(MountFlags::MS_RDONLY) {
                    "ro"
                } else {
                    "rw"
                };
                Ok(data.map_or_else(|| access.to_owned(), |data| format!("{access},{data}")))
            })
        }

        fn sleep(&mut self, _duration: Duration) {
            let (sleeps, mut action) = self.with(|state| {
                state.sleeps += 1;
                (state.sleeps, state.on_sleep.take())
            });
            if let Some(action) = action.as_mut() {
                action(sleeps);
            }
            self.with(|state| {
                if state.on_sleep.is_none() {
                    state.on_sleep = action;
                }
            });
        }
    }

    impl MappingKernel for FakeKernel {
        fn open_beneath(
            &mut self,
            directory: &FileDescriptor,
            relative: &CStr,
            flags: OpenFlags,
            mode: Mode,
            cross_mounts: bool,
        ) -> Result<FileDescriptor, Errno> {
            let base = path_of(directory)?;
            self.check(Operation::Open, &base.join(path(relative)))?;
            let opened = sys::open_beneath(directory, relative, flags, mode, cross_mounts)?;
            if !cross_mounts {
                let reached = path_of(&opened)?;
                let crossed =
                    self.with(|state| state.mount_of(&base).0 != state.mount_of(&reached).0);
                if crossed {
                    return Err(Errno::EXDEV);
                }
            }
            Ok(opened)
        }

        fn status(&mut self, descriptor: &FileDescriptor) -> Result<FileStatus, Errno> {
            if self.tree(descriptor)?.is_none() {
                self.check(Operation::Status, &path_of(descriptor)?)?;
            }
            self.status_of(descriptor)
        }

        fn whole_file_system(&mut self, mount: u64) -> Result<bool, Errno> {
            let (point, whole) = self.with(|state| {
                let point = state
                    .mounted
                    .iter()
                    .find(|(_, mounted)| *mounted == mount)
                    .map(|(point, _)| point.clone());
                (point, !state.subtrees.contains(&mount))
            });
            // As the kernel does, it finds only a mount that is attached.
            let point = match point {
                Some(point) => point,
                None if mount == BASE_MOUNT => PathBuf::from("/"),
                None => return Err(Errno::ENOENT),
            };
            self.check(Operation::MountRoot, &point)?;
            Ok(whole)
        }

        fn create_directory(
            &mut self,
            directory: &FileDescriptor,
            name: &CStr,
            mode: Mode,
        ) -> Result<(), Errno> {
            let created = path_of(directory)?.join(path(name));
            self.check(Operation::Create, &created)?;
            sys::create_directory_at(directory, name, mode)?;
            self.created(&created);
            Ok(())
        }

        fn create_file(
            &mut self,
            directory: &FileDescriptor,
            name: &CStr,
            mode: Mode,
        ) -> Result<(), Errno> {
            let created = path_of(directory)?.join(path(name));
            self.check(Operation::Create, &created)?;
            sys::create_file_at(directory, name, mode)?;
            self.created(&created);
            Ok(())
        }

        fn change_mode(&mut self, descriptor: &FileDescriptor, mode: Mode) -> Result<(), Errno> {
            self.check(Operation::ChangeMode, &path_of(descriptor)?)?;
            sys::change_mode(descriptor, mode)
        }

        fn open_tree(
            &mut self,
            descriptor: &FileDescriptor,
            clone: bool,
        ) -> Result<FileDescriptor, Errno> {
            let source = path_of(descriptor)?;
            let call = Call::OpenTree {
                source: source.display().to_string(),
                clone,
            };
            self.record(call, Operation::OpenTree, &source)?;
            let status = self.status_of(descriptor)?;
            let number = self.with(|state| {
                let mount = if clone {
                    state.next_mount()
                } else {
                    status.mount
                };
                state.trees.push(Tree {
                    source,
                    kind: status.kind,
                    mount,
                });
                state.trees.len() - 1
            });
            let name = format!("nvx-fake-tree-{number}");
            let flags = nix::sys::memfd::MFdFlags::MFD_CLOEXEC;
            let file = nix::sys::memfd::memfd_create(name.as_str(), flags)?;
            Ok(FileDescriptor::from_owned(file))
        }

        fn set_attributes(
            &mut self,
            tree: &FileDescriptor,
            set: MountAttributes,
            clear: MountAttributes,
        ) -> Result<(), Errno> {
            let (_, tree) = self.tree(tree)?.ok_or(Errno::EINVAL)?;
            let call = Call::SetAttributes {
                source: tree.source.display().to_string(),
                set,
                clear,
            };
            self.record(call, Operation::SetAttributes, &tree.source)
        }

        fn move_mount(
            &mut self,
            tree: &FileDescriptor,
            target: &FileDescriptor,
        ) -> Result<(), Errno> {
            let (_, tree) = self.tree(tree)?.ok_or(Errno::EINVAL)?;
            let mut action = self.with(|state| state.on_move.take());
            if let Some(action) = action.as_mut() {
                action(&path_of(target)?);
            }
            self.with(|state| {
                if state.on_move.is_none() {
                    state.on_move = action;
                }
            });
            // The target's path now, after any rename.
            let point = path_of(target)?;
            let call = Call::MoveMount {
                source: tree.source.display().to_string(),
                target: point.display().to_string(),
            };
            self.record(call, Operation::MoveMount, &point)?;
            self.with(|state| {
                state.mounted.retain(|(_, mount)| *mount != tree.mount);
                state.mounted.push((point, tree.mount));
            });
            Ok(())
        }

        fn unmount_beneath(
            &mut self,
            directory: &FileDescriptor,
            name: &CStr,
        ) -> Result<(), Errno> {
            let point = path_of(directory)?.join(path(name));
            let (mount, root) = self.with(|state| state.mount_of(&point));
            let call = Call::Unmount(point.display().to_string());
            self.record(call, Operation::Unmount, &point)?;
            if !root || mount == BASE_MOUNT {
                return Err(Errno::EINVAL);
            }
            self.with(|state| state.mounted.retain(|(_, mounted)| *mounted != mount));
            Ok(())
        }

        fn detach(&mut self, tree: &FileDescriptor) -> Result<(), Errno> {
            let (_, tree) = self.tree(tree)?.ok_or(Errno::EINVAL)?;
            let point = self.with(|state| {
                state
                    .mounted
                    .iter()
                    .find(|(_, mount)| *mount == tree.mount)
                    .map(|(point, _)| point.clone())
            });
            let point = point.ok_or(Errno::EINVAL)?;
            let call = Call::Detach(point.display().to_string());
            self.record(call, Operation::Detach, &point)?;
            self.with(|state| state.mounted.retain(|(_, mounted)| *mounted != tree.mount));
            Ok(())
        }
    }
}
