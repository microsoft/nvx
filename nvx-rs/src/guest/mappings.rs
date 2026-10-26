//! Bind mounts of host mappings beneath the descriptors of their roots.
//!
//! A [`Mapping`] names a source below a source root, such as the guest's mount of the host's
//! export; a target below a target root, the tree that the mappings go into; and the target's
//! [`Access`]. A [`Mapper`] applies mappings beneath one [`TargetRoot`], in order, from a
//! [`SourceRoot`], tracks them, and unmounts them at its [`teardown`](Mapper::teardown). [`bind`]
//! applies one runtime bind, such as a generated machine ID, a resolver file, or the runtime's time
//! status, through the same walk and attach, without the checks of a host mapping's target and
//! without tracking.
//!
//! # Walk
//!
//! No path string names an object that a mapping uses. The mapper opens a target's components
//! one at a time, each with `openat2` beneath the descriptor of the one before it, starting from
//! the target root's, with `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS`, and
//! `RESOLVE_NO_MAGICLINKS`: a symbolic link or a magic link at any component, the last
//! included, is refused, and no path leads above the root. It opens a source beneath its root
//! the same way, in one call, and with `RESOLVE_NO_XDEV` too, so that a source is on its root's
//! mount. An empty source names the source root itself, such as the root of a directory export,
//! which it opens as `.` beneath the root, with the same flags. A target root is the root of a
//! mount, and that mount, like each that the caller adds with [`TargetRoot::add_mount`], shows its
//! whole file system, which `statmount` tells, rather than being a bind of a directory below that
//! file system's root: no rename, through it or another mount of its file system, can move a file
//! out of it, so nothing that the walk opened on one of the root's own mounts leaves the root,
//! whatever is renamed meanwhile.
//!
//! # Creation
//!
//! A missing component of a target is created, as a directory with mode 0755, or, for the last
//! component of a file's mapping, as an empty regular file with mode 0644, both exactly, whatever
//! the caller's umask or a default ACL of its parent, either of which could withhold
//! permissions, so that a workload that is not root reaches the mapping. It is created only
//! where `statx` shows its parent directory on one of the root's own mounts, by their unique
//! mount IDs: the target root's mount, such as a layered root's overlay, and those that the
//! caller adds with [`TargetRoot::add_mount`], such as a scratch file system of a root without
//! an overlay. So nothing is created inside another mapping, nor on another mount, not even on
//! a bind of the root's own file system, which has the device number of its source but a mount
//! ID of its own.
//!
//! A file gets its mode through the descriptor that its exclusive creation returns, which
//! refers to that file alone. A directory is created with no permissions, which neither the
//! umask nor an ACL can narrow, and the mapper then opens the component as it opens an existing
//! one, and checks whatever is there then as such. It gives the directory its mode through that
//! descriptor only if it still has no permissions and the mapper's user as its owner, as the
//! directory that it created has: a directory that a rename put in its place keeps its mode,
//! unless it, too, has no permissions and that owner, which no other user can give a directory,
//! nor move such a directory into its place from another. The mapper changes nothing that
//! existed already.
//!
//! # Checks
//!
//! Before it opens anything for a mapping, a mapper refuses a target that is:
//!
//! - reserved: in the `/run/nvx/time` tree, where the runtime binds its time status, or in one of
//!   the root profile's [`Reserved`] trees; an ancestor of any of them, such as `/run` or
//!   `/run/nvx`, which would shadow them; or one of the profile's reserved paths;
//! - an earlier mapping's target, or an ancestor of one, which would shadow it.
//!
//! The target then takes the kind of its source: a directory onto a directory, or a regular file
//! onto a regular file. A source of any other kind, or of another kind than
//! [`Mapping::with_kind`] asks for, and an existing target of another kind than its source, are
//! refused. So is a target on a mount that is neither the root's own nor an earlier mapping's,
//! and one that is the root of a mount already, which the mapping would shadow, unless the root
//! owns that mount, such as the tmpfs at a guest's `/tmp`, which a mapping of the host's `/tmp`
//! then covers until the teardown.
//!
//! # Attach
//!
//! The mapper never exposes a mapping without its restrictions: `open_tree` with
//! `OPEN_TREE_CLONE` copies the source's mount alone, detached; `mount_setattr` sets `nosuid`,
//! `nodev`, and the access's read-only flag on the copy; and only then `move_mount` attaches the
//! copy onto the target's descriptor. Nothing is bound first and restricted later, and a failure
//! before the attach leaves nothing mounted. The mapper then opens the target again from the
//! root, and checks that it is the new mount, so a mapping that it reports is at its target: one
//! that a rename moved while the walk ran is detached at once, as a lazy unmount does, wherever
//! it went, and refused. If that detach fails too, a mapper keeps the mapping's descriptor, and
//! detaches it at its teardown.
//!
//! The mounts are made in the caller's mount namespace, which must be recursively private, so
//! that they propagate nowhere: a guest's initial namespace, whose mounts are private unless its
//! init made them shared, or a namespace that the caller unshared and made private. The mapper
//! never changes the propagation of the caller's mounts.
//!
//! # Order and failure
//!
//! [`Mapper::apply`] applies a batch of mappings in order. Zero mappings do no work. The first
//! mapping that is refused or fails is reported with its index; the mappings before it stay
//! mapped, and the mapper refuses every later one, in this batch or another: the failure latches
//! for the mapper's lifetime, so that its caller starts nothing on a partial table.
//!
//! # Teardown
//!
//! [`Mapper::teardown`] first detaches the mapping that a rename moved, if its attach failed to
//! detach it, through the descriptor that the mapper kept, then unmounts the mappings in reverse
//! order. It opens each target again from the root, checks that the topmost mount there is the
//! mapping's, and unmounts it by its name in its parent directory, whose descriptor it opened
//! from the root, without following a link there and without detaching a busy mount; this needs
//! procfs at `/proc`. It attempts each, whatever failed before, and reports every failure. A
//! mapper that is dropped leaves its mappings mounted.
//!
//! NVX's managed agent applies its host mapping table through a mapper beneath its own root, `/`,
//! from its mount of the host's export, an aggregate, whose root, which lists the aggregate's
//! children, it never maps: it refuses an empty source. A layered [`Root`](super::root::Root)
//! mounts its live shares, and the mappings that its caller applies, through one, which its
//! teardown unmounts first.
//!
//! ```no_run
//! use std::path::Path;
//!
//! use nvx::guest::mappings::{Access, Mapper, Mapping, Reserved, SourceRoot, TargetRoot};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let root = TargetRoot::open(Path::new("/newroot"))?;
//! let mut mapper = Mapper::new(root, Reserved::container_root());
//! let export = SourceRoot::open(Path::new("/run/export"))?;
//! // Mounts the export's 0/workspace at /newroot/workspace, read-write, nosuid, and nodev.
//! let workspace = Mapping::new("0/workspace", "/workspace", Access::ReadWrite)?;
//! mapper.apply(&export, &[workspace])?;
//! mapper.teardown()?;
//! # Ok(())
//! # }
//! ```

