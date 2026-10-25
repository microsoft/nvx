//! Host paths exposed to the guest through OpenVMM's aggregate virtio-fs export, as the one
//! planner of every NVX caller plans them, for the blockless root and for the container root of a
//! layered sandbox alike.
//!
//! OpenVMM offers one virtio-fs device. [`plan`] attaches it as an aggregate: a synthetic,
//! read-only root that lists one child per exported host directory or file, named by its index,
//! which the guest mounts at a directory that only the guest's root can enter. The guest then
//! bind-mounts each mapped path at its target, read-only or read-write, in the order of the plan's
//! [`Bind`]s: from the mapping table that it receives over the control channel, or from boot
//! tokens that a caller renders from the same binds.
//!
//! An [`ExportRequest`] names the mapped host paths, each read-only or read-write at its guest
//! target; the host paths that OpenVMM hides inside them, and the paths inside those that stay
//! visible; the paths to which the writes of a read-write directory are narrowed; the host
//! identity of the file operations; and the guest targets that the guest's root keeps for itself.
//!
//! - A caller gives each target, or lets MXC's convention for Linux guests derive it from the
//!   resolved host path: a Windows path `C:\work\src` appears at `/mnt/c/work/src`, and a Linux
//!   path at the same path ([`fn@guest_path`]).
//! - The reserved targets depend on the root, and the caller supplies them ([`ReservedTargets`]):
//!   the blockless root keeps the guest's system trees, and the container root of a layered
//!   sandbox keeps the trees of its runtime.
//!
//! The children are the outermost mapped directories and the mapped files outside them. OpenVMM
//! exports each such file alone, without its directory, so mapped paths may lie anywhere, on any
//! volume, except that a whole volume cannot be mapped. A child is read-write only if it holds a
//! read-write mapping, and OpenVMM then limits writes to the child's writable parts, so the host
//! enforces each mapping's access itself, and guest mount flags are never the boundary. OpenVMM
//! cannot keep a path read-only inside a writable part, so a read-only path there is refused on
//! every host, while a read-write path inside a read-only directory is not. OpenVMM also hides the
//! denied paths, except for the allowed paths inside them.
//!
//! The plan records the identities of the mapped and denied paths that a workload could move,
//! those inside a writable part, and [`ExportPlan::verify`] checks them again before each start.
//! A policy that would need more children than OpenVMM offers is refused rather than served
//! through the directories of its files, which would grant more than the files.

mod guest_path;
mod host_path;
mod limits;
mod policy;
mod verify;

use std::fs;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::protocol::MAX_HOST_MAPPINGS;
use crate::protocol::export::EXPORT_DIRECTORY;
use crate::protocol::share::{RESERVED_EXACT_MOUNT_TARGETS, RESERVED_MOUNT_TARGETS};

pub(crate) use self::guest_path::within;
use self::guest_path::{depth, guest_source};
pub use self::guest_path::{guest_path, resolve_guest_path};
pub(crate) use self::host_path::{canonicalize, is_reparse_point};
use self::host_path::{identity, relative_text};
use self::limits::{check_policy_paths, check_root};
use self::policy::{
    Writable, allowed_paths, canonicals, check_allowed_paths, check_read_only, check_write_paths,
    denied_paths, distinct_roots, nearest_rule, needed_denied_paths, writable_part, write_paths,
};

/// Guest directory where the guest's init mounts the export, which the export's `virtfs_dir=`
/// boot token names, and guests check: [`EXPORT_DIRECTORY`]. Only the guest's root can enter its
/// parent.
pub const GUEST_EXPORT: &str = EXPORT_DIRECTORY;

