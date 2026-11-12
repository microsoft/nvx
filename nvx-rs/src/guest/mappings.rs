//! Host mappings, and the host mapping table through which a guest agent applies them.
//!
//! A [`Mapping`] names a source below a source root, such as the guest's mount of the host's
//! export; a target below a target root, the tree that the mappings go into; the target's
//! [`Access`]; and, if it asks for one, the [`Kind`] that its source must have. A
//! [`MappingTable`] checks the parts of a host mapping table, as the
//! [last section](#host-mapping-table) describes, and hands their mappings to the root of its
//! caller's choice.
//!
//! NVX mounts mappings through its mapper, which binds each beneath the descriptor of its root.
//! The mapper, like the rest of the guest's space, is nvx-init's internal reference code, private
//! to this crate.
//!
//! # Host mapping table
//!
//! A host that maps host paths into its guest, such as NVX's managed lifecycle, announces how
//! many mappings its table holds, and once the guest agent is ready, sends the table in parts,
//! `MAPS` payloads of consecutive entries, each the source of a mapping in the host export, its
//! target, and its access. A [`MappingTable`] keeps the agent's account of the table: it checks
//! each part against what the host announced and against the entries that it applied, hands the
//! part's mappings to the caller, which applies them beneath the root of its choice, such as
//! through a mapper, and latches at the first that fails, which it reports with its entry in the
//! table, so that the agent starts no workload on a partial table. The caller keeps the mappings
//! until its teardown.
//!
//! NVX's managed agent applies its host mapping table through one, and through a mapper beneath
//! its own root, `/`, in direct mode, from its mount of the host's export, an aggregate, whose
//! root, which lists the aggregate's children, no table maps: a table refuses an empty source.

mod kernel;
pub(crate) mod mapper;
mod table;

use std::ffi::OsStr;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::sys::{self, FileKind, MountAttributes, MountFlags};

pub(crate) use self::kernel::{Kernel, SystemKernel};
pub(crate) use self::mapper::{
    Mapped, Mapper, MappingError, Reserved, SourceRoot, Step, TargetRoot, unbind_with,
};
pub use self::table::{MappingTable, Refusal, TableError};

/// The longest source or target of a [`Mapping`], in bytes: Linux's `PATH_MAX` without its
/// terminating NUL.
pub const MAX_PATH_BYTES: usize = sys::PATH_MAX - 1;

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

/// The path that [`Mapping::new`], or the mapper's `Reserved::new`, refuses, because it does not
/// name a path below its root.
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

/// A host mapping: a source below a source root, which a mapper mounts at a target below a target
/// root, with an [`Access`].
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