mod kernel;
#[cfg(test)]
mod privileged;
#[cfg(test)]
mod tests;

use std::ffi::{CStr, OsStr};
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::sys::{
    self, Errno, FileDescriptor, FileKind, Mode, MountAttributes, MountFlags, OpenFlags,
};
use crate::protocol::share::{RESERVED_EXACT_MOUNT_TARGETS, RESERVED_MOUNT_TARGETS};

pub(crate) use self::kernel::{Kernel, SystemKernel};

/// The longest source or target of a [`Mapping`], in bytes: Linux's `PATH_MAX` without its
/// terminating NUL.
pub const MAX_PATH_BYTES: usize = sys::PATH_MAX - 1;

/// The tree where the runtime binds its time status, below the target root, which every mapper
/// reserves: no mapping may be in it, or hold it.
const RUNTIME_TREE: &[u8] = b"run/nvx/time";
/// The mode of each directory that a mapper creates, whatever the umask or a default ACL:
/// `rwxr-xr-x`.
const DIRECTORY_MODE: Mode = Mode::S_IRWXU
    .union(Mode::S_IRGRP)
    .union(Mode::S_IXGRP)
    .union(Mode::S_IROTH)
    .union(Mode::S_IXOTH);
/// The mode of each file target that a mapper creates, whatever the umask or a default ACL:
/// `rw-r--r--`.
const FILE_MODE: Mode = Mode::S_IRUSR
    .union(Mode::S_IWUSR)
    .union(Mode::S_IRGRP)
    .union(Mode::S_IROTH);

/// Whether programs can write a mapping's target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// The target is read-only.
    ReadOnly,
    /// The target is writable, as far as the source allows.
    ReadWrite,
}

impl Access {
    /// Returns the flags that restrict a mount with this access: `nosuid` and `nodev`, and `ro`
    /// for [`Access::ReadOnly`].
    pub(crate) fn flags(self) -> MountFlags {
        let flags = MountFlags::MS_NOSUID | MountFlags::MS_NODEV;
        match self {
            Self::ReadOnly => flags | MountFlags::MS_RDONLY,
            Self::ReadWrite => flags,
        }
    }

    /// Returns the attributes that a mount with this access gets, and those that it loses:
    /// `nosuid` and `nodev`, and the read-only flag, which [`Access::ReadOnly`] sets and
    /// [`Access::ReadWrite`] clears.
    fn attributes(self) -> (MountAttributes, MountAttributes) {
        let restricted = MountAttributes::NO_SET_ID | MountAttributes::NO_DEVICES;
        match self {
            Self::ReadOnly => (
                restricted | MountAttributes::READ_ONLY,
                MountAttributes::empty(),
            ),
            Self::ReadWrite => (restricted, MountAttributes::READ_ONLY),
        }
    }
}

/// What a mapping maps: a directory onto a directory, or a regular file onto a regular file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A directory.
    Directory,
    /// A regular file.
    File,
}

impl Kind {
    /// Returns the kind of a file of `kind`, if a mapping can map it.
    fn of(kind: FileKind) -> Option<Self> {
        match kind {
            FileKind::Directory => Some(Self::Directory),
            FileKind::Regular => Some(Self::File),
            FileKind::Other => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Directory => "directory",
            Self::File => "regular file",
        })
    }
}

/// The path that [`Mapping::new`] or [`Reserved::new`] refuses, because it does not name a path
/// below its root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InvalidMapping {
    /// The source is neither empty, for the source root itself, nor a relative path of at most
    /// [`MAX_PATH_BYTES`] bytes without NUL bytes, whose components are neither empty, `.`, nor
    /// `..`.
    Source,
    /// The target is not an absolute path of at most [`MAX_PATH_BYTES`] bytes without NUL bytes,
    /// whose components after the leading `/` are neither empty, `.`, nor `..`.
    Target,
}

impl fmt::Display for InvalidMapping {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (path, form) = match self {
            Self::Source => ("source", "empty, for its root, or a relative"),
            Self::Target => ("target", "an absolute"),
        };
        write!(
            formatter,
            "a mapping {path} must be {form} path of at most {MAX_PATH_BYTES} bytes without NUL \
             bytes or empty, `.`, or `..` components"
        )
    }
}

impl std::error::Error for InvalidMapping {}

/// A host mapping: a source below a source root, which a [`Mapper`] mounts at a target below a
/// target root, with an [`Access`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Mapping {
    /// The source, relative to the source root.
    source: PathBuf,
    /// The target, relative to the target root: the absolute target that [`Mapping::new`] took,
    /// without its leading `/`.
    target: PathBuf,
    /// Whether programs can write the target.
    access: Access,
    /// The kind that the source must have, if the mapping asks for one.
    kind: Option<Kind>,
}

impl Mapping {
    /// Describes the mapping of `source`, a path relative to the source root, at `target`, an
    /// absolute path as seen from the target root, with `access`: for example `0/workspace` at
    /// `/workspace`. An empty `source` names the source root itself, such as the root of a
    /// directory export, whose whole directory the mapping then maps. The target takes the kind of
    /// the source, whatever it is, unless [`with_kind`](Self::with_kind) asks for one.
    ///
    /// Each path holds at most [`MAX_PATH_BYTES`] bytes, no NUL byte, and no empty, `.`, or `..`
    /// component, so that it names a path below its root, or, for an empty source, the root
    /// itself; [`InvalidMapping`] tells which one does not. Other bytes need not be UTF-8.
    pub fn new(
        source: impl Into<PathBuf>,
        target: impl Into<PathBuf>,
        access: Access,
    ) -> Result<Self, InvalidMapping> {
        let source = source.into();
        if !source.as_os_str().is_empty() && below_its_root(&source, false).is_none() {
            return Err(InvalidMapping::Source);
        }
        let target = below_its_root(&target.into(), true)
            .ok_or(InvalidMapping::Target)?
            .to_path_buf();
        Ok(Self {
            source,
            target,
            access,
            kind: None,
        })
    }