/// Largest number of host directories and files that OpenVMM exports through one aggregate.
pub const MAX_CHILDREN: usize = 256;
/// Largest number of hidden, allowed, or writable paths that OpenVMM accepts in one exported
/// directory.
const MAX_POLICY_PATHS: usize = 128;
/// Largest combined UTF-8 byte length of one exported directory's paths of one kind.
const MAX_POLICY_BYTES: usize = 16 * 1024;
/// Largest length of one such path.
const MAX_POLICY_PATH_BYTES: usize = 4096;
/// Largest combined UTF-8 byte length of all policies in one aggregate export.
const MAX_AGGREGATE_POLICY_BYTES: usize = 128 * 1024;
/// Longest guest path, without the terminating NUL.
const MAX_GUEST_PATH: usize = 4095;

/// Guest directories that a mapping must not cover in the blockless root, because the guest's own
/// system lives there.
pub const RESERVED_TARGETS: [&str; 13] = [
    "/bin", "/boot", "/dev", "/etc", "/lib", "/proc", "/root", "/run", "/sbin", "/sys", "/usr",
    "/var", "/init",
];

/// Why a request for the caller owner fails on a Windows host.
const CALLER_OWNER_ON_WINDOWS: &str = "the caller owner requires a Linux host; Windows has no \
                                       per-request POSIX identity for OpenVMM to switch to";

/// The guest targets that a root keeps for its own system or runtime, which no mapping may take.
///
/// A mapping may be neither at nor below one of the [`trees`](Self::trees), nor at one of their
/// ancestors, where its bind would shadow the tree, nor at one of the [`exact`](Self::exact)
/// paths, though it may be below them. No mapping is ever at the guest's root, `/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservedTargets<'a> {
    /// Guest trees that the root owns.
    pub trees: &'a [&'a str],
    /// Guest paths at which the root mounts or creates something, below which mappings may lie.
    pub exact: &'a [&'a str],
}

impl ReservedTargets<'static> {
    /// The blockless root, in which the guest's own system runs: [`RESERVED_TARGETS`].
    pub const BLOCKLESS_ROOT: Self = Self {
        trees: &RESERVED_TARGETS,
        exact: &[],
    };

    /// The container root of a layered sandbox: the trees in which its runtime mounts, binds, or
    /// stages something, [`RESERVED_MOUNT_TARGETS`], and exactly the directories on the way to
    /// them that it creates, [`RESERVED_EXACT_MOUNT_TARGETS`], which the guest checks as well.
    pub const CONTAINER_ROOT: Self = Self {
        trees: &RESERVED_MOUNT_TARGETS,
        exact: &RESERVED_EXACT_MOUNT_TARGETS,
    };
}

impl<'a> ReservedTargets<'a> {
    /// Returns the reserved path that a mapping at `target`, an absolute guest path, would take
    /// or shadow, or `None` if `target` is free.
    pub fn reservation(&self, target: &str) -> Option<&'a str> {
        if target == "/" {
            return Some("/");
        }
        self.exact
            .iter()
            .copied()
            .find(|exact| *exact == target)
            .or_else(|| {
                self.trees
                    .iter()
                    .copied()
                    .find(|tree| within(target, tree) || within(tree, target))
            })
    }
}

/// The host identity under which OpenVMM runs the file operations of an export.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    /// Every operation runs as OpenVMM, the default.
    #[default]
    Vmm,
    /// Each operation runs as the identity of the guest caller, with guest root squashed to the
    /// owner of the exported directory or file. Linux hosts only.
    Caller,
}

impl Owner {
    /// Returns whether this is the default owner, which the serde form of a plan leaves out.
    fn is_vmm(&self) -> bool {
        *self == Self::Vmm
    }
}

/// The host paths of one sandbox, as [`plan`] exports them: the children of OpenVMM's aggregate
/// export, the bind mounts of the guest, and the host identity of the file operations.
///
/// Callers record the plan, so its serde form is part of their state: a caller versions its
/// records, and a record of another version is not read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportPlan {
    /// Children of the aggregate export; each is named by its index.
    pub children: Vec<ExportChild>,
    /// Bind mounts the guest creates, parents before children.
    pub binds: Vec<Bind>,
    /// The host identity of the file operations; a plan of the default owner leaves it out.
    #[serde(default, skip_serializing_if = "Owner::is_vmm")]
    pub owner: Owner,
}