    /// Returns the mapping, which maps its source only if it is of `kind`.
    pub fn with_kind(mut self, kind: Kind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Returns the source, relative to the source root: empty for the source root itself.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Returns the target, relative to the target root: the target that [`new`](Self::new) took,
    /// without its leading `/`, such as `workspace` for `/workspace`.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Returns whether programs can write the target.
    pub fn access(&self) -> Access {
        self.access
    }

    /// Returns the kind that the source must have, if the mapping asks for one.
    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }
}

/// The targets that a root profile reserves for its runtime, which no mapping may take, besides
/// the `/run/nvx/time` tree that every [`Mapper`] reserves: trees, which a mapping may be neither
/// in nor an ancestor of, and paths, at which a mapping may not be, though it may be below them.
///
/// The default reserves nothing more.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Reserved {
    /// The reserved trees, relative to the target root.
    trees: Vec<PathBuf>,
    /// The reserved paths, relative to the target root.
    paths: Vec<PathBuf>,
}

impl Reserved {
    /// Reserves `trees` and `paths`, absolute paths as seen from the target root, which must be
    /// canonical, as a [`Mapping`]'s target is: otherwise [`InvalidMapping::Target`].
    pub fn new<T, P>(trees: T, paths: P) -> Result<Self, InvalidMapping>
    where
        T: IntoIterator,
        T::Item: AsRef<Path>,
        P: IntoIterator,
        P::Item: AsRef<Path>,
    {
        let relative = |path: &Path| {
            below_its_root(path, true)
                .map(Path::to_path_buf)
                .ok_or(InvalidMapping::Target)
        };
        Ok(Self {
            trees: trees
                .into_iter()
                .map(|tree| relative(tree.as_ref()))
                .collect::<Result<_, _>>()?,
            paths: paths
                .into_iter()
                .map(|path| relative(path.as_ref()))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Returns the targets that NVX's container runtime reserves in a container root, which
    /// [`protocol::share`](crate::protocol::share) lists for the host and the guest: the trees
    /// `/proc`, `/sys`, `/dev`, `/.nvx-agent`, `/etc/machine-id`, `/etc/resolv.conf`, and
    /// `/run/nvx/time`, and the paths `/etc`, `/run`, and `/run/nvx`.
    pub fn container_root() -> Self {
        Self::new(RESERVED_MOUNT_TARGETS, RESERVED_EXACT_MOUNT_TARGETS)
            .expect("the container runtime's reserved targets are canonical")
    }

    /// Returns whether the relative `target` is reserved: in a reserved tree, or an ancestor of
    /// one, or a reserved path. The runtime's tree is always reserved.
    fn holds(&self, target: &[u8]) -> bool {
        let tree = |tree: &[u8]| within(target, tree) || within(tree, target);
        tree(RUNTIME_TREE)
            || self.trees.iter().any(|reserved| tree(bytes(reserved)))
            || self.paths.iter().any(|reserved| bytes(reserved) == target)
    }
}

/// The root beneath which a [`Mapper`] maps: a descriptor of the root of a mount of a whole file
/// system, and the root's own mounts, each of a whole file system too, on which the mapper may
/// create what a target lacks.
#[derive(Debug)]
pub struct TargetRoot {
    /// The root's directory, opened with `O_PATH`.
    directory: FileDescriptor,
    /// The unique IDs of the root's own mounts: the root's mount first.
    mounts: Vec<u64>,
}

impl TargetRoot {
    /// Opens the directory `path`, which must be the root of a mount that shows its whole file
    /// system, not a bind of a directory below that file system's root, and whose mount becomes
    /// the root's own: for example a layered root's overlay, or a guest's `/`. A final symbolic
    /// link at `path` is not followed. It needs Linux 6.8 or later, for the unique mount IDs and
    /// `statmount`.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with(&mut SystemKernel, path)
    }

    /// Adds the mount that the directory `path` is on, which must show its whole file system, to
    /// the root's own mounts, on which the mapper may create what a target lacks, such as the
    /// scratch file system of a root without an overlay, and whose root a mapping may cover. A
    /// final symbolic link at `path` is not followed.
    pub fn add_mount(&mut self, path: &Path) -> io::Result<()> {
        self.add_mount_with(&mut SystemKernel, path)
    }

    /// Implements [`open`](Self::open) with `kernel`.
    pub(crate) fn open_with(kernel: &mut dyn Kernel, path: &Path) -> io::Result<Self> {
        let directory = open_directory(path)?;
        let status = kernel.status(&directory)?;
        if !status.mount_root {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not the root of a mount", path.display()),
            ));
        }
        check_whole(kernel, status.mount, path)?;
        Ok(Self {
            directory,
            mounts: vec![status.mount],
        })
    }

    /// Implements [`add_mount`](Self::add_mount) with `kernel`.
    pub(crate) fn add_mount_with(
        &mut self,
        kernel: &mut dyn Kernel,
        path: &Path,
    ) -> io::Result<()> {
        let mount = kernel.status(&open_directory(path)?)?.mount;
        check_whole(kernel, mount, path)?;
        if !self.owns(mount) {
            self.mounts.push(mount);
        }
        Ok(())
    }

    /// Returns whether the mount `mount` is one of the root's own.
    fn owns(&self, mount: u64) -> bool {
        self.mounts.contains(&mount)
    }
}

/// The root beneath which a [`Mapper`] or [`bind`] opens the sources of mappings, such as the
/// guest's mount of the host's export: a descriptor of a directory.
#[derive(Debug)]
pub struct SourceRoot {
    /// The root's directory, opened with `O_PATH`.
    directory: FileDescriptor,
}

impl SourceRoot {
    /// Opens the directory `path`, not through a final symbolic link.
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            directory: open_directory(path)?,
        })
    }
}