/// One host directory or file of the aggregate export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportChild {
    /// Canonical host directory, or the canonical host file that the child exposes alone.
    pub root: PathBuf,
    /// Whether the root is a regular file, which OpenVMM exposes without its directory; a child
    /// of a directory leaves it out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub file: bool,
    /// Whether the guest may modify it, which a read-write mapping inside it requires.
    pub writable: bool,
    /// Canonical host paths inside the child that OpenVMM hides; the root itself, when it shows
    /// only its allowed paths.
    pub denied: Vec<PathBuf>,
    /// Identities, at provision, of the denied paths that a workload could move: those inside a
    /// writable part.
    pub denied_identities: Vec<Option<FileIdentity>>,
    /// Canonical paths inside denied paths that stay visible.
    pub allowed: Vec<PathBuf>,
    /// Canonical paths to which OpenVMM limits the guest's writes; empty if the guest may modify
    /// everything that it can reach in a writable child.
    pub write: Vec<PathBuf>,
}

/// Device and file index of a host object, which survive renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileIdentity {
    /// Device, or the volume serial number on Windows.
    pub device: u64,
    /// Inode, or the file index on Windows.
    pub index: u64,
}

/// One mapped host path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Bind {
    /// Index of the child that holds the mapped path.
    pub child: usize,
    /// Path relative to the child's root, `/`-separated; empty for the root itself.
    pub source: String,
    /// Absolute guest path.
    pub target: String,
    /// Whether the guest mounts it read-only.
    pub read_only: bool,
    /// Identity of the host object at provision, when it lies inside a writable part and a
    /// workload could therefore move it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<FileIdentity>,
}

impl Bind {
    /// Path of the mapped object relative to the guest's mount of the export.
    pub fn guest_source(&self) -> String {
        guest_source(self.child, &self.source)
    }
}

/// Where the guest sees a mapped host path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// At the guest path that [`fn@guest_path`] derives from the resolved host path.
    Convention,
    /// At this absolute guest path: a path other than the root, of at most 4,095 bytes, without
    /// NUL characters or empty, `.`, or `..` components.
    Guest(&'a str),
}

/// A host directory or regular file that the guest reads, or reads and writes, at a target.
#[derive(Debug, Clone, Copy)]
pub struct Mapping<'a> {
    /// How messages name the list or option that requested the mapping, such as
    /// `filesystem.readonlyPaths`.
    pub name: &'a str,
    /// The host path.
    pub path: &'a Path,
    /// Where the guest sees it.
    pub target: Target<'a>,
    /// Whether the guest may only read it.
    pub read_only: bool,
}

impl<'a> Mapping<'a> {
    /// Returns the mappings of the paths of `list`, under its name, at the guest paths of the
    /// convention, read-only or read-write.
    pub fn convention(list: PathList<'a>, read_only: bool) -> impl Iterator<Item = Self> + 'a {
        list.paths.iter().map(move |path| Self {
            name: list.name,
            path,
            target: Target::Convention,
            read_only,
        })
    }
}

/// A list of host paths, and the name by which messages refer to its entries, such as
/// `readOnly`.
#[derive(Debug, Clone, Copy)]
pub struct PathList<'a> {
    /// The name of the list in messages.
    pub name: &'a str,
    /// The host paths.
    pub paths: &'a [PathBuf],
}

impl PathList<'static> {
    /// A list without paths.
    pub const EMPTY: Self = Self {
        name: "",
        paths: &[],
    };
}

/// The host paths that a sandbox maps, those that it hides inside them and shows again inside
/// those, the paths to which it narrows writes, the host identity of the file operations, and the
/// guest targets that the guest's root keeps for itself.
#[derive(Debug, Clone, Copy)]
pub struct ExportRequest<'a> {
    /// The mapped host paths, each at its target with its access.
    pub mappings: &'a [Mapping<'a>],
    /// Host paths hidden inside the mapped paths. A mapped directory may be denied itself when
    /// allowed paths inside it stay visible.
    pub denied: PathList<'a>,
    /// Host paths inside denied paths that stay visible, with the directories on the way to
    /// them listing only them.
    pub allowed: PathList<'a>,
    /// Host paths to which the guest's writes are narrowed inside the read-write directory that
    /// they lie in, which must be mapped whole: the guest may then write only them and the
    /// read-write mappings inside that directory.
    pub write: PathList<'a>,
    /// The host identity of the file operations.
    pub owner: Owner,
    /// The guest targets that the guest's root keeps for itself.
    pub reserved: ReservedTargets<'a>,
}

impl<'a> ExportRequest<'a> {
    /// Returns the request that maps `mappings` into a root that reserves `reserved`, without
    /// denied, allowed, or write paths, as the VMM.
    pub fn new(mappings: &'a [Mapping<'a>], reserved: ReservedTargets<'a>) -> Self {
        Self {
            mappings,
            denied: PathList::EMPTY,
            allowed: PathList::EMPTY,
            write: PathList::EMPTY,
            owner: Owner::Vmm,
            reserved,
        }
    }
}