/// A step of applying or unmounting a mapping, at which a system call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Step {
    /// Opening the source beneath its root: a missing source fails with `ENOENT`, a symbolic or
    /// magic link on its path with `ELOOP`, and a path that crosses into another mount with
    /// `EXDEV`.
    Source,
    /// Opening a component of the target.
    Walk,
    /// Creating a missing component of the target.
    Create,
    /// Reading the status of an open file.
    Status,
    /// Copying the source's mount, detached.
    Clone,
    /// Restricting the copy to the mapping's access, `nosuid`, and `nodev`.
    Restrict,
    /// Attaching the copy at the target.
    Attach,
    /// Opening the target again to find the mapping.
    Locate,
    /// Unmounting the mapping.
    Unmount,
}

impl fmt::Display for Step {
    /// Writes the step as the verb phrase of a failure: `failed to {step}`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Source => "open the source",
            Self::Walk => "open a component of the target",
            Self::Create => "create a component of the target",
            Self::Status => "read the status of a file",
            Self::Clone => "copy the source's mount",
            Self::Restrict => "restrict the mount",
            Self::Attach => "attach the mount at the target",
            Self::Locate => "find the mapping at its target",
            Self::Unmount => "unmount the mapping",
        })
    }
}

/// Why a mapper refused a mapping, or failed to apply or unmount it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MappingError {
    /// An earlier mapping of the mapper failed, so it refuses every later one.
    Latched,
    /// The runtime reserves the target, or a tree below it.
    Reserved,
    /// The target is the target of an earlier mapping, or holds it, and would shadow it.
    Overlap {
        /// The earlier mapping's target.
        mapped: PathBuf,
    },
    /// The source is neither a directory nor a regular file, or is not of the mapping's kind.
    SourceKind,
    /// A component of the target, at this path, is a symbolic link or a magic link.
    Link {
        /// The component's path in the root.
        path: PathBuf,
    },
    /// A component of the target, at this path, is not a directory, though the target is below
    /// it.
    NotDirectory {
        /// The component's path in the root.
        path: PathBuf,
    },
    /// The target exists, but not as the kind of its source.
    TargetKind,
    /// The target, or a missing component of it, at this path, is not on one of the root's own
    /// mounts, where the mapper may create it, nor, for an existing target, inside an earlier
    /// mapping.
    Foreign {
        /// The path in the root.
        path: PathBuf,
    },
    /// The target is the root of a mount already, which the mapping would shadow, and the root
    /// does not own that mount.
    MountPoint,
    /// The mapping was not the topmost mount at its target once attached, since a rename moved a
    /// directory of the target while the walk ran, or, at a teardown, since a rename or another
    /// mount at the target since then.
    Moved {
        /// Whether the mapping stays mounted, wherever it is: because detaching it failed too,
        /// in which case a mapper's teardown detaches it, or because a teardown does not unmount
        /// what it does not find at its target.
        mounted: bool,
    },
    /// A system call failed at a step.
    System {
        /// The step.
        step: Step,
        /// The call's `errno`.
        error: i32,
    },
}

impl MappingError {
    /// Returns the `errno` that best describes the failure, such as a control protocol's status:
    /// the call's own for [`MappingError::System`], and otherwise `ECANCELED` for a latched
    /// mapper, `EPERM` for a reserved target, `EBUSY` for an overlap or a mount point, `EINVAL`
    /// for a kind, `ELOOP` for a link, `ENOTDIR` for a component that is not a directory, `EXDEV`
    /// for a foreign mount, and `EAGAIN` for a moved mapping.
    pub fn raw_os_error(&self) -> i32 {
        let error = match self {
            Self::Latched => Errno::ECANCELED,
            Self::Reserved => Errno::EPERM,
            Self::Overlap { .. } | Self::MountPoint => Errno::EBUSY,
            Self::SourceKind | Self::TargetKind => Errno::EINVAL,
            Self::Link { .. } => Errno::ELOOP,
            Self::NotDirectory { .. } => Errno::ENOTDIR,
            Self::Foreign { .. } => Errno::EXDEV,
            Self::Moved { .. } => Errno::EAGAIN,
            Self::System { error, .. } => return *error,
        };
        error as i32
    }
}

impl fmt::Display for MappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latched => formatter.write_str("an earlier mapping failed"),
            Self::Reserved => formatter.write_str("the runtime reserves the target"),
            Self::Overlap { mapped } => write!(
                formatter,
                "the target would shadow the mapping at {}",
                mapped.display()
            ),
            Self::SourceKind => formatter
                .write_str("the source is not a directory or a regular file of the mapping's kind"),
            Self::Link { path } => write!(
                formatter,
                "{} is a symbolic link or a magic link",
                path.display()
            ),
            Self::NotDirectory { path } => {
                write!(formatter, "{} is not a directory", path.display())
            }
            Self::TargetKind => {
                formatter.write_str("the target exists as another kind than its source")
            }
            Self::Foreign { path } => write!(
                formatter,
                "{} is not on one of the root's own mounts or inside an earlier mapping",
                path.display()
            ),
            Self::MountPoint => {
                formatter.write_str("the target is the root of a mount that the root does not own")
            }
            Self::Moved { mounted } => write!(
                formatter,
                "the mapping is not at its target{}",
                if *mounted {
                    ", and it stays mounted"
                } else {
                    ""
                }
            ),
            Self::System { step, error } => write!(
                formatter,
                "failed to {step}: {}",
                io::Error::from_raw_os_error(*error)
            ),
        }
    }
}

impl std::error::Error for MappingError {}

/// Which mapping of a batch a [`Mapper`] refused or failed to apply, from 0, and why.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ApplyError {
    /// The index of the mapping in its batch.
    entry: usize,
    /// Why it was refused or failed.
    error: MappingError,
}

impl ApplyError {
    /// Returns the index of the mapping in its batch, from 0.
    pub fn entry(&self) -> usize {
        self.entry
    }

    /// Returns why the mapping was refused or failed.
    pub fn error(&self) -> &MappingError {
        &self.error
    }
}

impl fmt::Display for ApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "mapping {} failed: {}", self.entry, self.error)
    }
}

impl std::error::Error for ApplyError {}

/// A mapping that a teardown failed to unmount, which stays mounted.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UnmountFailure {
    /// The mapping's target, as an absolute path in the root.
    target: PathBuf,
    /// Why it was not unmounted.
    error: MappingError,
}

impl UnmountFailure {
    /// Returns the mapping's target, as an absolute path in the root.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Returns why the mapping was not unmounted: [`MappingError::Moved`] if another mount is at
    /// its target, and otherwise the failed [`Step::Locate`] or [`Step::Unmount`], which is also
    /// the step of a mapping that a rename moved and that the teardown failed to detach.
    pub fn error(&self) -> &MappingError {
        &self.error
    }
}