/// Plans the export of `request`, or returns `None` if it maps nothing.
///
/// Mapped paths must exist, and resolve to paths that are valid UTF-8. A mapping's target is the
/// one that the request gives, or the guest path that the convention derives from the resolved
/// host path, so two spellings of one object (links, letter case, or short names) map to one guest
/// path; either must be free in the request's [`ReservedTargets`]. An object mapped twice at one
/// target is mapped once, read-only if either mapping is. Two objects never share a target, and a
/// target inside another mapping's must show the very object that the other mapping shows there.
/// A whole volume cannot be mapped, and a policy that would need more children than OpenVMM
/// offers is refused.
///
/// A read-only path must not lie inside a writable part, where the host could not keep it
/// read-only: inside a read-write directory, unless write paths narrow that directory's writes
/// to other paths, or inside a write path. A read-write path may lie inside a read-only
/// directory. A write path must lie inside a read-write directory that a child exports whole.
///
/// A denied path must not hide a mapped path, except that a denied mapped directory shows the
/// allowed paths inside it, unless it is a read-write mapping inside a directory that is not
/// writable as a whole: OpenVMM narrows that directory's writes to the mapping, and it cannot make
/// a hidden path writable. A denied path that does not exist yet must not lie inside a mapped
/// path, because nothing could hide it once a workload creates it; denied paths outside the
/// mapped paths are ignored. An allowed path must lie inside a denied path, with no other allowed
/// path between them, and a write path must not be hidden. The caller owner needs a Linux host,
/// on which no exported directory or file may belong to UID 0 or GID 0.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when the request breaks
/// these rules or exceeds a limit of OpenVMM or the guest agent.
pub fn plan(request: &ExportRequest<'_>) -> Result<Option<ExportPlan>> {
    let unsupported = |message: String| Err(Error::unsupported(message));
    if request.owner == Owner::Caller && cfg!(windows) {
        return unsupported(CALLER_OWNER_ON_WINDOWS.to_owned());
    }
    let mut mapped: Vec<Mapped<'_>> = Vec::new();
    for mapping in request.mappings {
        let entry = resolve_mapping(mapping, request.reserved)?;
        match mapped
            .iter_mut()
            .find(|other| other.canonical == entry.canonical && other.target == entry.target)
        {
            // The same object listed twice at one target keeps its most restrictive access, and
            // the name and spelling of the entry that gave it.
            Some(other) => {
                if entry.read_only && !other.read_only {
                    *other = entry;
                }
            }
            None => mapped.push(entry),
        }
    }
    if mapped.is_empty() {
        return Ok(None);
    }
    if mapped.len() > MAX_HOST_MAPPINGS {
        return unsupported(format!(
            "the openvmm backend maps at most {MAX_HOST_MAPPINGS} paths into one sandbox"
        ));
    }
    check_targets(&mapped)?;
    let roots = outermost(mapped.iter().map(|entry| entry.canonical.clone()).collect());
    let write = write_paths(request.write, &mapped, &roots)?;
    check_read_only(&mapped, &write, request.write.name, &roots)?;
    distinct_roots(&roots)?;
    if roots.len() > MAX_CHILDREN {
        // Exporting directories for some of the files would grant more than the files.
        return unsupported(format!(
            "the mapped paths need {} exported directories and files, but OpenVMM exports at \
             most {MAX_CHILDREN}; map directories that contain the files instead",
            roots.len()
        ));
    }
    let allowed = allowed_paths(request.allowed, &roots)?;
    let denied = denied_paths(request.denied, &mapped, &allowed)?;
    check_allowed_paths(request.allowed.name, request.denied.name, &allowed, &denied)?;
    check_write_paths(
        request.write.name,
        request.denied.name,
        &write,
        &denied,
        &allowed,
    )?;
    let denied = canonicals(&denied);
    let mut allowed = canonicals(&allowed);
    allowed.sort();

    let parts: Vec<Writable> = roots
        .iter()
        .map(|root| writable_part(root, &mapped, &write))
        .collect();
    // A read-write mapping inside a child that is not writable as a whole becomes one of the
    // child's write paths, and OpenVMM refuses to hide a write path. The write paths of the
    // request were checked already.
    for (root, part) in roots.iter().zip(&parts) {
        for path in &part.write {
            if let Some((rule, true)) = nearest_rule(path, &denied, &allowed, true) {
                let (name, given) = mapped
                    .iter()
                    .find(|entry| entry.canonical == *path)
                    .map_or((request.write.name, path.as_path()), |entry| {
                        (entry.name, entry.given.as_path())
                    });
                return unsupported(format!(
                    "{} entry {} hides the {name} entry {}, to which OpenVMM narrows the writes \
                     of {}, and OpenVMM cannot make a hidden path writable; map read-write the \
                     paths that it shows again instead",
                    request.denied.name,
                    rule.display(),
                    given.display(),
                    root.display()
                ));
            }
        }
    }
    // Only objects inside a writable part can be swapped by a workload; pinning others would
    // make ordinary host edits, which often replace files, block the next start.
    let movable = |path: &Path| {
        roots
            .iter()
            .zip(&parts)
            .any(|(root, part)| path.starts_with(root) && part.moves(root, path))
    };
    let mut children = Vec::with_capacity(roots.len());
    let mut policy_bytes = 0;
    for (root, part) in roots.iter().zip(&parts) {
        // Every root is a mapping itself, so the guest reaches all of it.
        let Some(own) = mapped.iter().find(|entry| entry.canonical == *root) else {
            return Err(Error::other(format!(
                "{} is exported but not mapped",
                root.display()
            )));
        };
        let inside = |paths: &[PathBuf]| -> Vec<PathBuf> {
            paths
                .iter()
                .filter(|path| path.starts_with(root))
                .cloned()
                .collect()
        };
        let child_allowed = inside(&allowed);
        let child_denied = needed_denied_paths(&inside(&denied), &child_allowed);
        check_root(root)?;
        policy_bytes += check_policy_paths(root, &child_denied, "hide")?;
        policy_bytes += check_policy_paths(root, &child_allowed, "show again")?;
        policy_bytes += check_policy_paths(root, &part.write, "make writable")?;
        let denied_identities = child_denied
            .iter()
            .map(|path| movable(path).then(|| identity(path)).transpose())
            .collect::<Result<Vec<_>>>()?;
        children.push(ExportChild {
            root: root.clone(),
            file: !own.directory,
            writable: part.writable,
            denied: child_denied,
            denied_identities,
            allowed: child_allowed,
            write: part.write.clone(),
        });
    }
    if policy_bytes > MAX_AGGREGATE_POLICY_BYTES {
        return unsupported(format!(
            "the paths that OpenVMM is to hide, show again, or make writable exceed its \
             {MAX_AGGREGATE_POLICY_BYTES}-byte limit"
        ));
    }

    let mut binds = Vec::with_capacity(mapped.len());
    for entry in &mapped {
        let Some(child) = children
            .iter()
            .position(|child| entry.canonical.starts_with(&child.root))
        else {
            return Err(Error::other(format!(
                "{} lies in no exported directory",
                entry.given.display()
            )));
        };
        let Some(source) = relative_text(&children[child].root, &entry.canonical) else {
            return unsupported(format!("{} is not valid UTF-8", entry.given.display()));
        };
        if GUEST_EXPORT.len() + 1 + guest_source(child, &source).len() > MAX_GUEST_PATH
            || entry.target.len() > MAX_GUEST_PATH
        {
            return unsupported(format!(
                "{} is too long for a guest path",
                entry.given.display()
            ));
        }
        binds.push(Bind {
            child,
            source,
            target: entry.target.clone(),
            read_only: entry.read_only,
            identity: movable(&entry.canonical)
                .then(|| identity(&entry.canonical))
                .transpose()?,
        });
    }
    binds.sort_by(|left, right| {
        depth(&left.target)
            .cmp(&depth(&right.target))
            .then_with(|| left.target.cmp(&right.target))
    });
    if request.owner == Owner::Caller {
        check_caller_roots(&children, unsupported_inspection)?;
    }
    let plan = ExportPlan {
        children,
        binds,
        owner: request.owner,
    };
    // Callers read a record of the plan back through `check`, which accepts every plan that this
    // function makes.
    debug_assert!(
        plan.check(request.reserved).is_ok(),
        "the planner made a plan that its check refuses: {:?}",
        plan.check(request.reserved)
    );
    Ok(Some(plan))
}

/// Resolves one requested mapping: its host path must exist as a directory or regular file that
/// is not a whole volume, and its target must be a guest path that `reserved` leaves free.
fn resolve_mapping<'a>(mapping: &Mapping<'a>, reserved: ReservedTargets<'_>) -> Result<Mapped<'a>> {
    let unsupported = |message: String| Err(Error::unsupported(message));
    let Mapping {
        name,
        path,
        target,
        read_only,
    } = *mapping;
    let canonical = canonicalize(path).map_err(|error| {
        Error::unsupported(format!("{name} entry {} is not accessible", path.display()))
            .with_source(error)
    })?;
    // OpenVMM's options and the records of a plan carry host paths as text.
    if canonical.to_str().is_none() {
        return unsupported(format!(
            "{name} entry {} resolves to a path that is not valid UTF-8",
            path.display()
        ));
    }
    let metadata = fs::metadata(&canonical).map_err(|error| {
        Error::unsupported(format!("cannot inspect {}", path.display())).with_source(error)
    })?;
    if !metadata.is_dir() && !metadata.is_file() {
        return unsupported(format!(
            "{name} entry {} is neither a directory nor a regular file",
            path.display()
        ));
    }
    if metadata.is_dir()
        && let Some(reason) = volume_root_refusal(&canonical)
    {
        return unsupported(format!("{name} entry {} {reason}", path.display()));
    }
    let target = match target {
        Target::Convention => {
            let Some(target) = guest_path(&canonical) else {
                return unsupported(format!(
                    "{name} entry {} has no guest path; use an absolute local path",
                    path.display()
                ));
            };
            if let Some(reserved) = reserved.reservation(&target) {
                return unsupported(format!(
                    "{name} entry {} would cover the guest's {reserved}",
                    path.display()
                ));
            }
            target
        }
        Target::Guest(target) => {
            if let Some(problem) = target_problem(target) {
                return unsupported(format!(
                    "{name} entry {} has the guest target {target:?}, which {problem}",
                    path.display()
                ));
            }
            if let Some(reserved) = reserved.reservation(target) {
                return unsupported(format!(
                    "{name} entry {} has the guest target {target}, which would cover the \
                     guest's {reserved}",
                    path.display()
                ));
            }
            target.to_owned()
        }
    };
    Ok(Mapped {
        name,
        given: path.to_path_buf(),
        canonical,
        target,
        read_only,
        directory: metadata.is_dir(),
    })
}