impl fmt::Display for UnmountFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the mapping at {}: {}",
            self.target.display(),
            self.error
        )
    }
}

/// The mappings that a teardown failed to unmount, in the order in which it attempted them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TeardownError {
    /// The failures, which is never empty.
    failures: Vec<UnmountFailure>,
}

impl TeardownError {
    /// Returns the failures, in the order in which the teardown attempted them.
    pub fn failures(&self) -> &[UnmountFailure] {
        &self.failures
    }

    /// Returns the error of a teardown that failed to unmount the mapping at `target`, an
    /// absolute path in the root, with `error`, for the tests of other modules.
    #[cfg(test)]
    pub(crate) fn of(target: impl Into<PathBuf>, error: MappingError) -> Self {
        Self {
            failures: vec![UnmountFailure {
                target: target.into(),
                error,
            }],
        }
    }
}

impl fmt::Display for TeardownError {
    /// Writes each failure, separated by `; `.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, failure) in self.failures.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "failed to unmount {failure}")?;
        }
        Ok(())
    }
}

impl std::error::Error for TeardownError {}

/// A mapping that a mapper attached and tracks, or a runtime bind that [`bind_with`] made, which
/// [`unbind_with`] unmounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mapped {
    /// The target, relative to the root.
    target: PathBuf,
    /// The unique ID of the mapping's mount.
    mount: u64,
}

#[cfg(test)]
impl Mapped {
    /// Returns the target, relative to the root.
    pub(crate) fn target(&self) -> &Path {
        &self.target
    }
}

/// A mapping that a rename moved away from its target while the walk ran, which the attach
/// failed to detach, and which stays mounted wherever it went.
#[derive(Debug)]
struct Stray {
    /// The target, relative to the root.
    target: PathBuf,
    /// The descriptor of the mapping's mount, through which a teardown detaches it.
    tree: FileDescriptor,
}

/// The applier of a root's mappings, which checks, attaches, tracks, and unmounts them, as the
/// [module](self) describes.
#[derive(Debug)]
pub struct Mapper {
    /// The root beneath which it maps.
    root: TargetRoot,
    /// The targets that the root's profile reserves.
    reserved: Reserved,
    /// The mappings that it attached, in order.
    mapped: Vec<Mapped>,
    /// The mapping that a rename moved and that it failed to detach, if any, which its last
    /// attach left, since that failure latched it.
    stray: Option<Stray>,
    /// Whether a mapping failed, after which it refuses every later one.
    latched: bool,
}

impl Mapper {
    /// Returns a mapper of no mappings yet beneath `root`, whose profile reserves `reserved`.
    pub fn new(root: TargetRoot, reserved: Reserved) -> Self {
        Self {
            root,
            reserved,
            mapped: Vec::new(),
            stray: None,
            latched: false,
        }
    }

    /// Applies `mappings` beneath the root, in order, from `source`, as the [module](self)
    /// describes, and tracks them. It needs the privilege to mount.
    ///
    /// The first mapping that is refused or fails is reported with its index, after the mappings
    /// before it, which stay mapped, and the mapper refuses every later mapping from then on.
    /// Zero mappings do no work. A mapping that a rename moved, and that the mapper failed to
    /// detach, [`MappingError::Moved`] with `mounted`, is not tracked, but the teardown detaches
    /// it first.
    pub fn apply(&mut self, source: &SourceRoot, mappings: &[Mapping]) -> Result<(), ApplyError> {
        self.apply_with(&mut SystemKernel, source, mappings)
    }

    /// Checks the target of `mapping` as [`apply`](Self::apply) does before it opens anything:
    /// that it is neither reserved nor overlaps an earlier mapping.
    pub fn check(&self, mapping: &Mapping) -> Result<(), MappingError> {
        self.check_target(mapping.target())
    }

    /// Returns whether a mapping failed, after which the mapper refuses every later one.
    pub fn is_latched(&self) -> bool {
        self.latched
    }

    /// Returns the targets of the mappings that the mapper tracks, in the order in which it
    /// applied them, relative to the root.
    pub fn targets(&self) -> impl ExactSizeIterator<Item = &Path> {
        self.mapped.iter().map(|mapped| mapped.target.as_path())
    }

    /// Unmounts the mappings in reverse order, as the [module](self) describes, and reports every
    /// one that it failed to unmount, which stays mounted. It first detaches the mapping that a
    /// rename moved and that the mapper failed to detach, if any.
    pub fn teardown(self) -> Result<(), TeardownError> {
        self.teardown_with(&mut SystemKernel)
    }

    /// Implements [`teardown`](Self::teardown) with `kernel`.
    pub(crate) fn teardown_with(mut self, kernel: &mut dyn Kernel) -> Result<(), TeardownError> {
        let failures: Vec<UnmountFailure> = self
            .unmount_with(kernel)
            .into_iter()
            .map(|(_, failure)| failure)
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(TeardownError { failures })
        }
    }

    /// Implements [`apply`](Self::apply) with `kernel`.
    pub(crate) fn apply_with(
        &mut self,
        kernel: &mut dyn Kernel,
        source: &SourceRoot,
        mappings: &[Mapping],
    ) -> Result<(), ApplyError> {
        for (entry, mapping) in mappings.iter().enumerate() {
            self.attach_with(
                kernel,
                Source::Beneath(source, mapping.source()),
                mapping.target(),
                mapping.access(),
                mapping.kind(),
            )
            .map_err(|error| ApplyError { entry, error })?;
        }
        Ok(())
    }

    /// Moves the mount whose root `mount` refers to, such as a file system that the caller
    /// mounted where only it can reach, onto `target`, a path relative to the root, as the mapper
    /// attaches a directory's mapping: with `nosuid`, `nodev`, and `access` set while the mount is
    /// still where only the caller reaches it, and after the same checks. The mapper then tracks
    /// it as a mapping, or, as [`apply`](Self::apply) does, detaches it at the teardown if a
    /// rename moved it and detaching it failed.
    pub(crate) fn move_mount_with(
        &mut self,
        kernel: &mut dyn Kernel,
        mount: &FileDescriptor,
        target: &Path,
        access: Access,
    ) -> Result<(), MappingError> {
        self.attach_with(
            kernel,
            Source::Mount(mount),
            target,
            access,
            Some(Kind::Directory),
        )
    }

    /// Detaches the mapping that a rename moved, if the mapper failed to detach it, then unmounts
    /// the mappings in reverse order with `kernel`, forgets them all, and returns, in the order of
    /// the attempts, each that it failed to unmount, with its index among the mapper's attaches
    /// that left a mount: the moved mapping's, which the mapper attached last, is the number of
    /// its mappings.
    pub(crate) fn unmount_with(&mut self, kernel: &mut dyn Kernel) -> Vec<(usize, UnmountFailure)> {
        let mapped = std::mem::take(&mut self.mapped);
        let mut failures = Vec::new();
        if let Some(stray) = self.stray.take()
            && let Err(error) = kernel.detach(&stray.tree)
        {
            let target = absolute(&stray.target);
            let error = system(Step::Unmount, error);
            failures.push((mapped.len(), UnmountFailure { target, error }));
        }
        for (index, mapping) in mapped.iter().enumerate().rev() {
            if let Err(error) = unmount(kernel, &self.root, mapping) {
                let target = absolute(&mapping.target);
                failures.push((index, UnmountFailure { target, error }));
            }
        }
        failures
    }

    /// Checks the relative `target` as [`check`](Self::check) does.
    fn check_target(&self, target: &Path) -> Result<(), MappingError> {
        let target = bytes(target);
        if self.reserved.holds(target) {
            return Err(MappingError::Reserved);
        }
        match self
            .mapped
            .iter()
            .find(|mapped| within(bytes(&mapped.target), target))
        {
            Some(mapped) => Err(MappingError::Overlap {
                mapped: absolute(&mapped.target),
            }),
            None => Ok(()),
        }
    }

    /// Checks the relative `target`, attaches `source` there with `access`, and tracks the
    /// mapping, unless the mapper is latched; a refusal or failure latches it, and the mapper
    /// keeps a mapping that a rename moved and that the attach failed to detach.
    fn attach_with(
        &mut self,
        kernel: &mut dyn Kernel,
        source: Source<'_>,
        target: &Path,
        access: Access,
        kind: Option<Kind>,
    ) -> Result<(), MappingError> {
        if self.latched {
            return Err(MappingError::Latched);
        }
        let attached = self
            .check_target(target)
            .map_err(AttachFailure::from)
            .and_then(|()| {
                let mounts: Vec<u64> = self.mapped.iter().map(|mapped| mapped.mount).collect();
                attach(kernel, &self.root, &mounts, source, target, access, kind)
            });
        match attached {
            Ok(mount) => {
                self.mapped.push(Mapped {
                    target: target.to_path_buf(),
                    mount,
                });
                Ok(())
            }
            Err(failure) => {
                self.latched = true;
                self.stray = failure.stray.map(|tree| Stray {
                    target: target.to_path_buf(),
                    tree,
                });
                Err(failure.error)
            }
        }
    }
}

/// Binds the source of `mapping`, beneath `source`, at its target beneath `root`, with the walk,
/// the creation, the checks of kinds and mounts, and the attach of a [`Mapper`]'s mapping, as the
/// [module](self) describes, but without the checks of reserved targets and overlaps, which a
/// runtime's own binds, such as a generated machine ID over `/etc/machine-id`, would fail, and
/// without tracking it: it lasts as long as the mount namespace, or until its caller unmounts it.
/// So does a bind that a rename moved and that it failed to detach, wherever it went, which it
/// reports as [`MappingError::Moved`] with `mounted`. It needs the privilege to mount.
pub fn bind(root: &TargetRoot, source: &SourceRoot, mapping: &Mapping) -> Result<(), MappingError> {
    bind_with(&mut SystemKernel, root, source, mapping).map(drop)
}

/// Implements [`bind`] with `kernel`, and returns the bind, which [`unbind_with`] unmounts.
pub(crate) fn bind_with(
    kernel: &mut dyn Kernel,
    root: &TargetRoot,
    source: &SourceRoot,
    mapping: &Mapping,
) -> Result<Mapped, MappingError> {
    attach(
        kernel,
        root,
        &[],
        Source::Beneath(source, mapping.source()),
        mapping.target(),
        mapping.access(),
        mapping.kind(),
    )
    .map(|mount| Mapped {
        target: mapping.target().to_path_buf(),
        mount,
    })
    .map_err(|failure| failure.error)
}