/// Explains why `target` cannot be a guest target, or returns `None` if it can: it must be an
/// absolute path other than the root, of at most [`MAX_GUEST_PATH`] bytes, without NUL characters
/// or empty, `.`, or `..` components.
fn target_problem(target: &str) -> Option<&'static str> {
    let Some(rest) = target.strip_prefix('/') else {
        return Some("is not absolute");
    };
    if rest.is_empty() {
        return Some("is the guest's root");
    }
    if target.len() > MAX_GUEST_PATH {
        return Some("is longer than a guest path may be");
    }
    if target.contains('\0') {
        return Some("contains a NUL character");
    }
    if rest
        .split('/')
        .any(|component| matches!(component, "" | "." | ".."))
    {
        return Some("has an empty, `.`, or `..` component");
    }
    None
}

/// Refuses mappings whose targets collide: two objects at one target, or a target inside another
/// mapping's that does not show the very object that the other mapping shows there, which it
/// would hide.
fn check_targets(mapped: &[Mapped<'_>]) -> Result<()> {
    for (index, entry) in mapped.iter().enumerate() {
        for other in &mapped[..index] {
            // The entries of one object at one target are merged already.
            if entry.target == other.target {
                return Err(Error::unsupported(format!(
                    "{} entry {} and the {} entry {} have the same guest target {}",
                    entry.name,
                    entry.given.display(),
                    other.name,
                    other.given.display(),
                    entry.target
                )));
            }
            let (outer, inner) = if within(&entry.target, &other.target) {
                (other, entry)
            } else if within(&other.target, &entry.target) {
                (entry, other)
            } else {
                continue;
            };
            if !shows(outer, inner) {
                return Err(Error::unsupported(format!(
                    "{} entry {} at {} would hide part of the {} entry {} at {}; map it at a \
                     target outside that one",
                    inner.name,
                    inner.given.display(),
                    inner.target,
                    outer.name,
                    outer.given.display(),
                    outer.target
                )));
            }
        }
    }
    Ok(())
}

/// Returns whether `inner`, whose target lies below that of `outer`, maps the host object that
/// `outer` shows at that target: whether the rest of `inner`'s target spells, name for name, the
/// host path from `outer`'s object to `inner`'s, as a plan's bind sources spell it. A Windows host
/// reads a backslash in a name as a separator, and a name with a root or a drive as another path,
/// so such a guest name never matches.
fn shows(outer: &Mapped<'_>, inner: &Mapped<'_>) -> bool {
    let Some(rest) = inner
        .target
        .strip_prefix(outer.target.as_str())
        .and_then(|rest| rest.strip_prefix('/'))
    else {
        return false;
    };
    outer.directory
        && relative_text(&outer.canonical, &inner.canonical).is_some_and(|names| names == rest)
}

/// Refuses an exported directory or file that belongs to UID 0 or GID 0, to which the caller
/// owner would squash guest root, and fails as `uninspected` returns for one whose owner cannot be
/// read, with its path and the failure.
fn check_caller_roots(
    children: &[ExportChild],
    uninspected: fn(&Path, std::io::Error) -> Error,
) -> Result<()> {
    #[cfg(unix)]
    for child in children {
        use std::os::unix::fs::MetadataExt;

        let metadata =
            fs::metadata(&child.root).map_err(|error| uninspected(&child.root, error))?;
        if metadata.uid() == 0 || metadata.gid() == 0 {
            return Err(Error::unsupported(format!(
                "the caller owner squashes guest root to the owner of each exported directory or \
                 file, so {} must not be owned by UID 0 or GID 0",
                child.root.display()
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = (children, uninspected);
    Ok(())
}

/// Returns the failure of a plan that cannot inspect `path`, with `error`, which refuses the
/// request.
fn unsupported_inspection(path: &Path, error: std::io::Error) -> Error {
    Error::unsupported(format!("cannot inspect {}", path.display())).with_source(error)
}

/// Returns the failure of [`ExportPlan::verify`] for `path`, which no longer names the host object
/// that the plan found there.
fn changed_object(path: &Path) -> Error {
    Error::other(format!(
        "{} no longer names the host object it named at provision; deprovision the sandbox and \
         provision it again",
        path.display()
    ))
}

/// Returns the failure of [`ExportPlan::verify`] that cannot inspect `path`, an object that the
/// plan found, with `error`: a path that no longer reaches the object, as the host sees it, whose
/// object went, a component of which is no longer a directory or is a link that [`loops`], or
/// that the host may no longer search or open, names it no longer, [`changed_object`], and any
/// other failure is the host's own, which shows nothing of the object.
fn uninspected(path: &Path, error: std::io::Error) -> Error {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::PermissionDenied
    ) || loops(&error)
    {
        changed_object(path).with_source(error)
    } else {
        Error::io(format!("cannot inspect {}", path.display())).with_source(error)
    }
}

/// Returns whether `error` is the host's refusal to resolve a path whose links loop, or nest
/// deeper than the host follows them, as a link that names itself does: the failure that the
/// standard library's unstable `ErrorKind::FilesystemLoop` names.
fn loops(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    let code = Some(nix::errno::Errno::ELOOP as i32);
    #[cfg(windows)]
    let code = Some(windows_sys::Win32::Foundation::ERROR_CANT_RESOLVE_FILENAME as i32);
    #[cfg(not(any(unix, windows)))]
    let code = None;
    code.is_some() && error.raw_os_error() == code
}

/// Explains why a mapped host directory, given in canonical form, cannot be exported, or returns
/// `None` if it can: a whole volume. It inspects only the path, not the host.
fn volume_root_refusal(canonical: &Path) -> Option<&'static str> {
    (!canonical
        .components()
        .any(|component| matches!(component, Component::Normal(_))))
    .then_some(
        "is a whole volume, which the openvmm backend does not map; map the directories inside \
         it",
    )
}

/// Explains why `path`, a host path of a recorded plan, is not in the form in which [`plan`]
/// resolves paths, or returns `None` if it is: without a NUL character, which no name holds and
/// no process argument carries; without a `..` component, through which a path that lies inside a
/// directory by its text could leave it; and, on Windows, on a local drive, without the verbatim
/// prefix. It inspects only the path, not the host.
fn resolved_path_problem(path: &Path) -> Option<&'static str> {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return Some("contains a NUL character");
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Some("has a `..` component");
    }
    if cfg!(windows)
        && !matches!(
            path.components().next(),
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
        )
    {
        return Some("is not a path on a local drive");
    }
    None
}

/// Sorts `paths`, drops duplicates, and returns those that no other of them contains.
fn outermost(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    paths
        .iter()
        .filter(|path| {
            !paths
                .iter()
                .any(|other| other != *path && path.starts_with(other))
        })
        .cloned()
        .collect()
}

/// Canonicalized, validated form of one requested host mapping.
struct Mapped<'a> {
    /// How messages name the list or option that requested it.
    name: &'a str,
    /// Host path exactly as the caller supplied it, for diagnostics.
    given: PathBuf,
    /// Link-resolved host path used to construct the export.
    canonical: PathBuf,
    /// Absolute guest destination of the bind mount.
    target: String,
    /// Whether the guest must mount this path read-only.
    read_only: bool,
    /// Whether the mapped host object is a directory rather than a regular file.
    directory: bool,
}

#[cfg(test)]
mod tests;