/// Unmounts `binds`, which [`bind_with`] made beneath `root`, in reverse order, with `kernel`, as a
/// mapper's teardown unmounts its mappings: each that is the topmost mount at its target, by its
/// name in its parent directory, without detaching a busy one. It attempts each, whatever failed
/// before, and reports every one that it failed to unmount, which stays mounted.
pub(crate) fn unbind_with(
    kernel: &mut dyn Kernel,
    root: &TargetRoot,
    binds: &[Mapped],
) -> Result<(), TeardownError> {
    let mut failures = Vec::new();
    for bind in binds.iter().rev() {
        if let Err(error) = unmount(kernel, root, bind) {
            let target = absolute(&bind.target);
            failures.push(UnmountFailure { target, error });
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(TeardownError { failures })
    }
}

/// What a mapping attaches: the object at a path beneath a source root, whose mount the attach
/// copies, or a mount that it moves.
#[derive(Clone, Copy, Debug)]
enum Source<'a> {
    /// The object at a relative path beneath a source root.
    Beneath(&'a SourceRoot, &'a Path),
    /// The mount whose root a descriptor refers to.
    Mount(&'a FileDescriptor),
}

/// Why an attach failed, with the descriptor of the mapping's mount if a rename moved it and
/// detaching it failed too, so that it stays mounted wherever it went.
#[derive(Debug)]
struct AttachFailure {
    /// Why it failed.
    error: MappingError,
    /// The descriptor of the mapping's mount, if it stays mounted.
    stray: Option<FileDescriptor>,
}

impl From<MappingError> for AttachFailure {
    fn from(error: MappingError) -> Self {
        Self { error, stray: None }
    }
}

/// Attaches `source` at the relative `target` beneath `root`, with `access`, as the
/// [module](self) describes, where `mapped` are the mounts of the earlier mappings, inside which
/// an existing target may be, and returns the unique ID of the new mount. A source of another
/// kind than `kind`, if given, is refused.
fn attach(
    kernel: &mut dyn Kernel,
    root: &TargetRoot,
    mapped: &[u64],
    source: Source<'_>,
    target: &Path,
    access: Access,
    kind: Option<Kind>,
) -> Result<u64, AttachFailure> {
    let opened;
    let (copied, found, clone) = match source {
        Source::Beneath(source, path) => {
            let (descriptor, found) = open_source(kernel, source, path)?;
            opened = descriptor;
            (&opened, found, true)
        }
        Source::Mount(descriptor) => {
            let status = kernel
                .status(descriptor)
                .map_err(|error| system(Step::Source, error))?;
            if !status.mount_root {
                return Err(MappingError::SourceKind.into());
            }
            let found = Kind::of(status.kind).ok_or(MappingError::SourceKind)?;
            (descriptor, found, false)
        }
    };
    if kind.is_some_and(|kind| kind != found) {
        return Err(MappingError::SourceKind.into());
    }
    let destination = walk(kernel, root, mapped, target, found)?;
    let tree = kernel
        .open_tree(copied, clone)
        .map_err(|error| system(Step::Clone, error))?;
    let (set, clear) = access.attributes();
    kernel
        .set_attributes(&tree, set, clear)
        .map_err(|error| system(Step::Restrict, error))?;
    let mount = kernel
        .status(&tree)
        .map_err(|error| system(Step::Status, error))?
        .mount;
    kernel
        .move_mount(&tree, &destination)
        .map_err(|error| system(Step::Attach, error))?;
    match locate(kernel, root, target) {
        Ok((_, status)) if status.mount == mount && status.mount_root => Ok(mount),
        // The tree keeps the mount busy, so only a detach takes it off wherever it went.
        _ => match kernel.detach(&tree) {
            Ok(()) => Err(MappingError::Moved { mounted: false }.into()),
            Err(_) => Err(AttachFailure {
                error: MappingError::Moved { mounted: true },
                stray: Some(tree),
            }),
        },
    }
}

/// Opens `path` beneath `source`, or the source root itself for an empty `path`, and returns it
/// and its kind, which must be a mapping's.
fn open_source(
    kernel: &mut dyn Kernel,
    source: &SourceRoot,
    path: &Path,
) -> Result<(FileDescriptor, Kind), MappingError> {
    // `openat2` resolves no empty path, so the root itself is `.` beneath it, which the same
    // resolution confines to the root's own mount, as it does every other source.
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    let opened = sys::path_c_string(path).and_then(|path| {
        let descriptor = kernel.open_beneath(
            &source.directory,
            &path,
            OpenFlags::O_PATH,
            Mode::empty(),
            false,
        )?;
        let status = kernel.status(&descriptor)?;
        Ok((descriptor, status))
    });
    let (descriptor, status) = opened.map_err(|error| system(Step::Source, error))?;
    let kind = Kind::of(status.kind).ok_or(MappingError::SourceKind)?;
    Ok((descriptor, kind))
}

/// Opens each component of the relative `target` beneath `root`, creating each that is missing,
/// as a directory, or as a regular file for the last one of a file's mapping, and returns the
/// target, which must be of `kind`, on one of the root's own mounts or in one of `mapped`, and
/// not the root of a mount, unless the root owns that mount, which the mapping then covers.
fn walk(
    kernel: &mut dyn Kernel,
    root: &TargetRoot,
    mapped: &[u64],
    target: &Path,
    kind: Kind,
) -> Result<FileDescriptor, MappingError> {
    let components: Vec<&OsStr> = target.iter().collect();
    let mut current: Option<FileDescriptor> = None;
    for (index, component) in components.iter().enumerate() {
        let parent = current.as_ref().unwrap_or(&root.directory);
        let name =
            sys::c_string(component.as_bytes()).map_err(|error| system(Step::Walk, error))?;
        let wanted = if index + 1 == components.len() {
            kind
        } else {
            Kind::Directory
        };
        let refused = |error| match error {
            Errno::ELOOP => MappingError::Link {
                path: leading(target, index + 1),
            },
            Errno::ENOTDIR => MappingError::NotDirectory {
                path: leading(target, index),
            },
            error => system(Step::Walk, error),
        };
        let open = |kernel: &mut dyn Kernel| {
            kernel.open_beneath(parent, &name, OpenFlags::O_PATH, Mode::empty(), true)
        };
        let opened = match open(kernel) {
            Ok(opened) => opened,
            Err(Errno::ENOENT) => {
                let created = create(kernel, root, parent, &name, wanted, || {
                    leading(target, index + 1)
                })?;
                // What is there now, what it created or what another process put there first or
                // since, is checked as an existing component.
                let opened = open(kernel).map_err(refused)?;
                if created && wanted == Kind::Directory {
                    give_mode(kernel, &opened)?;
                }
                opened
            }
            Err(error) => return Err(refused(error)),
        };
        current = Some(opened);
    }
    let destination = current.ok_or_else(|| system(Step::Walk, Errno::EINVAL))?;
    let status = kernel
        .status(&destination)
        .map_err(|error| system(Step::Status, error))?;
    if Kind::of(status.kind) != Some(kind) {
        return Err(MappingError::TargetKind);
    }
    // The root of one of the root's own mounts, such as the tmpfs at a guest's `/tmp`, is a
    // target that the mapping covers; the root of any other, an earlier mapping's included, is
    // not.
    if status.mount_root && !root.owns(status.mount) {
        return Err(MappingError::MountPoint);
    }
    if !root.owns(status.mount) && !mapped.contains(&status.mount) {
        return Err(MappingError::Foreign {
            path: absolute(target),
        });
    }
    Ok(destination)
}

/// Creates `name` in `parent`, a directory on one of `root`'s own mounts, as a directory with no
/// permissions, which [`give_mode`] gives [`DIRECTORY_MODE`] once the walk opens it, or as an
/// empty regular file with exactly [`FILE_MODE`] for [`Kind::File`], whatever the umask or a
/// default ACL, and returns whether it created it: not if another process created an entry of
/// that name first. A parent on another mount is refused as [`MappingError::Foreign`] at
/// `path`.
fn create(
    kernel: &mut dyn Kernel,
    root: &TargetRoot,
    parent: &FileDescriptor,
    name: &CStr,
    kind: Kind,
    path: impl FnOnce() -> PathBuf,
) -> Result<bool, MappingError> {
    let status = kernel
        .status(parent)
        .map_err(|error| system(Step::Status, error))?;
    if !root.owns(status.mount) {
        return Err(MappingError::Foreign { path: path() });
    }
    let created = match kind {
        Kind::Directory => kernel.create_directory(parent, name, Mode::empty()),
        Kind::File => kernel.create_file(parent, name, FILE_MODE),
    };
    match created {
        Ok(()) => Ok(true),
        Err(Errno::EEXIST) => Ok(false),
        Err(error) => Err(system(Step::Create, error)),
    }
}

/// Gives the directory that `opened` refers to [`DIRECTORY_MODE`], through `opened`, if it is
/// the one that [`create`] created, with no permissions, before the walk opened it: if it still
/// has no permissions, and the mapper's user, its effective user ID, as its owner. A directory
/// that a rename put in its place keeps its mode. It keeps the set-group-ID bit that a directory
/// inherits from a parent that has it, as `mkdir` does.
fn give_mode(kernel: &mut dyn Kernel, opened: &FileDescriptor) -> Result<(), MappingError> {
    let status = kernel
        .status(opened)
        .map_err(|error| system(Step::Status, error))?;
    let permissions = Mode::S_IRWXU | Mode::S_IRWXG | Mode::S_IRWXO;
    let created = status.kind == FileKind::Directory
        && status.owner == nix::unistd::geteuid().as_raw()
        && status.mode & permissions.bits() == 0;
    if !created {
        return Ok(());
    }
    let inherited = Mode::from_bits_truncate(status.mode) & Mode::S_ISGID;
    kernel
        .change_mode(opened, DIRECTORY_MODE | inherited)
        .map_err(|error| system(Step::Create, error))
}

/// Opens the relative `target` beneath `root`, through the mounts below it, and returns it and
/// its status: the topmost mount at the target, if any.
fn locate(
    kernel: &mut dyn Kernel,
    root: &TargetRoot,
    target: &Path,
) -> Result<(FileDescriptor, sys::FileStatus), Errno> {
    let path = sys::path_c_string(target)?;
    let descriptor = kernel.open_beneath(
        &root.directory,
        &path,
        OpenFlags::O_PATH,
        Mode::empty(),
        true,
    )?;
    let status = kernel.status(&descriptor)?;
    Ok((descriptor, status))
}

/// Unmounts `mapping` beneath `root`, after checking that its mount is the topmost one at its
/// target: by its name in its parent directory, which it opens from the root, since a
/// descriptor of the mapping itself would keep it busy.
fn unmount(
    kernel: &mut dyn Kernel,
    root: &TargetRoot,
    mapping: &Mapped,
) -> Result<(), MappingError> {
    let located = (|| -> Result<_, Errno> {
        let parent = match mapping.target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                let path = sys::path_c_string(parent)?;
                let flags = OpenFlags::O_PATH | OpenFlags::O_DIRECTORY;
                Some(kernel.open_beneath(&root.directory, &path, flags, Mode::empty(), true)?)
            }
            _ => None,
        };
        let name = mapping.target.file_name().ok_or(Errno::EINVAL)?;
        let name = sys::c_string(name.as_bytes())?;
        let directory = parent.as_ref().unwrap_or(&root.directory);
        let status = kernel
            .open_beneath(directory, &name, OpenFlags::O_PATH, Mode::empty(), true)
            .and_then(|target| kernel.status(&target))?;
        Ok((parent, name, status))
    })();
    let (parent, name, status) = located.map_err(|error| system(Step::Locate, error))?;
    if status.mount != mapping.mount || !status.mount_root {
        return Err(MappingError::Moved { mounted: true });
    }
    kernel
        .unmount_beneath(parent.as_ref().unwrap_or(&root.directory), &name)
        .map_err(|error| system(Step::Unmount, error))
}

/// Opens the directory `path` with `O_PATH`, not through a final symbolic link.
fn open_directory(path: &Path) -> io::Result<FileDescriptor> {
    let flags =
        OpenFlags::O_PATH | OpenFlags::O_DIRECTORY | OpenFlags::O_NOFOLLOW | OpenFlags::O_CLOEXEC;
    let path = sys::path_c_string(path)?;
    Ok(sys::open(&path, flags, Mode::empty())?)
}

/// Refuses `mount`, the mount that `path` is on, unless it shows its whole file system: on a
/// bind of a directory, a rename through another mount of its file system may move what a walk
/// opened out of the bind.
fn check_whole(kernel: &mut dyn Kernel, mount: u64, path: &Path) -> io::Result<()> {
    if kernel.whole_file_system(mount)? {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{} is on a mount of a directory below its file system's root, not of the whole \
             file system",
            path.display()
        ),
    ))
}

/// Returns the failure of a system call at `step` with `error`.
fn system(step: Step, error: Errno) -> MappingError {
    MappingError::System {
        step,
        error: error as i32,
    }
}

/// Returns the bytes of `path`.
fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Returns whether the relative path `inner` is the relative path `outer` or below it.
fn within(inner: &[u8], outer: &[u8]) -> bool {
    inner
        .strip_prefix(outer)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
}

/// Returns the first `count` components of the relative `target`, as an absolute path.
fn leading(target: &Path, count: usize) -> PathBuf {
    let mut path = PathBuf::from("/");
    path.extend(target.iter().take(count));
    path
}

/// Returns the relative `target` as an absolute path.
fn absolute(target: &Path) -> PathBuf {
    Path::new("/").join(target)
}

/// Returns the part of `path` below its root: the whole of a relative path, or what follows the
/// leading `/` of an `absolute` one. It returns `None` unless `path` holds at most
/// [`MAX_PATH_BYTES`] bytes and no NUL byte, starts with `/` exactly when it is `absolute`, and
/// has a part below its root of at least one component, each neither empty, `.`, nor `..`.
pub(crate) fn below_its_root(path: &Path, absolute: bool) -> Option<&Path> {
    let bytes = path.as_os_str().as_bytes();
    let components = if absolute {
        bytes.strip_prefix(b"/")?
    } else {
        bytes
    };
    let valid = bytes.len() <= MAX_PATH_BYTES
        && !bytes.contains(&0)
        && !components.is_empty()
        && components
            .split(|&byte| byte == b'/')
            .all(|component| !matches!(component, b"" | b"." | b".."));
    valid.then(|| Path::new(OsStr::from_bytes(components)))
}
