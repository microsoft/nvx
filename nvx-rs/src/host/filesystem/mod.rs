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
//!   path at the same path ([`guest_path`]).
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

use std::fs;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};

use super::platform;
use crate::error::{Error, Result};
use crate::protocol::export::EXPORT_DIRECTORY;
use crate::protocol::share::{RESERVED_EXACT_MOUNT_TARGETS, RESERVED_MOUNT_TARGETS};
use crate::protocol::{HostMap, MAPS_TOKEN, MAX_HOST_MAPPINGS};

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

/// Joins an aggregate child index and child-relative source into a guest export path.
fn guest_source(child: usize, source: &str) -> String {
    if source.is_empty() {
        child.to_string()
    } else {
        format!("{child}/{source}")
    }
}

/// Returns the guest path at which a sandbox exposes the host path `path`, or `None` if it
/// has no guest equivalent (relative paths, UNC paths, or paths that are not valid UTF-8).
///
/// A Windows path `C:\work\src` maps to `/mnt/c/work/src`, and an absolute Linux path maps to
/// itself. `.` components are dropped; `..` components are rejected.
pub fn guest_path(path: &Path) -> Option<String> {
    let mut components = path.components().peekable();
    let mut guest = match components.peek()? {
        Component::Prefix(prefix) => {
            let drive = match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
                _ => return None,
            };
            components.next();
            if !matches!(components.next()?, Component::RootDir) {
                return None;
            }
            format!("/mnt/{}", char::from(drive).to_ascii_lowercase())
        }
        Component::RootDir if !cfg!(windows) => {
            components.next();
            String::new()
        }
        _ => return None,
    };
    for component in components {
        match component {
            Component::Normal(name) => {
                guest.push('/');
                guest.push_str(name.to_str()?);
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    if guest.is_empty() {
        guest.push('/');
    }
    Some(guest)
}

/// Returns the guest path of an existing host path the way mappings derive it: from the
/// resolved path, so links, letter case, and short names do not matter. A path that does not
/// exist is translated as written.
///
/// Use it to turn a host working directory into a `process.cwd` inside a mapped path.
pub fn resolve_guest_path(path: &Path) -> Option<String> {
    match canonicalize(path) {
        Ok(canonical) => guest_path(&canonical),
        Err(_) => guest_path(path),
    }
}

/// Where the guest sees a mapped host path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// At the guest path that [`guest_path`] derives from the resolved host path.
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
        check_caller_roots(&children)?;
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

/// A path of an access policy, resolved.
struct PolicyPath {
    /// Host path as the caller supplied it, for diagnostics.
    given: PathBuf,
    /// Link-resolved host path.
    canonical: PathBuf,
}

/// Returns the resolved paths of `paths`.
fn canonicals(paths: &[PolicyPath]) -> Vec<PathBuf> {
    paths.iter().map(|path| path.canonical.clone()).collect()
}

/// Resolves the path of the list `field`, which must exist.
fn resolve_policy_path(field: &str, path: &Path) -> Result<PathBuf> {
    canonicalize(path).map_err(|error| {
        Error::unsupported(format!(
            "{field} entry {} is not accessible",
            path.display()
        ))
        .with_source(error)
    })
}

/// Resolves the write paths of `list`: each must lie inside a read-write directory that a child
/// of `roots` exports whole, other than that directory itself.
fn write_paths(
    list: PathList<'_>,
    mapped: &[Mapped<'_>],
    roots: &[PathBuf],
) -> Result<Vec<PolicyPath>> {
    let field = list.name;
    let mut paths = Vec::with_capacity(list.paths.len());
    for path in list.paths {
        let canonical = resolve_policy_path(field, path)?;
        let narrows = roots
            .iter()
            .find(|root| canonical.starts_with(root))
            .is_some_and(|root| {
                *root != canonical
                    && mapped.iter().any(|entry| {
                        entry.canonical == *root && entry.directory && !entry.read_only
                    })
            });
        if !narrows {
            return Err(Error::unsupported(format!(
                "{field} entry {} lies in no directory that is mapped read-write as a whole, \
                 whose writes it could narrow",
                path.display()
            )));
        }
        paths.push(PolicyPath {
            given: path.clone(),
            canonical,
        });
    }
    Ok(paths)
}

/// Refuses a read-only mapping that the host could not keep read-only: one inside a write path,
/// one that a read-write mapping holds, unless write paths narrow that mapping's writes, or one of
/// an object that a read-write mapping maps elsewhere. OpenVMM can limit a child's writes to some
/// of its paths, but cannot keep a path inside them read-only. Only the guest's bind mount would
/// then protect the path, and guest root can bypass it: through the export, or on a Windows host,
/// which opens files case-insensitively, through another spelling of a file's name.
fn check_read_only(
    mapped: &[Mapped<'_>],
    write: &[PolicyPath],
    write_name: &str,
    roots: &[PathBuf],
) -> Result<()> {
    // Write paths narrow the writes of the read-write directory that a child exports whole.
    let narrowed = |canonical: &Path| {
        roots.iter().any(|root| root == canonical)
            && write
                .iter()
                .any(|path| path.canonical.starts_with(canonical))
    };
    let inside = |inner: &Mapped<'_>, outer_name: &str, outer: &Path, instead: &str| {
        Err(Error::unsupported(format!(
            "{} entry {} lies inside the {outer_name} entry {}, where the host cannot keep it \
             read-only; instead, {instead}",
            inner.name,
            inner.given.display(),
            outer.display()
        )))
    };
    for inner in mapped.iter().filter(|entry| entry.read_only) {
        if let Some(path) = write
            .iter()
            .find(|path| inner.canonical.starts_with(&path.canonical))
        {
            return inside(
                inner,
                write_name,
                &path.given,
                "narrow the writes to paths outside it",
            );
        }
        let Some(outer) = mapped.iter().find(|outer| {
            !outer.read_only
                && !narrowed(&outer.canonical)
                && (outer.canonical == inner.canonical
                    || (outer.directory && inner.canonical.starts_with(&outer.canonical)))
        }) else {
            continue;
        };
        if outer.canonical == inner.canonical {
            return Err(Error::unsupported(format!(
                "{} entry {} names the object of the {} entry {}, which the guest may write, so \
                 the host cannot keep it read-only",
                inner.name,
                inner.given.display(),
                outer.name,
                outer.given.display()
            )));
        }
        return inside(
            inner,
            outer.name,
            &outer.given,
            "map that directory read-only and the paths in it that the guest writes read-write",
        );
    }
    Ok(())
}

/// Refuses roots that name one directory or file: OpenVMM refuses one object exported twice,
/// which bind mounts and hard links make possible.
fn distinct_roots(roots: &[PathBuf]) -> Result<()> {
    let mut identities: Vec<(FileIdentity, &Path)> = Vec::with_capacity(roots.len());
    for root in roots {
        let current = identity(root)?;
        if let Some((_, other)) = identities.iter().find(|(other, _)| *other == current) {
            return Err(Error::unsupported(format!(
                "{} and {} are the same host directory or file; map it once",
                other.display(),
                root.display()
            )));
        }
        identities.push((current, root));
    }
    Ok(())
}

/// Resolves the allowed paths of `list`, which must exist inside a child of `roots`, below its
/// root.
fn allowed_paths(list: PathList<'_>, roots: &[PathBuf]) -> Result<Vec<PolicyPath>> {
    let field = list.name;
    let mut paths: Vec<PolicyPath> = Vec::with_capacity(list.paths.len());
    for path in list.paths {
        let canonical = resolve_policy_path(field, path)?;
        if roots.contains(&canonical) {
            return Err(Error::unsupported(format!(
                "{field} entry {} is an exported path itself rather than a path inside one",
                path.display()
            )));
        }
        if !roots.iter().any(|root| canonical.starts_with(root)) {
            return Err(Error::unsupported(format!(
                "{field} entry {} lies in no mapped directory",
                path.display()
            )));
        }
        if !paths.iter().any(|other| other.canonical == canonical) {
            paths.push(PolicyPath {
                given: path.clone(),
                canonical,
            });
        }
    }
    Ok(paths)
}

/// Resolves the denied paths of `list` that exist, and refuses one that hides a mapped path,
/// which `allowed` paths do not show again, or one that does not exist inside a mapped path.
fn denied_paths(
    list: PathList<'_>,
    mapped: &[Mapped<'_>],
    allowed: &[PolicyPath],
) -> Result<Vec<PolicyPath>> {
    let field = list.name;
    let mut resolved = Vec::with_capacity(list.paths.len());
    for path in list.paths {
        let (canonical, exists) = canonicalize_lenient(path).map_err(|error| {
            Error::unsupported(format!(
                "{field} entry {} cannot be resolved",
                path.display()
            ))
            .with_source(error)
        })?;
        resolved.push((path, canonical, exists));
    }
    let denied_paths: Vec<PathBuf> = resolved
        .iter()
        .filter(|(_, _, exists)| *exists)
        .map(|(_, canonical, _)| canonical.clone())
        .collect();
    let allowed_paths = canonicals(allowed);
    let mut denied: Vec<PolicyPath> = Vec::with_capacity(denied_paths.len());
    for (path, canonical, exists) in resolved {
        if !exists {
            if let Some(entry) = mapped
                .iter()
                .find(|entry| canonical.starts_with(&entry.canonical))
            {
                return Err(Error::unsupported(format!(
                    "{field} entry {} does not exist, so it cannot be hidden inside the mapped \
                     path {}",
                    path.display(),
                    entry.given.display()
                )));
            }
            continue;
        }
        // A mapped directory that is denied itself shows the allowed paths inside it, and nothing
        // else, whichever denied path contains it.
        let hidden = mapped.iter().find(|entry| {
            entry.canonical.starts_with(&canonical)
                && match nearest_rule(&entry.canonical, &denied_paths, &allowed_paths, true) {
                    Some((rule, true)) => {
                        !(entry.directory
                            && rule == entry.canonical.as_path()
                            && allowed_paths.iter().any(|shown| {
                                *shown != entry.canonical && shown.starts_with(&entry.canonical)
                            }))
                    }
                    _ => false,
                }
        });
        if let Some(entry) = hidden {
            return Err(Error::unsupported(format!(
                "{field} entry {} contains the mapped path {}",
                path.display(),
                entry.given.display()
            )));
        }
        if !denied.iter().any(|other| other.canonical == canonical) {
            denied.push(PolicyPath {
                given: path.clone(),
                canonical,
            });
        }
    }
    Ok(denied)
}

/// Refuses an allowed path that is also denied, or whose nearest enclosing policy path is not a
/// denied one, which OpenVMM refuses.
fn check_allowed_paths(
    allowed_name: &str,
    denied_name: &str,
    allowed: &[PolicyPath],
    denied: &[PolicyPath],
) -> Result<()> {
    let (denied_paths, allowed_paths) = (canonicals(denied), canonicals(allowed));
    for path in allowed {
        let refused = |problem: String| {
            Err(Error::unsupported(format!(
                "{allowed_name} entry {} {problem}",
                path.given.display()
            )))
        };
        if denied_paths.contains(&path.canonical) {
            return refused(format!("is also a {denied_name} entry"));
        }
        match nearest_rule(&path.canonical, &denied_paths, &allowed_paths, false) {
            Some((_, true)) => {}
            Some((other, false)) => {
                return refused(format!(
                    "lies inside the {allowed_name} entry {}, which shows it already",
                    other.display()
                ));
            }
            None => {
                return refused(format!(
                    "lies inside no {denied_name} entry, so it has nothing to show again"
                ));
            }
        }
    }
    Ok(())
}

/// Refuses a write path that a denied path hides, which OpenVMM refuses.
fn check_write_paths(
    write_name: &str,
    denied_name: &str,
    write: &[PolicyPath],
    denied: &[PolicyPath],
    allowed: &[PolicyPath],
) -> Result<()> {
    let (denied_paths, allowed_paths) = (canonicals(denied), canonicals(allowed));
    for path in write {
        if let Some((rule, true)) =
            nearest_rule(&path.canonical, &denied_paths, &allowed_paths, true)
        {
            return Err(Error::unsupported(format!(
                "{write_name} entry {} lies inside the {denied_name} entry {}, which hides it",
                path.given.display(),
                rule.display()
            )));
        }
    }
    Ok(())
}

/// Returns the denied paths of one child that OpenVMM needs to hide, in order: those that no other
/// of them hides already, without an allowed path between them. OpenVMM refuses the others.
///
/// It takes one child's paths alone: a denied path outside the child, such as one that contains a
/// denied root, hides none of the child for OpenVMM, which receives each child's policy alone.
fn needed_denied_paths(denied: &[PathBuf], allowed: &[PathBuf]) -> Vec<PathBuf> {
    let mut needed: Vec<PathBuf> = denied
        .iter()
        .filter(|path| {
            !nearest_rule(path, denied, allowed, false).is_some_and(|(_, denied)| denied)
        })
        .cloned()
        .collect();
    needed.sort();
    needed
}

/// Explains how the policy of `child` breaks a rule of OpenVMM, or returns `None` if it keeps them
/// all: it lists no path twice, hides no path inside another that it hides without an allowed
/// path between them, shows each allowed path again inside a hidden path without another allowed
/// path between them, and makes writable only paths strictly inside it that it shows, none inside
/// another. [`plan`] checks these rules on the paths that it resolves. It reads nothing from the
/// host.
fn policy_rule_problem(child: &ExportChild) -> Option<String> {
    let (denied, allowed) = (&child.denied, &child.allowed);
    for (paths, kind) in [
        (denied, "hidden"),
        (allowed, "allowed"),
        (&child.write, "writable"),
    ] {
        let repeated = paths
            .iter()
            .enumerate()
            .find(|&(position, path)| paths[..position].contains(path));
        if let Some((_, path)) = repeated {
            return Some(format!("it lists the {kind} path {} twice", path.display()));
        }
    }
    for path in denied {
        if let Some((rule, true)) = nearest_rule(path, denied, allowed, false) {
            return Some(format!(
                "it hides {} inside {}, which it hides already",
                path.display(),
                rule.display()
            ));
        }
    }
    for path in allowed {
        if denied.contains(path) {
            return Some(format!("it both hides and shows {}", path.display()));
        }
        match nearest_rule(path, denied, allowed, false) {
            Some((_, true)) => {}
            Some((rule, false)) => {
                return Some(format!(
                    "it shows {} again inside {}, which it shows already",
                    path.display(),
                    rule.display()
                ));
            }
            None => {
                return Some(format!(
                    "it shows {} again, but hides no path that contains it",
                    path.display()
                ));
            }
        }
    }
    for path in &child.write {
        if *path == child.root {
            return Some(format!(
                "it makes all of {} writable through a writable path",
                path.display()
            ));
        }
        if let Some(other) = child
            .write
            .iter()
            .find(|other| *other != path && path.starts_with(other))
        {
            return Some(format!(
                "it makes {} writable inside {}, which it makes writable already",
                path.display(),
                other.display()
            ));
        }
        if let Some((rule, true)) = nearest_rule(path, denied, allowed, true) {
            return Some(format!(
                "it makes {} writable, but {} hides it",
                path.display(),
                rule.display()
            ));
        }
    }
    None
}

/// Returns the nearest of the `denied` and `allowed` paths that contains `path`, which counts
/// itself only when `inclusive`, and whether it is a denied path: the rule by which OpenVMM
/// decides whether the guest sees `path`.
fn nearest_rule<'p>(
    path: &Path,
    denied: &'p [PathBuf],
    allowed: &'p [PathBuf],
    inclusive: bool,
) -> Option<(&'p Path, bool)> {
    denied
        .iter()
        .map(|rule| (rule, true))
        .chain(allowed.iter().map(|rule| (rule, false)))
        .filter(|(rule, _)| (inclusive || rule.as_path() != path) && path.starts_with(rule))
        .max_by_key(|(rule, _)| rule.components().count())
        .map(|(rule, denied)| (rule.as_path(), denied))
}

/// Whether a child is writable, and the paths to which OpenVMM limits its writes.
struct Writable {
    /// Whether the guest may modify the child.
    writable: bool,
    /// The paths to which OpenVMM limits the guest's writes; empty if the guest may modify all of
    /// a writable child.
    write: Vec<PathBuf>,
}

impl Writable {
    /// Returns whether a workload could move `path`, which lies in the child at `root`: whether it
    /// lies strictly inside a writable part, in a directory that the workload may change.
    fn moves(&self, root: &Path, path: &Path) -> bool {
        self.writable
            && path != root
            && (self.write.is_empty()
                || self
                    .write
                    .iter()
                    .any(|write| path != write && path.starts_with(write)))
    }
}

/// Returns the writable part of the child at `root`: its read-write mappings, or, when one of them
/// is the whole child, all of it, unless write paths narrow its writes to them and the read-write
/// mappings inside it.
fn writable_part(root: &Path, mapped: &[Mapped<'_>], write: &[PolicyPath]) -> Writable {
    let read_write: Vec<PathBuf> = mapped
        .iter()
        .filter(|entry| !entry.read_only && entry.canonical.starts_with(root))
        .map(|entry| entry.canonical.clone())
        .collect();
    if read_write.is_empty() {
        return Writable {
            writable: false,
            write: Vec::new(),
        };
    }
    let narrowing: Vec<PathBuf> = write
        .iter()
        .filter(|path| path.canonical.starts_with(root))
        .map(|path| path.canonical.clone())
        .collect();
    let write = if !read_write.iter().any(|path| path == root) {
        // Unless a read-write mapping covers the whole child, OpenVMM limits writes to them.
        outermost(read_write)
    } else if narrowing.is_empty() {
        Vec::new()
    } else {
        outermost(
            narrowing
                .into_iter()
                .chain(read_write.into_iter().filter(|path| path != root))
                .collect(),
        )
    };
    Writable {
        writable: true,
        write,
    }
}

/// Refuses an exported directory or file that belongs to UID 0 or GID 0, to which the caller
/// owner would squash guest root.
fn check_caller_roots(children: &[ExportChild]) -> Result<()> {
    #[cfg(unix)]
    for child in children {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(&child.root).map_err(|error| {
            Error::unsupported(format!("cannot inspect {}", child.root.display()))
                .with_source(error)
        })?;
        if metadata.uid() == 0 || metadata.gid() == 0 {
            return Err(Error::unsupported(format!(
                "the caller owner squashes guest root to the owner of each exported directory or \
                 file, so {} must not be owned by UID 0 or GID 0",
                child.root.display()
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = children;
    Ok(())
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

impl ExportPlan {
    /// Checks that a plan read back from a record is one that [`plan`] could have made for a root
    /// that reserves `reserved`: at least one and at most [`MAX_CHILDREN`] children, none inside
    /// another, each an absolute host path below a volume's root, with its host paths in the form
    /// in which `plan` resolves them, whose policy paths lie inside it within OpenVMM's limits and
    /// keep its rules, with an identity slot for each denied path, and writable exactly when a bind
    /// maps a path of it read-write; and at least one and at most [`MAX_HOST_MAPPINGS`] binds,
    /// each of a known child, from a relative source that holds the host's names joined with `/`,
    /// that a guest path can hold, and that the child does not hide, unless it shows paths inside
    /// it again, to a free guest target, parents before children, with a bind of every child's
    /// root. A target inside another's must show what the other shows there. A child's write
    /// paths must be the outermost paths of its read-write binds, unless one maps the whole child,
    /// when they may narrow its writes only to paths that keep its other read-write binds
    /// writable; and each denied or mapped object that a workload could move, strictly inside a
    /// part of the child that the guest may write, must carry its identity. It reads nothing from
    /// the host; [`verify`](Self::verify) checks the host.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
    pub fn check(&self, reserved: ReservedTargets<'_>) -> Result<()> {
        let invalid = |reason: String| {
            Err(Error::unsupported(format!(
                "the recorded export plan is invalid: {reason}"
            )))
        };
        if self.children.is_empty() || self.children.len() > MAX_CHILDREN {
            return invalid(format!(
                "it exports {} directories and files, but OpenVMM exports from 1 to \
                 {MAX_CHILDREN}",
                self.children.len()
            ));
        }
        if self.binds.is_empty() || self.binds.len() > MAX_HOST_MAPPINGS {
            return invalid(format!(
                "it maps {} paths, but a sandbox maps from 1 to {MAX_HOST_MAPPINGS}",
                self.binds.len()
            ));
        }
        let mut policy_bytes = 0;
        for (index, child) in self.children.iter().enumerate() {
            if !child.root.is_absolute() || volume_root_refusal(&child.root).is_some() {
                return invalid(format!(
                    "child {index}, {}, is not an absolute path below a volume's root",
                    child.root.display()
                ));
            }
            if let Some((path, problem)) = std::iter::once(&child.root)
                .chain(&child.denied)
                .chain(&child.allowed)
                .chain(&child.write)
                .find_map(|path| resolved_path_problem(path).map(|problem| (path, problem)))
            {
                return invalid(format!(
                    "child {index} has the host path {path:?}, which {problem}"
                ));
            }
            if child.denied_identities.len() != child.denied.len() {
                return invalid(format!(
                    "child {index} has {} denied paths but {} identities",
                    child.denied.len(),
                    child.denied_identities.len()
                ));
            }
            if let Some(path) = [&child.denied, &child.allowed, &child.write]
                .into_iter()
                .flatten()
                .find(|path| !path.starts_with(&child.root) || (child.file && *path != &child.root))
            {
                return invalid(format!(
                    "child {index} has the policy path {}, which is not inside it",
                    path.display()
                ));
            }
            for (paths, action) in [
                (&child.denied, "hide"),
                (&child.allowed, "show again"),
                (&child.write, "make writable"),
            ] {
                match policy_limits(&child.root, paths, action) {
                    Ok(bytes) => policy_bytes += bytes,
                    Err(problem) => return invalid(format!("child {index}: {problem}")),
                }
            }
            if let Some(other) = self.children[..index].iter().position(|other| {
                other.root.starts_with(&child.root) || child.root.starts_with(&other.root)
            }) {
                return invalid(format!(
                    "child {index}, {}, overlaps child {other}, {}",
                    child.root.display(),
                    self.children[other].root.display()
                ));
            }
            if let Some(problem) = policy_rule_problem(child) {
                return invalid(format!("child {index}: {problem}"));
            }
            if !child.writable && !child.write.is_empty() {
                return invalid(format!("child {index} is read-only but has writable paths"));
            }
            if !self
                .binds
                .iter()
                .any(|bind| bind.child == index && bind.source.is_empty())
            {
                return invalid(format!("it maps no path at the root of child {index}"));
            }
            // OpenVMM lets the guest write a child that a read-write bind needs, and no other.
            let read_write = self
                .binds
                .iter()
                .any(|bind| bind.child == index && !bind.read_only);
            if child.writable != read_write {
                return invalid(if child.writable {
                    format!("child {index} is writable but maps no path read-write")
                } else {
                    format!("child {index} is read-only but maps a path read-write")
                });
            }
        }
        if policy_bytes > MAX_AGGREGATE_POLICY_BYTES {
            return invalid(format!(
                "the paths that OpenVMM is to hide, show again, or make writable exceed its \
                 {MAX_AGGREGATE_POLICY_BYTES}-byte limit"
            ));
        }
        let mut previous: Option<&Bind> = None;
        for (index, bind) in self.binds.iter().enumerate() {
            let Some(child) = self.children.get(bind.child) else {
                return invalid(format!(
                    "it maps a path of the unknown child {}",
                    bind.child
                ));
            };
            if !bind.source.is_empty()
                && (child.file
                    || bind
                        .source
                        .split('/')
                        .any(|component| matches!(component, "" | "." | "..")))
            {
                return invalid(format!(
                    "it maps the path {:?} of child {}, which is not a path inside a directory",
                    bind.source, bind.child
                ));
            }
            // The guest receives the source as a path below its mount of the export.
            if bind.source.contains('\0') {
                return invalid(format!(
                    "it maps the path {:?} of child {}, which contains a NUL character",
                    bind.source, bind.child
                ));
            }
            if GUEST_EXPORT.len() + 1 + bind.guest_source().len() > MAX_GUEST_PATH {
                return invalid(format!(
                    "it maps a path of {} bytes in child {}, which is too long for a guest path",
                    bind.source.len(),
                    bind.child
                ));
            }
            let path = if bind.source.is_empty() {
                child.root.clone()
            } else {
                child.root.join(&bind.source)
            };
            // The host checks the object at that path, which must be the one that the guest maps:
            // the source must hold the host's names below the root, joined with `/`, as `plan`
            // spells them. A Windows host would read a backslash in a name as a separator, and a
            // name with a root or a drive as another path.
            if relative_text(&child.root, &path).as_deref() != Some(bind.source.as_str()) {
                return invalid(format!(
                    "it maps the path {:?} of child {}, which the host reads as other names",
                    bind.source, bind.child
                ));
            }
            // A mapped path shows something: OpenVMM does not hide it, or it shows the allowed
            // paths inside it again.
            if matches!(
                nearest_rule(&path, &child.denied, &child.allowed, true),
                Some((_, true))
            ) && !child
                .allowed
                .iter()
                .any(|shown| *shown != path && shown.starts_with(&path))
            {
                return invalid(format!(
                    "it maps {} at {}, but child {} hides it",
                    path.display(),
                    bind.target,
                    bind.child
                ));
            }
            if let Some(problem) = target_problem(&bind.target) {
                return invalid(format!("its guest target {:?} {problem}", bind.target));
            }
            if let Some(reserved) = reserved.reservation(&bind.target) {
                return invalid(format!(
                    "its guest target {} would cover the guest's {reserved}",
                    bind.target
                ));
            }
            if let Some(previous) = previous
                && (depth(&previous.target), previous.target.as_str())
                    >= (depth(&bind.target), bind.target.as_str())
            {
                return invalid(format!(
                    "it maps {} after {}, out of order or twice",
                    bind.target, previous.target
                ));
            }
            // A target inside another shows what the other shows there, as `plan` requires.
            for outer in &self.binds[..index] {
                let Some(rest) = bind
                    .target
                    .strip_prefix(outer.target.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
                else {
                    continue;
                };
                let shown = if outer.source.is_empty() {
                    rest.to_owned()
                } else {
                    format!("{}/{rest}", outer.source)
                };
                if bind.child != outer.child || bind.source != shown {
                    return invalid(format!(
                        "it maps {} inside {}, where it would hide part of that path",
                        bind.target, outer.target
                    ));
                }
            }
            previous = Some(bind);
        }
        for (index, child) in self.children.iter().enumerate() {
            let bound = |bind: &Bind| {
                if bind.source.is_empty() {
                    child.root.clone()
                } else {
                    child.root.join(&bind.source)
                }
            };
            let read_write: Vec<PathBuf> = self
                .binds
                .iter()
                .filter(|bind| bind.child == index && !bind.read_only)
                .map(bound)
                .collect();
            // A child that a read-write bind maps whole is writable throughout, unless write paths
            // narrow its writes, which must then keep its other read-write binds writable; any
            // other child's writes are narrowed to exactly what its read-write binds map.
            if read_write.contains(&child.root) {
                if let Some(path) = read_write.iter().find(|path| {
                    **path != child.root
                        && !child.write.is_empty()
                        && !child.write.iter().any(|write| path.starts_with(write))
                }) {
                    return invalid(format!(
                        "child {index} narrows its writes so that {}, which it maps read-write, \
                         stays read-only",
                        path.display()
                    ));
                }
            } else {
                let mut write = child.write.clone();
                write.sort();
                if outermost(read_write) != write {
                    return invalid(format!(
                        "child {index} lets the guest write other paths than those that it maps \
                         read-write"
                    ));
                }
            }
            // A workload that may write a part of the child could move an object there and put a
            // decoy at its path, so the plan pins each denied and mapped object in such a part, as
            // `plan` does, for `verify` to check it at the next start.
            let part = Writable {
                writable: child.writable,
                write: child.write.clone(),
            };
            if let Some((path, _)) = child
                .denied
                .iter()
                .zip(&child.denied_identities)
                .find(|(path, identity)| identity.is_none() && part.moves(&child.root, path))
            {
                return invalid(format!(
                    "child {index} does not pin the denied path {}, which a workload could move",
                    path.display()
                ));
            }
            if let Some(bind) = self.binds.iter().find(|bind| {
                bind.child == index
                    && bind.identity.is_none()
                    && part.moves(&child.root, &bound(bind))
            }) {
                return invalid(format!(
                    "it does not pin the object that it maps at {}, which a workload could move",
                    bind.target
                ));
            }
        }
        Ok(())
    }

    /// Checks that the host keeps every read-only mapping read-only, that every child's root is
    /// still the kind of object that [`plan`] exported, that every mapped and denied path inside a
    /// writable part still names the object it named then, and that the host still supports the
    /// owner.
    ///
    /// [`plan`] refuses a read-only path inside a part that the guest may write, which only the
    /// guest's bind mount would keep read-only; a recorded plan that holds one fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). A file that a directory replaced
    /// would otherwise grant the whole directory, and a workload that may write a directory could
    /// rename a denied or mapped object inside it and put a decoy at its path, so the next start
    /// would hide or bind the decoy instead; these fail with
    /// [`ErrorKind::Other`](crate::ErrorKind::Other). With the caller owner, the host must be a
    /// Linux host, and no exported directory or file may belong to UID 0 or GID 0; these fail with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). Run it while the VM is stopped,
    /// so that no workload can race with it; OpenVMM checks each child's kind again when it opens
    /// it.
    pub fn verify(&self) -> Result<()> {
        for bind in self.binds.iter().filter(|bind| bind.read_only) {
            let (child, path) = self.bound(bind)?;
            // Without writable paths, the guest may write all of a writable child.
            if child.writable
                && (child.write.is_empty()
                    || child.write.iter().any(|write| path.starts_with(write)))
            {
                return Err(Error::unsupported(format!(
                    "the sandbox maps {} read-only inside a part of {} that the guest may write, \
                     where the host cannot keep it read-only; deprovision the sandbox, and \
                     provision it again without read-only paths inside read-write directories",
                    path.display(),
                    child.root.display()
                )));
            }
        }
        let changed = |path: &Path| {
            Err(Error::other(format!(
                "{} no longer names the host object it named at provision; deprovision the \
                 sandbox and provision it again",
                path.display()
            )))
        };
        for child in &self.children {
            let kind_matches = fs::symlink_metadata(&child.root).is_ok_and(|metadata| {
                if child.file {
                    metadata.is_file()
                } else {
                    metadata.is_dir()
                }
            });
            if !kind_matches {
                return changed(&child.root);
            }
        }
        for bind in &self.binds {
            let Some(expected) = bind.identity else {
                continue;
            };
            let (_, path) = self.bound(bind)?;
            match platform::file_identity(&path) {
                Ok((device, index)) if (FileIdentity { device, index }) == expected => {}
                _ => return changed(&path),
            }
        }
        for child in &self.children {
            for (path, expected) in child.denied.iter().zip(&child.denied_identities) {
                let Some(expected) = expected else {
                    continue;
                };
                match platform::file_identity(path) {
                    Ok((device, index)) if (FileIdentity { device, index }) == *expected => {}
                    _ => return changed(path),
                }
            }
        }
        if self.owner == Owner::Caller {
            if cfg!(windows) {
                return Err(Error::unsupported(CALLER_OWNER_ON_WINDOWS));
            }
            check_caller_roots(&self.children)?;
        }
        Ok(())
    }

    /// Returns the child that holds `bind` and the host path that `bind` maps.
    fn bound(&self, bind: &Bind) -> Result<(&ExportChild, PathBuf)> {
        let Some(child) = self.children.get(bind.child) else {
            return Err(Error::io(
                "the sandbox state maps a path of an unknown host directory",
            ));
        };
        let path = if bind.source.is_empty() {
            child.root.clone()
        } else {
            child.root.join(&bind.source)
        };
        Ok((child, path))
    }

    /// Kernel command-line token that tells the guest agent how many bind mounts to expect over
    /// the control channel.
    pub fn kernel_token(&self) -> String {
        format!("{MAPS_TOKEN}{}", self.binds.len())
    }

    /// The bind mounts in the order and form in which the guest agent receives them.
    pub fn guest_table(&self) -> Vec<HostMap> {
        self.binds
            .iter()
            .map(|bind| HostMap {
                source: bind.guest_source(),
                target: bind.target.clone(),
                read_only: bind.read_only,
            })
            .collect()
    }

    /// OpenVMM options that export the plan's directories and files at [`GUEST_EXPORT`], with
    /// their access policies and the owner. A file child carries the `file` flag, so OpenVMM
    /// refuses a directory that replaced the file.
    pub fn openvmm_arguments(&self) -> Vec<String> {
        let mut arguments = vec!["--mount-aggregate".to_owned(), GUEST_EXPORT.to_owned()];
        for (index, child) in self.children.iter().enumerate() {
            let mode = if child.writable { "rw" } else { "ro" };
            let kind = if child.file { ",file" } else { "" };
            arguments.push("--mount-child".to_owned());
            arguments.push(format!(
                "{index},{},{mode}{kind}",
                display_path(&child.root)
            ));
        }
        for (option, paths) in [
            (
                "--mount-deny",
                ExportChild::denied_paths as fn(&ExportChild) -> &[PathBuf],
            ),
            ("--mount-allow", ExportChild::allowed_paths),
            ("--mount-write", ExportChild::write_paths),
        ] {
            for path in self.children.iter().flat_map(paths) {
                arguments.push(option.to_owned());
                arguments.push(display_path(path));
            }
        }
        if self.owner == Owner::Caller {
            arguments.extend(["--mount-owner".to_owned(), "caller".to_owned()]);
        }
        arguments
    }
}

impl ExportChild {
    /// Returns the paths that OpenVMM hides.
    fn denied_paths(&self) -> &[PathBuf] {
        &self.denied
    }

    /// Returns the paths that stay visible inside denied paths.
    fn allowed_paths(&self) -> &[PathBuf] {
        &self.allowed
    }

    /// Returns the paths to which OpenVMM limits the guest's writes.
    fn write_paths(&self) -> &[PathBuf] {
        &self.write
    }
}

/// Applies OpenVMM's rules for exported directories, which it reaches without links.
fn check_root(root: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in root.components() {
        current.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            Error::unsupported(format!("cannot inspect {}", current.display())).with_source(error)
        })?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(Error::unsupported(format!(
                "OpenVMM cannot export {}, because {} is a link or reparse point",
                root.display(),
                current.display()
            )));
        }
    }
    Ok(())
}

/// Applies OpenVMM's rules for the paths that it is to `action` in the exported directory `root`,
/// so a policy that OpenVMM would refuse fails at provision rather than at every start: those of
/// [`policy_limits`], and on the host, no link or reparse point below `root` and no other file
/// system. Returns their combined length.
fn check_policy_paths(root: &Path, paths: &[PathBuf], action: &str) -> Result<usize> {
    let unsupported = |message: String| Err(Error::unsupported(message));
    let total = policy_limits(root, paths, action).map_err(Error::unsupported)?;
    for path in paths {
        // `policy_limits` read each path as text below `root` already.
        let relative = relative_text(root, path).unwrap_or_default();
        // The exported directory itself, which a denied root names, was checked as a root.
        let components = relative
            .split('/')
            .filter(|component| !component.is_empty());
        let mut current = root.to_path_buf();
        for component in components {
            current.push(component);
            let metadata = fs::symlink_metadata(&current).map_err(|error| {
                Error::unsupported(format!("cannot inspect {}", current.display()))
                    .with_source(error)
            })?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return unsupported(format!(
                    "OpenVMM cannot {action} {}, because {} is a link or reparse point",
                    path.display(),
                    current.display()
                ));
            }
        }
        #[cfg(unix)]
        if platform::file_identity(path).ok().map(|(device, _)| device)
            != platform::file_identity(root).ok().map(|(device, _)| device)
        {
            return unsupported(format!(
                "OpenVMM cannot {action} {}, which lies on another file system than {}",
                path.display(),
                root.display()
            ));
        }
    }
    Ok(total)
}

/// Applies those of OpenVMM's rules for the paths that it is to `action` in the exported directory
/// `root` that read nothing from the host: at most [`MAX_POLICY_PATHS`] paths, each valid UTF-8
/// below `root`, of at most [`MAX_POLICY_PATH_BYTES`] bytes there, without colons, backslashes,
/// whitespace other than spaces, or a name that begins or ends with a space, and of at most
/// [`MAX_POLICY_BYTES`] bytes together. Returns their combined length, or why OpenVMM would refuse
/// them.
fn policy_limits(
    root: &Path,
    paths: &[PathBuf],
    action: &str,
) -> std::result::Result<usize, String> {
    let count = paths.len();
    if count > MAX_POLICY_PATHS {
        return Err(format!(
            "OpenVMM can {action} at most {MAX_POLICY_PATHS} paths in one exported directory, \
             but {} needs {count}",
            root.display()
        ));
    }
    let mut total = 0;
    for path in paths {
        let Some(relative) = relative_text(root, path) else {
            return Err(format!("{} is not valid UTF-8", path.display()));
        };
        // A name may contain spaces, but not begin or end with one.
        if relative.len() > MAX_POLICY_PATH_BYTES
            || relative.chars().any(|character| {
                (character.is_whitespace() && character != ' ') || matches!(character, ':' | '\\')
            })
            || relative
                .split('/')
                .any(|name| name.starts_with(' ') || name.ends_with(' '))
        {
            return Err(format!(
                "OpenVMM cannot {action} {}: below the exported directory {}, a path must not \
                 contain colons, backslashes, or whitespace other than spaces, and no name may \
                 begin or end with a space",
                path.display(),
                root.display()
            ));
        }
        total += relative.len();
    }
    if total > MAX_POLICY_BYTES {
        return Err(format!(
            "the paths that OpenVMM is to {action} in {} exceed its {MAX_POLICY_BYTES}-byte \
             limit",
            root.display()
        ));
    }
    Ok(total)
}

#[cfg(windows)]
/// Returns whether Windows marks `metadata` as a reparse point.
pub(crate) fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
/// Reports no reparse points on hosts whose metadata has no such Windows attribute.
pub(crate) fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

/// Returns the stable device-and-file identity of `path` for rename detection.
fn identity(path: &Path) -> Result<FileIdentity> {
    platform::file_identity(path)
        .map(|(device, index)| FileIdentity { device, index })
        .map_err(|error| {
            Error::unsupported(format!("cannot identify {}", path.display())).with_source(error)
        })
}

/// Path of `path` relative to `root`, `/`-separated; empty for the root itself.
fn relative_text(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        parts.push(component.as_os_str().to_str()?);
    }
    Some(parts.join("/"))
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

/// Returns whether the guest path `path` is `directory` or one of its descendants, comparing
/// whole components: `/run/nvx` is within `/run`, but `/runner` is not, and every absolute path is
/// within the root, `/`.
pub(crate) fn within(path: &str, directory: &str) -> bool {
    path == directory
        || path
            .strip_prefix(directory)
            .is_some_and(|rest| directory.ends_with('/') || rest.starts_with('/'))
}

/// Counts the nonempty components of an absolute guest path.
fn depth(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}

/// Resolves links and returns a path without the Windows verbatim prefix, which must still name
/// the object that the resolved path names.
///
/// Windows reads a path without the prefix differently: it drops a trailing period or space of a
/// name, among other things. A path that resolves to itself again names the same object; one that
/// only the verbatim prefix keeps, such as that of a directory whose name ends in a period, would
/// name another object, or none, so it is refused.
pub(crate) fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    let Some(stripped) = strip_verbatim(&canonical) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "network paths are unsupported",
        ));
    };
    if stripped != canonical && !fs::canonicalize(&stripped).is_ok_and(|again| again == canonical) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "only a verbatim path names it, as with a name that ends in a period or a space",
        ));
    }
    Ok(stripped)
}

/// Canonicalizes the deepest existing ancestor and appends the rest. Returns whether the
/// complete path exists.
fn canonicalize_lenient(path: &Path) -> std::io::Result<(PathBuf, bool)> {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match canonicalize(&existing) {
            Ok(canonical) => {
                let exists = missing.is_empty();
                let mut resolved = canonical;
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok((resolved, exists));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name().map(ToOwned::to_owned) else {
                    return Err(error);
                };
                missing.push(name);
                if !existing.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
/// Removes a local Windows verbatim prefix and rejects UNC paths.
fn strip_verbatim(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.starts_with("UNC\\") => None,
        Some(rest) => Some(PathBuf::from(rest)),
        None if text.starts_with(r"\\") => None,
        None => Some(path.to_path_buf()),
    }
}

#[cfg(not(windows))]
/// Returns the unchanged path on hosts without Windows verbatim prefixes.
fn strip_verbatim(path: &Path) -> Option<PathBuf> {
    Some(path.to_path_buf())
}

/// Returns a lossily decoded path for user-facing diagnostics.
fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    /// The lists of a filesystem policy as callers hold them, under the names that messages give
    /// them.
    #[derive(Debug, Default)]
    struct FilesystemPolicy {
        readonly_paths: Vec<PathBuf>,
        readwrite_paths: Vec<PathBuf>,
        denied_paths: Vec<PathBuf>,
    }

    fn plan(policy: &FilesystemPolicy) -> Result<Option<ExportPlan>> {
        let mappings: Vec<Mapping<'_>> = Mapping::convention(
            PathList {
                name: "filesystem.readonlyPaths",
                paths: &policy.readonly_paths,
            },
            true,
        )
        .chain(Mapping::convention(
            PathList {
                name: "filesystem.readwritePaths",
                paths: &policy.readwrite_paths,
            },
            false,
        ))
        .collect();
        super::plan(&ExportRequest {
            denied: PathList {
                name: "filesystem.deniedPaths",
                paths: &policy.denied_paths,
            },
            ..ExportRequest::new(&mappings, ReservedTargets::BLOCKLESS_ROOT)
        })
    }

    fn root() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "work/src/secret",
            "work/out",
            "work/private",
            "projects/app",
            "projects/private",
            "build-output",
            "tools/bin",
        ] {
            fs::create_dir_all(directory.path().join(name)).unwrap();
        }
        fs::write(directory.path().join("work/config.json"), b"{}").unwrap();
        fs::write(directory.path().join("work/run.sh"), b"true").unwrap();
        directory
    }

    fn canonical(path: &Path) -> PathBuf {
        canonicalize(path).unwrap()
    }

    fn sources(mapping: &ExportPlan) -> Vec<(String, bool)> {
        mapping
            .binds
            .iter()
            .map(|bind| (bind.guest_source(), bind.read_only))
            .collect()
    }

    fn option_values(arguments: &[String], option: &str) -> Vec<String> {
        arguments
            .windows(2)
            .filter(|pair| pair[0] == option)
            .map(|pair| pair[1].clone())
            .collect()
    }

    #[test]
    fn guest_paths_follow_the_mxc_convention() {
        if cfg!(windows) {
            assert_eq!(
                guest_path(Path::new(r"C:\Work\src")).as_deref(),
                Some("/mnt/c/Work/src")
            );
            assert_eq!(guest_path(Path::new(r"D:\")).as_deref(), Some("/mnt/d"));
            assert_eq!(
                guest_path(Path::new(r"\\?\C:\x\.\y")).as_deref(),
                Some("/mnt/c/x/y")
            );
            assert_eq!(guest_path(Path::new(r"\\server\share\x")), None);
            assert_eq!(guest_path(Path::new(r"\x")), None);
        } else {
            assert_eq!(
                guest_path(Path::new("/home/me/src")).as_deref(),
                Some("/home/me/src")
            );
        }
        assert_eq!(guest_path(Path::new("relative")), None);
        assert_eq!(guest_path(Path::new("..")), None);
    }

    /// Verifies that a guest path is within itself and its ancestors only, comparing whole
    /// components, as the reserved targets here and the sandbox's share checks require.
    #[test]
    fn guest_paths_are_within_themselves_and_their_ancestors() {
        for (path, directory) in [
            ("/run", "/run"),
            ("/run/nvx", "/run"),
            ("/run/nvx/t", "/run"),
            ("/", "/"),
            ("/run", "/"),
            ("/run/nvx", "/"),
        ] {
            assert!(within(path, directory), "{path} {directory}");
        }
        for (path, directory) in [
            ("/runner", "/run"),
            ("/run", "/run/nvx"),
            ("/srv/run", "/run"),
            ("/", "/run"),
        ] {
            assert!(!within(path, directory), "{path} {directory}");
        }
    }

    /// The layout of microsoft/nvx#281: directories that share no directory but the volume's
    /// root, each with its own access.
    #[test]
    fn unrelated_directories_are_exported_with_their_own_access() {
        let directory = root();
        let base = directory.path();
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("tools")],
            readwrite_paths: vec![base.join("projects/app"), base.join("build-output")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        let children: Vec<(PathBuf, bool)> = mapping
            .children
            .iter()
            .map(|child| (child.root.clone(), child.writable))
            .collect();
        assert_eq!(
            children,
            [
                (canonical(&base.join("build-output")), true),
                (canonical(&base.join("projects/app")), true),
                (canonical(&base.join("tools")), false),
            ]
        );
        assert!(mapping.children.iter().all(|child| child.write.is_empty()
            && child.allowed.is_empty()
            && child.denied.is_empty()));
        // Parents mount before children, so the shallower targets come first.
        assert_eq!(
            sources(&mapping),
            [
                ("0".to_owned(), false),
                ("2".to_owned(), true),
                ("1".to_owned(), false)
            ]
        );

        let arguments = mapping.openvmm_arguments();
        assert_eq!(arguments[..2], ["--mount-aggregate", GUEST_EXPORT]);
        assert_eq!(
            option_values(&arguments, "--mount-child"),
            [
                format!(
                    "0,{},rw",
                    display_path(&canonical(&base.join("build-output")))
                ),
                format!(
                    "1,{},rw",
                    display_path(&canonical(&base.join("projects/app")))
                ),
                format!("2,{},ro", display_path(&canonical(&base.join("tools")))),
            ]
        );
        assert_eq!(arguments.len(), 8);
        assert_eq!(mapping.kernel_token(), "nvx_maps=3");
        let table = mapping.guest_table();
        assert_eq!(table.len(), 3);
        assert_eq!(
            table[1].target,
            guest_path(&canonical(&base.join("tools"))).unwrap()
        );
        assert!(table[1].read_only);
    }

    /// The layout of microsoft/nvx#283: directories whose names hold commas, each with a denied
    /// subdirectory.
    #[test]
    fn host_paths_may_contain_commas() {
        let directory = root();
        let base = directory.path();
        let alpha = base.join("projects/alpha,beta");
        let reports = base.join("reports/2026, final");
        for path in [&alpha, &reports] {
            fs::create_dir_all(path.join("secret")).unwrap();
        }
        let policy = FilesystemPolicy {
            readonly_paths: vec![reports.clone()],
            readwrite_paths: vec![alpha.clone()],
            denied_paths: vec![alpha.join("secret"), reports.join("secret")],
        };
        let mapping = plan(&policy).unwrap().unwrap();
        let children: Vec<(PathBuf, bool, Vec<PathBuf>)> = mapping
            .children
            .iter()
            .map(|child| (child.root.clone(), child.writable, child.denied.clone()))
            .collect();
        assert_eq!(
            children,
            [
                (
                    canonical(&alpha),
                    true,
                    vec![canonical(&alpha.join("secret"))]
                ),
                (
                    canonical(&reports),
                    false,
                    vec![canonical(&reports.join("secret"))]
                ),
            ]
        );
        // OpenVMM reads each child's mode after the last comma, so the host paths keep theirs.
        let arguments = mapping.openvmm_arguments();
        assert_eq!(
            option_values(&arguments, "--mount-child"),
            [
                format!("0,{},rw", display_path(&canonical(&alpha))),
                format!("1,{},ro", display_path(&canonical(&reports))),
            ]
        );
        assert_eq!(
            option_values(&arguments, "--mount-deny"),
            [
                display_path(&canonical(&alpha.join("secret"))),
                display_path(&canonical(&reports.join("secret"))),
            ]
        );
        // So do the guest paths, which the control channel carries with their lengths.
        let mut table: Vec<(String, bool)> = mapping
            .guest_table()
            .into_iter()
            .map(|map| (map.target, map.read_only))
            .collect();
        table.sort();
        assert_eq!(
            table,
            [
                (guest_path(&canonical(&alpha)).unwrap(), false),
                (guest_path(&canonical(&reports)).unwrap(), true),
            ]
        );
        assert!(table[0].0.ends_with("/projects/alpha,beta"), "{table:?}");
        assert!(table[1].0.ends_with("/reports/2026, final"), "{table:?}");
    }

    #[test]
    fn mapped_files_are_exported_alone() {
        let directory = root();
        let base = directory.path();
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("work/config.json"), base.join("work/src")],
            readwrite_paths: vec![base.join("work/run.sh"), base.join("work/out")],
            denied_paths: vec![
                // Not exported at all: no child holds it.
                base.join("work/private"),
                base.join("work/src/secret"),
            ],
        };
        let mapping = plan(&policy).unwrap().unwrap();
        let work = canonical(&base.join("work"));
        // Each file is a child of its own, without its directory, beside the directories.
        let children: Vec<(PathBuf, bool, bool)> = mapping
            .children
            .iter()
            .map(|child| (child.root.clone(), child.file, child.writable))
            .collect();
        assert_eq!(
            children,
            [
                (work.join("config.json"), true, false),
                (work.join("out"), false, true),
                (work.join("run.sh"), true, true),
                (work.join("src"), false, false),
            ]
        );
        assert!(
            mapping
                .children
                .iter()
                .all(|child| child.allowed.is_empty() && child.write.is_empty())
        );
        assert_eq!(
            mapping.children[3].denied,
            [work.join("src").join("secret")]
        );
        assert_eq!(
            sources(&mapping),
            [
                ("0".to_owned(), true),
                ("1".to_owned(), false),
                ("2".to_owned(), false),
                ("3".to_owned(), true),
            ]
        );

        // OpenVMM exposes each file as the child itself, so nothing hides its directory, and the
        // `file` flag makes it refuse a directory in the file's place.
        let arguments = mapping.openvmm_arguments();
        assert_eq!(
            option_values(&arguments, "--mount-child")[0],
            format!("0,{},ro,file", display_path(&work.join("config.json")))
        );
        assert_eq!(
            option_values(&arguments, "--mount-deny"),
            [display_path(&work.join("src").join("secret"))]
        );
        assert!(option_values(&arguments, "--mount-allow").is_empty());
        assert!(option_values(&arguments, "--mount-write").is_empty());

        // A record without file children leaves the field out, and one with them names it.
        let record = serde_json::to_value(&mapping).unwrap();
        assert_eq!(record["children"][0]["file"], true);
        assert!(record["children"][1].get("file").is_none());
        let restored: ExportPlan = serde_json::from_value(record).unwrap();
        assert_eq!(restored, mapping);

        // A file inside a mapped directory belongs to that directory's child, whose writes
        // OpenVMM limits to the file when the directory is read-only.
        let nested = FilesystemPolicy {
            readonly_paths: vec![base.join("work")],
            readwrite_paths: vec![base.join("work/config.json")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&nested).unwrap().unwrap();
        assert_eq!(mapping.children.len(), 1);
        assert!(!mapping.children[0].file && mapping.children[0].writable);
        assert_eq!(mapping.children[0].write, [work.join("config.json")]);
        assert_eq!(
            sources(&mapping),
            [("0".to_owned(), true), ("0/config.json".to_owned(), false)]
        );

        // One file reached through two names is one object, which is mapped once.
        let link = base.join("work/config-link.json");
        fs::hard_link(base.join("work/config.json"), &link).unwrap();
        let aliased = FilesystemPolicy {
            readonly_paths: vec![base.join("work/config.json"), link],
            ..FilesystemPolicy::default()
        };
        let error = plan(&aliased).unwrap_err();
        assert!(
            error.message().contains("same host directory or file"),
            "{error}"
        );
    }

    #[test]
    fn policies_beyond_the_children_of_an_aggregate_are_refused() {
        let directory = root();
        let base = directory.path();
        let files = |parent: &str, count: usize| -> Vec<PathBuf> {
            (0..count)
                .map(|index| {
                    let path = base.join(parent).join(format!("file-{index:03}"));
                    fs::write(&path, b"x").unwrap();
                    path
                })
                .collect()
        };
        // Up to 256 files are children of their own, however many share a directory.
        let policy = FilesystemPolicy {
            readonly_paths: files("tools", 256),
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert_eq!(mapping.children.len(), 256);
        assert!(
            mapping
                .children
                .iter()
                .all(|child| child.file && child.allowed.is_empty())
        );

        // More are refused rather than exported through their directory, which would grant
        // more than the files.
        let mut readonly_paths = files("tools", 256);
        readonly_paths.extend(files("build-output", 1));
        let error = plan(&FilesystemPolicy {
            readonly_paths,
            ..FilesystemPolicy::default()
        })
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.message().contains("at most 256"), "{error}");
        assert!(
            error
                .message()
                .contains("map directories that contain the files instead"),
            "{error}"
        );

        // A policy that grants the directory instead needs one child.
        let mut readonly_paths = vec![base.join("tools")];
        readonly_paths.extend(files("build-output", 1));
        let mapping = plan(&FilesystemPolicy {
            readonly_paths,
            ..FilesystemPolicy::default()
        })
        .unwrap()
        .unwrap();
        assert_eq!(mapping.children.len(), 2);
    }

    #[test]
    fn nested_mappings_merge_into_the_outermost_directory() {
        let directory = root();
        let base = directory.path();
        fs::create_dir_all(base.join("work/out/cache")).unwrap();
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("work"), base.join("work/src")],
            readwrite_paths: vec![base.join("work/out"), base.join("work/out/cache")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert_eq!(mapping.children.len(), 1);
        let child = &mapping.children[0];
        let work = canonical(&base.join("work"));
        assert!(child.writable);
        // The read-only root is read-only on the host, except the outermost read-write mapping
        // inside it, to which OpenVMM limits writes.
        assert_eq!(child.write, [work.join("out")]);
        assert_eq!(
            sources(&mapping),
            [
                ("0".to_owned(), true),
                ("0/out".to_owned(), false),
                ("0/src".to_owned(), true),
                ("0/out/cache".to_owned(), false),
            ]
        );

        // A read-write root covers everything, so OpenVMM need not limit writes.
        let policy = FilesystemPolicy {
            readwrite_paths: vec![base.join("work"), base.join("work/src")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert!(mapping.children[0].writable && mapping.children[0].write.is_empty());
        assert_eq!(
            sources(&mapping),
            [("0".to_owned(), false), ("0/src".to_owned(), false)]
        );

        // The read-write duplicate of a read-only path is tightened to read-only, so a
        // read-write path inside it is the only writable one.
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("work")],
            readwrite_paths: vec![base.join("work"), base.join("work/src")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert!(mapping.children[0].writable);
        assert_eq!(mapping.children[0].write, [work.join("src")]);
        assert_eq!(
            sources(&mapping),
            [("0".to_owned(), true), ("0/src".to_owned(), false)]
        );
    }

    /// Only the guest's bind mount could keep a read-only path inside a read-write directory
    /// read-only, so such a path is refused on every host, under any of its names, while the
    /// inverse nesting and denied paths inside a read-write directory stay valid.
    #[test]
    fn read_only_paths_inside_read_write_directories_are_refused() {
        let directory = root();
        let base = directory.path();
        fs::create_dir(base.join("work/output")).unwrap();
        let policy = |readonly_paths: &[PathBuf], readwrite_paths: &[PathBuf]| FilesystemPolicy {
            readonly_paths: readonly_paths.to_vec(),
            readwrite_paths: readwrite_paths.to_vec(),
            ..FilesystemPolicy::default()
        };
        let refusal = |inner: &Path, outer: &Path| {
            format!(
                "filesystem.readonlyPaths entry {} lies inside the filesystem.readwritePaths \
                 entry {},",
                inner.display(),
                outer.display()
            )
        };
        let work = base.join("work");
        let (src, config) = (work.join("src"), work.join("config.json"));
        let secret = src.join("secret");
        // A directory, a file, a path deeper inside, a path that is also listed read-write, and a
        // path inside a read-write directory that itself lies inside a read-only one.
        for (readonly, readwrite, inner, outer) in [
            (vec![src.clone()], vec![work.clone()], &src, &work),
            (vec![config.clone()], vec![work.clone()], &config, &work),
            (vec![secret.clone()], vec![work.clone()], &secret, &work),
            (
                vec![src.clone()],
                vec![work.clone(), src.clone()],
                &src,
                &work,
            ),
            (
                vec![work.clone(), secret.clone()],
                vec![src.clone()],
                &secret,
                &src,
            ),
        ] {
            let error = plan(&policy(&readonly, &readwrite)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(
                error.message().starts_with(&refusal(inner, outer)),
                "{error}"
            );
        }
        // Resolved paths decide, so another name of a nested path is refused too: a spelling in
        // other letter case on Windows, and one through a symbolic link elsewhere.
        #[cfg(windows)]
        let alias = base.join("WORK").join("CONFIG.JSON");
        #[cfg(unix)]
        let alias = {
            std::os::unix::fs::symlink(&work, base.join("work-link")).unwrap();
            base.join("work-link").join("config.json")
        };
        let error = plan(&policy(
            std::slice::from_ref(&alias),
            std::slice::from_ref(&work),
        ))
        .unwrap_err();
        assert!(
            error.message().starts_with(&refusal(&alias, &work)),
            "{error}"
        );

        // The inverse nesting is valid: OpenVMM limits the read-only directory's writes to the
        // read-write path inside it.
        let mapping = plan(&policy(std::slice::from_ref(&work), &[work.join("out")]))
            .unwrap()
            .unwrap();
        assert_eq!(mapping.children[0].write, [canonical(&work.join("out"))]);
        // Containment compares whole components, so work/output lies beside work/out.
        let mapping = plan(&policy(&[work.join("output")], &[work.join("out")]))
            .unwrap()
            .unwrap();
        assert_eq!(mapping.children.len(), 2);
        let denied = FilesystemPolicy {
            readwrite_paths: vec![work.clone()],
            denied_paths: vec![work.join("private")],
            ..FilesystemPolicy::default()
        };
        assert_eq!(plan(&denied).unwrap().unwrap().children[0].denied.len(), 1);
    }

    #[test]
    fn denied_paths_belong_to_the_directory_that_contains_them() {
        let directory = root();
        let base = directory.path();
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("work/src"), base.join("projects")],
            readwrite_paths: vec![base.join("build-output")],
            denied_paths: vec![
                base.join("work/src/secret"),
                base.join("projects/private"),
                base.join("work/out"),
                base.join("elsewhere"),
            ],
        };
        let mapping = plan(&policy).unwrap().unwrap();
        let denied: Vec<Vec<PathBuf>> = mapping
            .children
            .iter()
            .map(|child| child.denied.clone())
            .collect();
        assert_eq!(
            denied,
            [
                Vec::new(),
                vec![canonical(&base.join("projects/private"))],
                vec![canonical(&base.join("work/src/secret"))],
            ]
        );
        // Nothing a workload could move is pinned without a read-write parent.
        assert!(
            mapping
                .children
                .iter()
                .flat_map(|child| &child.denied_identities)
                .all(Option::is_none)
        );
        assert_eq!(
            option_values(&mapping.openvmm_arguments(), "--mount-deny").len(),
            2
        );
    }

    /// Checks the order, duplicates, and containment by whole components that the roots, denied
    /// paths, and writable paths share, which needs no host paths.
    #[test]
    fn outermost_paths_are_sorted_once_without_the_paths_inside_others() {
        let paths = ["/b/c", "/a/bc", "/b", "/a/b", "/b/c/d", "/a/b"].map(PathBuf::from);
        assert_eq!(
            outermost(paths.to_vec()),
            ["/a/b", "/a/bc", "/b"].map(PathBuf::from)
        );
    }

    #[test]
    fn unenforceable_policies_are_rejected() {
        let directory = root();
        let base = directory.path();
        let cases = [
            FilesystemPolicy {
                readonly_paths: vec![base.join("missing")],
                ..FilesystemPolicy::default()
            },
            FilesystemPolicy {
                readwrite_paths: vec![base.join("work/out")],
                denied_paths: vec![base.join("work")],
                ..FilesystemPolicy::default()
            },
            FilesystemPolicy {
                readwrite_paths: vec![base.join("work/out")],
                denied_paths: vec![base.join("work/out/not-yet")],
                ..FilesystemPolicy::default()
            },
            FilesystemPolicy {
                readonly_paths: vec![base.join("work/config.json")],
                denied_paths: vec![base.join("work")],
                ..FilesystemPolicy::default()
            },
            // A whole volume is never exported.
            FilesystemPolicy {
                readonly_paths: vec![PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" })],
                ..FilesystemPolicy::default()
            },
        ];
        for policy in cases {
            assert_eq!(
                plan(&policy).unwrap_err().kind(),
                ErrorKind::Unsupported,
                "{policy:?}"
            );
        }
        if !cfg!(windows) {
            let system = FilesystemPolicy {
                readonly_paths: vec![PathBuf::from("/usr")],
                ..FilesystemPolicy::default()
            };
            assert_eq!(plan(&system).unwrap_err().kind(), ErrorKind::Unsupported);
        }
        let empty = FilesystemPolicy {
            denied_paths: vec![base.join("tools")],
            ..FilesystemPolicy::default()
        };
        assert_eq!(plan(&empty).unwrap(), None);
    }

    #[test]
    fn whole_volumes_are_refused() {
        // The paths are only inspected, so nothing is created at a volume root. A file directly
        // in a volume's root is exported alone, so only a whole volume is refused.
        let (volumes, directories): (&[&str], &[&str]) = if cfg!(windows) {
            (&[r"C:\", r"D:\"], &[r"C:\work", r"D:\a\b"])
        } else {
            (&["/"], &["/work", "/srv/a/b"])
        };
        for volume in volumes {
            let reason = volume_root_refusal(Path::new(volume)).unwrap();
            assert!(reason.contains("whole volume"), "{volume}: {reason}");
        }
        for directory in directories {
            assert_eq!(volume_root_refusal(Path::new(directory)), None);
        }
    }

    #[test]
    fn spellings_of_one_object_share_its_guest_path() {
        let directory = root();
        let base = directory.path();
        let upper = if cfg!(windows) {
            base.join("WORK").join("SRC")
        } else {
            base.join("work").join(".").join("src")
        };
        let policy = FilesystemPolicy {
            readonly_paths: vec![upper],
            readwrite_paths: vec![base.join("work").join("src")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert_eq!(mapping.binds.len(), 1);
        assert!(mapping.binds[0].read_only);
        assert!(!mapping.children[0].writable);
        assert_eq!(
            mapping.binds[0].target,
            guest_path(&canonical(&base.join("work").join("src"))).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolved_guest_paths_canonicalize_temp_directory_case_aliases() {
        let directory = root();
        let path = directory.path().join("work").join("out");
        let upper = PathBuf::from(path.to_str().unwrap().to_uppercase());
        let expected = guest_path(&canonical(&path)).unwrap();
        assert_ne!(guest_path(&upper), Some(expected.clone()));
        assert_eq!(resolve_guest_path(&upper), Some(expected));
    }

    #[cfg(windows)]
    #[test]
    fn paths_that_only_a_verbatim_path_names_are_refused() {
        let directory = root();
        let base = directory.path();
        // A verbatim path keeps a trailing period or space of a name, which Windows drops from a
        // path without the prefix: there, `work.` and `work ` would name `work`, and `solo.`
        // nothing.
        let verbatim = fs::canonicalize(base).unwrap();
        let named = |name: &str| PathBuf::from(format!(r"{}\{name}", verbatim.display()));
        for name in ["work.", "work ", "solo.", r"work\src\secret."] {
            fs::create_dir(named(name)).unwrap();
        }
        for name in ["work.", "work ", "solo."] {
            let path = named(name);
            let error = canonicalize(&path).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported, "{error}");
            let policy = FilesystemPolicy {
                readwrite_paths: vec![path],
                ..FilesystemPolicy::default()
            };
            let error = plan(&policy).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert!(error.to_string().contains("is not accessible"), "{error}");
        }
        // A denied path that only a verbatim path names would hide another path instead.
        let policy = FilesystemPolicy {
            readwrite_paths: vec![base.join("work")],
            denied_paths: vec![named(r"work\src\secret.")],
            ..FilesystemPolicy::default()
        };
        let error = plan(&policy).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.to_string().contains("cannot be resolved"), "{error}");
        // A verbatim path that names what the path without its prefix names is planned as that
        // path.
        let policy = FilesystemPolicy {
            readwrite_paths: vec![named("work")],
            ..FilesystemPolicy::default()
        };
        let exported = plan(&policy).unwrap().unwrap();
        assert_eq!(exported.children[0].root, canonical(&base.join("work")));
    }

    #[test]
    fn policy_paths_follow_openvmm_rules() {
        let directory = root();
        let base = directory.path();
        for name in ["work/out/My Secrets", "work/out/ leading", "my files"] {
            fs::create_dir_all(base.join(name)).unwrap();
        }
        fs::write(base.join("my files/notes one.txt"), b"notes").unwrap();
        // Names may contain spaces, in a file that a child exposes alone and in a policy path.
        let spaced = FilesystemPolicy {
            readonly_paths: vec![base.join("my files/notes one.txt")],
            readwrite_paths: vec![base.join("work/out")],
            denied_paths: vec![base.join("work/out/My Secrets")],
        };
        let mapping = plan(&spaced).unwrap().unwrap();
        let arguments = mapping.openvmm_arguments();
        assert!(
            option_values(&arguments, "--mount-child")[0].ends_with("notes one.txt,ro,file"),
            "{arguments:?}"
        );
        assert!(option_values(&arguments, "--mount-deny")[0].ends_with("My Secrets"));
        // But they must not begin or end with one.
        let leading = FilesystemPolicy {
            readwrite_paths: vec![base.join("work/out")],
            denied_paths: vec![base.join("work/out/ leading")],
            ..FilesystemPolicy::default()
        };
        assert_eq!(plan(&leading).unwrap_err().kind(), ErrorKind::Unsupported);
        if !cfg!(windows) {
            fs::create_dir_all(base.join("work/out/tab\there")).unwrap();
            fs::create_dir_all(base.join("work/out/colon:here")).unwrap();
            for name in ["tab\there", "colon:here"] {
                let policy = FilesystemPolicy {
                    readwrite_paths: vec![base.join("work/out")],
                    denied_paths: vec![base.join("work/out").join(name)],
                    ..FilesystemPolicy::default()
                };
                assert_eq!(
                    plan(&policy).unwrap_err().kind(),
                    ErrorKind::Unsupported,
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn openvmm_limits_are_checked_per_directory() {
        let directory = root();
        let base = directory.path();
        let files = |count: usize| -> Vec<PathBuf> {
            (0..count)
                .map(|index| {
                    let path = base.join("tools").join(format!("tool-{index:03}"));
                    fs::write(&path, b"x").unwrap();
                    path
                })
                .collect()
        };
        // One exported directory hides at most 128 denied paths, and singles out at most 128
        // read-write paths inside a read-only one.
        for policy in [
            FilesystemPolicy {
                readonly_paths: vec![base.join("tools")],
                denied_paths: files(129),
                ..FilesystemPolicy::default()
            },
            FilesystemPolicy {
                readonly_paths: vec![base.join("tools")],
                readwrite_paths: files(129),
                ..FilesystemPolicy::default()
            },
        ] {
            let error = plan(&policy).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert!(error.message().contains("at most 128"), "{error}");
        }
    }

    #[test]
    fn swapped_objects_fail_verification() {
        let directory = root();
        let base = directory.path();
        // Everything below the read-write work directory is pinned.
        let policy = FilesystemPolicy {
            readwrite_paths: vec![base.join("work"), base.join("work/src")],
            denied_paths: vec![base.join("work/src/secret")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert!(
            mapping.children[0]
                .denied_identities
                .iter()
                .all(Option::is_some)
        );
        mapping.verify().unwrap();
        fs::rename(base.join("work/src/secret"), base.join("work/src/moved")).unwrap();
        fs::create_dir(base.join("work/src/secret")).unwrap();
        assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);
        fs::remove_dir(base.join("work/src/secret")).unwrap();
        fs::rename(base.join("work/src/moved"), base.join("work/src/secret")).unwrap();
        mapping.verify().unwrap();
        fs::rename(base.join("work/src"), base.join("work/src-moved")).unwrap();
        fs::create_dir_all(base.join("work/src/secret")).unwrap();
        assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);

        // Without a read-write parent, nothing a workload could move is pinned, so host edits
        // that replace objects do not block the next start.
        let read_only = FilesystemPolicy {
            readonly_paths: vec![base.join("tools")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&read_only).unwrap().unwrap();
        fs::remove_dir_all(base.join("tools")).unwrap();
        fs::create_dir(base.join("tools")).unwrap();
        mapping.verify().unwrap();

        // Every child keeps its kind: a directory in place of a file child would grant all of it,
        // and a directory child must remain a directory.
        let settings = base.join("settings.json");
        fs::write(&settings, b"{}").unwrap();
        let with_file = FilesystemPolicy {
            readonly_paths: vec![settings.clone(), base.join("tools")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&with_file).unwrap().unwrap();
        mapping.verify().unwrap();
        fs::remove_file(&settings).unwrap();
        fs::create_dir(&settings).unwrap();
        assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);
        fs::remove_dir(&settings).unwrap();
        fs::write(&settings, b"{}").unwrap();
        mapping.verify().unwrap();
        fs::remove_dir(base.join("tools")).unwrap();
        fs::write(base.join("tools"), b"").unwrap();
        assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);
    }

    /// A recorded plan with a read-only bind in a part of a child that the guest may write, which
    /// only the guest's bind mount would keep read-only, is refused before a guest runs.
    #[test]
    fn read_only_binds_that_the_host_cannot_enforce_fail_verification() {
        let directory = root();
        let base = directory.path();
        fs::create_dir_all(base.join("work/out/cache")).unwrap();
        let refused = |mut mapping: ExportPlan, source: &str| {
            let bind = mapping
                .binds
                .iter_mut()
                .find(|bind| bind.source == source)
                .unwrap();
            bind.read_only = true;
            let error = mapping.verify().unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(
                error.message().contains("read-only inside a part of"),
                "{error}"
            );
        };
        // A read-only path inside a read-write root, all of which the guest may write.
        let writable_root = plan(&FilesystemPolicy {
            readwrite_paths: vec![base.join("work"), base.join("work/src")],
            ..FilesystemPolicy::default()
        })
        .unwrap()
        .unwrap();
        writable_root.verify().unwrap();
        refused(writable_root, "src");
        // A read-only path inside a writable path of a read-only root, whose other read-only
        // paths stay read-only on the host.
        let writable_path = plan(&FilesystemPolicy {
            readonly_paths: vec![base.join("work"), base.join("work/src")],
            readwrite_paths: vec![base.join("work/out"), base.join("work/out/cache")],
            ..FilesystemPolicy::default()
        })
        .unwrap()
        .unwrap();
        writable_path.verify().unwrap();
        refused(writable_path, "out/cache");
    }

    /// A directory on another volume than the temporary directory, if the host has one: the one
    /// that `NVX_TEST_SECOND_VOLUME` names, or the runner's temporary directory
    /// on GitHub's Windows runners. Tests create their files below it. Linux's `/dev/shm` does
    /// not qualify, because it lies in the guest's reserved `/dev`.
    fn second_volume() -> Option<PathBuf> {
        let candidate = std::env::var_os("NVX_TEST_SECOND_VOLUME")
            .or_else(|| std::env::var_os("RUNNER_TEMP"))
            .map(PathBuf::from)?;
        let temporary = canonical(&std::env::temp_dir());
        let candidate = canonicalize(&candidate).ok()?;
        let volume = |path: &Path| platform::file_identity(path).ok().map(|(device, _)| device);
        (candidate.is_dir() && volume(&candidate) != volume(&temporary)).then_some(candidate)
    }

    #[test]
    fn directories_on_other_volumes_are_exported_too() {
        let Some(volume) = second_volume() else {
            eprintln!("skipped: the host has no second volume");
            return;
        };
        let directory = root();
        let other = tempfile::tempdir_in(volume).unwrap();
        fs::create_dir(other.path().join("cache")).unwrap();
        let policy = FilesystemPolicy {
            readonly_paths: vec![directory.path().join("tools")],
            readwrite_paths: vec![other.path().join("cache")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert_eq!(mapping.children.len(), 2);
        let modes: Vec<bool> = mapping
            .children
            .iter()
            .map(|child| child.writable)
            .collect();
        assert!(modes.contains(&true) && modes.contains(&false));
    }
    /// Returns a mapping of `path` at the guest path `target`.
    fn at<'a>(path: &'a Path, target: &'a str, read_only: bool) -> Mapping<'a> {
        Mapping {
            name: "mappings",
            path,
            target: Target::Guest(target),
            read_only,
        }
    }

    #[test]
    fn explicit_targets_place_mappings_at_any_free_guest_path() {
        let directory = root();
        let base = directory.path();
        let (work, tools) = (base.join("work"), base.join("tools"));
        let config = work.join("config.json");
        let mappings = [
            at(&work, "/workspace", false),
            at(&tools, "/opt/tools", true),
            // A target inside another shows what the other shows there.
            at(&config, "/workspace/config.json", false),
            // One object may appear at several targets.
            at(&work, "/mirror", false),
        ];
        let request = ExportRequest::new(&mappings, ReservedTargets::CONTAINER_ROOT);
        let mapping = super::plan(&request).unwrap().unwrap();
        let roots: Vec<&Path> = mapping
            .children
            .iter()
            .map(|child| child.root.as_path())
            .collect();
        assert_eq!(roots, [canonical(&tools), canonical(&work)]);
        let binds: Vec<(&str, String, bool)> = mapping
            .binds
            .iter()
            .map(|bind| (bind.target.as_str(), bind.guest_source(), bind.read_only))
            .collect();
        assert_eq!(
            binds,
            [
                ("/mirror", "1".to_owned(), false),
                ("/workspace", "1".to_owned(), false),
                ("/opt/tools", "0".to_owned(), true),
                ("/workspace/config.json", "1/config.json".to_owned(), false),
            ]
        );
        mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();
        // A root that keeps its whole tree refuses every mapping, planned or recorded.
        let everything = ReservedTargets {
            trees: &["/"],
            exact: &[],
        };
        for error in [
            super::plan(&ExportRequest::new(&mappings, everything)).unwrap_err(),
            mapping.check(everything).unwrap_err(),
        ] {
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(
                error.message().ends_with("would cover the guest's /"),
                "{error}"
            );
        }
        // The convention's plans pass the check of their root too.
        let convention = plan(&FilesystemPolicy {
            readonly_paths: vec![tools.clone()],
            readwrite_paths: vec![work.clone(), config.clone()],
            ..FilesystemPolicy::default()
        })
        .unwrap()
        .unwrap();
        convention.check(ReservedTargets::BLOCKLESS_ROOT).unwrap();

        let refused = |mappings: &[Mapping<'_>], expected: &str| {
            let request = ExportRequest::new(mappings, ReservedTargets::CONTAINER_ROOT);
            let error = super::plan(&request).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(
                error.message().contains(expected),
                "{error} lacks {expected}"
            );
        };
        refused(
            &[at(&work, "/data", true), at(&tools, "/data", true)],
            "have the same guest target /data",
        );
        refused(
            &[at(&work, "/data", true), at(&tools, "/data/tools", true)],
            "at /data/tools would hide part of the mappings entry",
        );
        // A file shows nothing below it.
        refused(
            &[at(&config, "/data", true), at(&tools, "/data/tools", true)],
            "would hide part of",
        );
        let long = format!("/{}", "x".repeat(4095));
        for (target, problem) in [
            ("data", "is not absolute"),
            ("/", "is the guest's root"),
            ("/a//b", "has an empty, `.`, or `..` component"),
            ("/a/./b", "has an empty, `.`, or `..` component"),
            ("/a/../b", "has an empty, `.`, or `..` component"),
            ("/a/", "has an empty, `.`, or `..` component"),
            ("/a\0b", "contains a NUL character"),
            (long.as_str(), "is longer than a guest path may be"),
        ] {
            refused(&[at(&work, target, true)], problem);
        }
        refused(
            &[at(&work, "/etc", true)],
            "has the guest target /etc, which would cover the guest's /etc",
        );
        refused(
            &[at(&work, "/proc/self", true)],
            "would cover the guest's /proc",
        );
        refused(
            &[at(&config, "/etc/resolv.conf", true)],
            "has the guest target /etc/resolv.conf, which would cover the guest's /etc/resolv.conf",
        );
        // The same object read-only and read-write at two targets would let the guest write it.
        refused(
            &[at(&work, "/a", false), at(&work, "/b", true)],
            "names the object of the mappings entry",
        );
        // A target inside another must spell, name for name, the host path below the other's
        // object, so a backslash in a guest name, which a Windows host reads as a separator, and a
        // guest name that a Windows host reads as a root of its own show nothing there.
        let host = work.join("src").join("host");
        fs::create_dir_all(&host).unwrap();
        refused(
            &[
                at(&work, "/workspace", true),
                at(&host, r"/workspace/src\host", true),
            ],
            "would hide part of",
        );
        let resolved = canonical(&work);
        let text = resolved.to_str().unwrap();
        let rooted = text
            .split_once(':')
            .map_or(text, |(_, rest)| rest)
            .replace('/', r"\");
        let inside = format!("/opt/tools/{rooted}");
        refused(
            &[at(&tools, "/opt/tools", true), at(&work, &inside, true)],
            "would hide part of",
        );
    }

    #[test]
    fn reserved_targets_depend_on_the_root() {
        let container = ReservedTargets::CONTAINER_ROOT;
        for (target, reserved) in [
            ("/", "/"),
            ("/proc", "/proc"),
            ("/proc/self", "/proc"),
            ("/sys/fs", "/sys"),
            ("/dev/shm", "/dev"),
            ("/.nvx-agent/tools", "/.nvx-agent"),
            ("/etc", "/etc"),
            ("/etc/machine-id", "/etc/machine-id"),
            ("/etc/resolv.conf", "/etc/resolv.conf"),
            ("/run", "/run"),
            // The ancestors of the runtime's trees are reserved, as exact paths here.
            ("/run/nvx", "/run/nvx"),
            ("/run/nvx/time/state", "/run/nvx/time"),
        ] {
            assert_eq!(container.reservation(target), Some(reserved), "{target}");
        }
        for target in [
            "/etc/app",
            "/etc/resolvconf",
            "/run/secrets",
            "/run/nvx/other",
            "/usr/local",
            "/workspace",
            "/procfs",
        ] {
            assert_eq!(container.reservation(target), None, "{target}");
        }
        let blockless = ReservedTargets::BLOCKLESS_ROOT;
        for (target, reserved) in [
            ("/", "/"),
            ("/usr/local", "/usr"),
            ("/run", "/run"),
            ("/etc/app", "/etc"),
            ("/init", "/init"),
        ] {
            assert_eq!(blockless.reservation(target), Some(reserved), "{target}");
        }
        for target in ["/workspace", "/mnt/c/work", "/home/me", "/initrd", "/opt"] {
            assert_eq!(blockless.reservation(target), None, "{target}");
        }
        // A mapping at an ancestor of a reserved tree would shadow it.
        let nested = ReservedTargets {
            trees: &["/srv/runtime/state"],
            exact: &[],
        };
        assert_eq!(nested.reservation("/srv"), Some("/srv/runtime/state"));
        assert_eq!(
            nested.reservation("/srv/runtime"),
            Some("/srv/runtime/state")
        );
        assert_eq!(nested.reservation("/srv/other"), None);
        // A root that keeps its whole tree takes no mapping.
        let everything = ReservedTargets {
            trees: &["/"],
            exact: &[],
        };
        for target in ["/", "/workspace", "/srv/runtime/state"] {
            assert_eq!(everything.reservation(target), Some("/"), "{target}");
        }
    }

    #[test]
    fn policy_paths_hide_show_and_narrow_writes() {
        let directory = root();
        let base = directory.path();
        fs::create_dir_all(base.join("work/src/secret/public")).unwrap();
        fs::create_dir_all(base.join("work/out/cache")).unwrap();
        let work = base.join("work");
        let paths =
            |names: &[&str]| -> Vec<PathBuf> { names.iter().map(|name| work.join(name)).collect() };
        let request = |mappings: &[Mapping<'_>],
                       denied: &[PathBuf],
                       allowed: &[PathBuf],
                       write: &[PathBuf]| {
            let request = ExportRequest {
                denied: PathList {
                    name: "denied",
                    paths: denied,
                },
                allowed: PathList {
                    name: "allowed",
                    paths: allowed,
                },
                write: PathList {
                    name: "write",
                    paths: write,
                },
                ..ExportRequest::new(mappings, ReservedTargets::CONTAINER_ROOT)
            };
            super::plan(&request)
        };
        let whole = [at(&work, "/workspace", false)];
        let (denied, allowed, write) = (
            paths(&["src/secret"]),
            paths(&["src/secret/public"]),
            paths(&["out"]),
        );
        let mapping = request(&whole, &denied, &allowed, &write).unwrap().unwrap();
        let child = &mapping.children[0];
        let resolved = canonical(&work);
        assert!(child.writable);
        assert_eq!(child.denied, [resolved.join("src").join("secret")]);
        assert_eq!(
            child.allowed,
            [resolved.join("src").join("secret").join("public")]
        );
        assert_eq!(child.write, [resolved.join("out")]);
        // Writes are narrowed to `out`, so nothing outside it can move.
        assert_eq!(child.denied_identities, [None]);
        let arguments = mapping.openvmm_arguments();
        let text = |path: PathBuf| vec![display_path(&path)];
        assert_eq!(
            option_values(&arguments, "--mount-deny"),
            text(resolved.join("src").join("secret"))
        );
        assert_eq!(
            option_values(&arguments, "--mount-allow"),
            text(resolved.join("src").join("secret").join("public"))
        );
        assert_eq!(
            option_values(&arguments, "--mount-write"),
            text(resolved.join("out"))
        );
        assert!(!arguments.contains(&"--mount-owner".to_owned()));
        mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();

        // A denied root shows only its allowed paths.
        let mapping = request(&whole, std::slice::from_ref(&work), &paths(&["src"]), &[])
            .unwrap()
            .unwrap();
        assert_eq!(mapping.children[0].denied, std::slice::from_ref(&resolved));
        assert_eq!(mapping.children[0].allowed, [resolved.join("src")]);
        let error = request(&whole, std::slice::from_ref(&work), &[], &[]).unwrap_err();
        assert!(
            error.message().contains("contains the mapped path"),
            "{error}"
        );

        // A read-only path may lie in the part of a narrowed directory that stays read-only, and
        // the directory may be mapped read-only elsewhere too.
        let src = work.join("src");
        let parts = [
            at(&work, "/workspace", false),
            at(&src, "/src", true),
            at(&work, "/view", true),
        ];
        request(&parts, &[], &[], &write).unwrap().unwrap();

        let refused = |mappings: &[Mapping<'_>],
                       denied: &[PathBuf],
                       allowed: &[PathBuf],
                       write: &[PathBuf],
                       expected: &str| {
            let error = request(mappings, denied, allowed, write).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(
                error.message().contains(expected),
                "{error} lacks {expected}"
            );
        };
        refused(
            &whole,
            &[],
            &paths(&["src"]),
            &[],
            "lies inside no denied entry",
        );
        refused(
            &whole,
            std::slice::from_ref(&work),
            std::slice::from_ref(&work),
            &[],
            "is an exported path itself",
        );
        refused(
            &whole,
            &paths(&["src"]),
            &paths(&["src/secret", "src/secret/public"]),
            &[],
            "which shows it already",
        );
        refused(
            &whole,
            &paths(&["src"]),
            &paths(&["src"]),
            &[],
            "is also a denied entry",
        );
        refused(
            &[at(&work, "/workspace", true)],
            &[],
            &[],
            &write,
            "lies in no directory that is mapped read-write as a whole",
        );
        refused(
            &whole,
            &[],
            &[],
            std::slice::from_ref(&work),
            "lies in no directory that is mapped read-write as a whole",
        );
        refused(
            &whole,
            &paths(&["src"]),
            &[],
            &paths(&["src/secret"]),
            "which hides it",
        );
        let cache = work.join("out/cache");
        refused(
            &[at(&work, "/workspace", false), at(&cache, "/cache", true)],
            &[],
            &[],
            &write,
            "lies inside the write entry",
        );
        refused(
            &[at(&work, "/workspace", false), at(&work, "/view", true)],
            &[],
            &[],
            &[],
            "names the object of the mappings entry",
        );
    }

    #[test]
    fn narrowed_writes_pin_only_what_lies_in_writable_paths() {
        let directory = root();
        let base = directory.path();
        fs::create_dir_all(base.join("work/out/cache")).unwrap();
        fs::create_dir_all(base.join("work/out/private")).unwrap();
        let work = base.join("work");
        let (cache, src) = (work.join("out/cache"), work.join("src"));
        let mappings = [
            at(&work, "/workspace", false),
            at(&cache, "/cache", false),
            at(&src, "/src", true),
        ];
        let denied = [work.join("out/private"), work.join("src/secret")];
        let write = [work.join("out")];
        let request = ExportRequest {
            denied: PathList {
                name: "denied",
                paths: &denied,
            },
            write: PathList {
                name: "write",
                paths: &write,
            },
            ..ExportRequest::new(&mappings, ReservedTargets::CONTAINER_ROOT)
        };
        let mapping = super::plan(&request).unwrap().unwrap();
        let child = &mapping.children[0];
        assert_eq!(child.write, [canonical(&work).join("out")]);
        let pinned: Vec<bool> = child
            .denied_identities
            .iter()
            .map(Option::is_some)
            .collect();
        assert_eq!(pinned, [true, false]);
        let pinned: Vec<(&str, bool)> = mapping
            .binds
            .iter()
            .map(|bind| (bind.target.as_str(), bind.identity.is_some()))
            .collect();
        assert_eq!(
            pinned,
            [("/cache", true), ("/src", false), ("/workspace", false)]
        );
        // A record keeps every pin of the plan, and narrows writes only to paths that keep its
        // read-write mappings writable.
        mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();
        let refused = |change: fn(&mut ExportPlan), expected: &str| {
            let mut changed = mapping.clone();
            change(&mut changed);
            let error = changed.check(ReservedTargets::CONTAINER_ROOT).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(error.message().contains(expected), "{error}");
        };
        refused(
            |changed| changed.children[0].denied_identities[0] = None,
            "does not pin the denied path",
        );
        refused(
            |changed| changed.binds[0].identity = None,
            "does not pin the object that it maps at /cache",
        );
        refused(
            |changed| changed.children[0].write = vec![changed.children[0].root.join("other")],
            "which it maps read-write, stays read-only",
        );
        // Without write paths, the guest could move `src/secret` too.
        refused(
            |changed| changed.children[0].write.clear(),
            "does not pin the denied path",
        );
        mapping.verify().unwrap();
        fs::rename(&cache, work.join("out/moved")).unwrap();
        fs::create_dir(&cache).unwrap();
        assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);
    }

    #[test]
    fn the_caller_owner_reaches_openvmm_on_linux_hosts() {
        let directory = root();
        let work = directory.path().join("work");
        let mappings = [at(&work, "/workspace", false)];
        let request = ExportRequest {
            owner: Owner::Caller,
            ..ExportRequest::new(&mappings, ReservedTargets::CONTAINER_ROOT)
        };
        if cfg!(windows) {
            let error = super::plan(&request).unwrap_err();
            assert!(error.message().contains("requires a Linux host"), "{error}");
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let owned = fs::metadata(&work).unwrap();
            if owned.uid() != 0 && owned.gid() != 0 {
                let mapping = super::plan(&request).unwrap().unwrap();
                assert_eq!(mapping.owner, Owner::Caller);
                let arguments = mapping.openvmm_arguments();
                assert_eq!(
                    arguments[arguments.len() - 2..],
                    ["--mount-owner", "caller"]
                );
                mapping.verify().unwrap();
                let record = serde_json::to_value(&mapping).unwrap();
                assert_eq!(record["owner"], "caller");
                assert_eq!(
                    serde_json::from_value::<ExportPlan>(record).unwrap(),
                    mapping
                );
            }
            // Guest root is squashed to the owner, which must not be root.
            let system = Path::new("/usr");
            if fs::metadata(system).is_ok_and(|metadata| metadata.uid() == 0) {
                let mappings = [at(system, "/srv/system", true)];
                let request = ExportRequest {
                    owner: Owner::Caller,
                    ..ExportRequest::new(&mappings, ReservedTargets::CONTAINER_ROOT)
                };
                let error = super::plan(&request).unwrap_err();
                assert!(
                    error
                        .message()
                        .contains("must not be owned by UID 0 or GID 0"),
                    "{error}"
                );
            }
        }
        // The default owner is left out of the record.
        let vmm = super::plan(&ExportRequest::new(
            &mappings,
            ReservedTargets::CONTAINER_ROOT,
        ))
        .unwrap()
        .unwrap();
        assert!(serde_json::to_value(&vmm).unwrap().get("owner").is_none());
    }

    #[test]
    fn read_write_mappings_to_which_writes_are_narrowed_stay_visible() {
        let directory = root();
        let base = directory.path();
        fs::create_dir_all(base.join("tree/private/public")).unwrap();
        fs::create_dir_all(base.join("tree/out")).unwrap();
        let (tree, private) = (base.join("tree"), base.join("tree/private"));
        let denied = [private.clone()];
        let allowed = [private.join("public")];
        let request = |mappings: &[Mapping<'_>], write: &[PathBuf]| {
            super::plan(&ExportRequest {
                denied: PathList {
                    name: "denied",
                    paths: &denied,
                },
                allowed: PathList {
                    name: "allowed",
                    paths: &allowed,
                },
                write: PathList {
                    name: "write",
                    paths: write,
                },
                ..ExportRequest::new(mappings, ReservedTargets::CONTAINER_ROOT)
            })
        };
        // OpenVMM narrows the writes of the tree to the read-write private, which it cannot hide:
        // in a read-only tree, and in a read-write tree whose writes are narrowed.
        let out = [tree.join("out")];
        for (mappings, write) in [
            (
                [at(&tree, "/tree", true), at(&private, "/private", false)],
                &[][..],
            ),
            (
                [at(&tree, "/tree", false), at(&private, "/private", false)],
                &out[..],
            ),
        ] {
            let error = request(&mappings, write).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(
                error
                    .message()
                    .contains("OpenVMM cannot make a hidden path writable"),
                "{error}"
            );
        }
        // In a tree that is writable as a whole, the denied private shows its allowed paths.
        let mapping = request(
            &[at(&tree, "/tree", false), at(&private, "/private", false)],
            &[],
        )
        .unwrap()
        .unwrap();
        let child = &mapping.children[0];
        assert!(child.writable && child.write.is_empty());
        assert_eq!(child.denied, [canonical(&private)]);
        assert_eq!(child.allowed, [canonical(&private).join("public")]);
    }

    #[test]
    fn denied_mapped_directories_inside_denied_ones_show_their_allowed_paths() {
        let directory = root();
        let base = directory.path();
        fs::create_dir_all(base.join("tree/private/public")).unwrap();
        fs::create_dir_all(base.join("tree/other")).unwrap();
        let (tree, private) = (base.join("tree"), base.join("tree/private"));
        let denied = [tree.clone(), private.clone()];
        let request = |mappings: &[Mapping<'_>], allowed: &[PathBuf]| {
            super::plan(&ExportRequest {
                denied: PathList {
                    name: "denied",
                    paths: &denied,
                },
                allowed: PathList {
                    name: "allowed",
                    paths: allowed,
                },
                ..ExportRequest::new(mappings, ReservedTargets::CONTAINER_ROOT)
            })
        };
        let both = [at(&tree, "/tree", false), at(&private, "/private", false)];
        // Each mapped directory is denied itself and shows the allowed path inside it; OpenVMM
        // needs only the outer denial, which hides the inner one already.
        let mapping = request(&both, &[private.join("public")]).unwrap().unwrap();
        let child = &mapping.children[0];
        assert_eq!(child.denied, [canonical(&tree)]);
        assert_eq!(child.allowed, [canonical(&private).join("public")]);
        mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();
        // OpenVMM receives each exported directory's policy alone, so one that a denied directory
        // outside it contains keeps its own denial.
        let mapping = request(
            &[at(&private, "/private", false)],
            &[private.join("public")],
        )
        .unwrap()
        .unwrap();
        let child = &mapping.children[0];
        assert_eq!(child.root, canonical(&private));
        assert_eq!(child.denied, [canonical(&private)]);
        assert_eq!(child.allowed, [canonical(&private).join("public")]);
        mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();
        // A mapped directory whose denial shows nothing again inside it stays hidden.
        let error = request(&both, &[tree.join("other")]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error.message().contains("contains the mapped path"),
            "{error}"
        );
    }

    #[test]
    fn recorded_plans_hold_no_more_policy_than_one_aggregate_accepts() {
        // Each child's paths of each kind stay within OpenVMM's limits for one directory.
        let child = |index: usize| {
            let root = std::env::temp_dir().join(format!("nvx-policy-{index}"));
            let long = |kind: &str, path: usize| format!("{kind}-{path}-{}", "x".repeat(3990));
            let paths = |kind: &str| -> Vec<PathBuf> {
                (0..4).map(|path| root.join(long(kind, path))).collect()
            };
            // OpenVMM shows paths again only inside a path that it hides.
            let hidden = root.join(long("denied", 0));
            ExportChild {
                root: root.clone(),
                file: false,
                writable: true,
                denied: paths("denied"),
                denied_identities: vec![None; 4],
                allowed: (0..4)
                    .map(|path| hidden.join(format!("allowed-{path}")))
                    .collect(),
                write: paths("write"),
            }
        };
        let recorded = |count: usize| ExportPlan {
            children: (0..count).map(child).collect(),
            binds: (0..count)
                .map(|index| Bind {
                    child: index,
                    source: String::new(),
                    target: format!("/c{index}"),
                    read_only: false,
                    identity: None,
                })
                .collect(),
            owner: Owner::Vmm,
        };
        recorded(2).check(ReservedTargets::CONTAINER_ROOT).unwrap();
        let error = recorded(3)
            .check(ReservedTargets::CONTAINER_ROOT)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error.message().contains("exceed its 131072-byte limit"),
            "{error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mapped_paths_resolve_to_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let directory = root();
        let base = directory.path();
        let share = base.join(OsStr::from_bytes(b"share-\xff"));
        fs::create_dir(&share).unwrap();
        // A link of a valid name resolves to the same path, which OpenVMM's options could not
        // name.
        let link = base.join("link");
        std::os::unix::fs::symlink(&share, &link).unwrap();
        for path in [&share, &link] {
            let mappings = [at(path, "/workspace", false)];
            let error = super::plan(&ExportRequest::new(
                &mappings,
                ReservedTargets::CONTAINER_ROOT,
            ))
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(
                error
                    .message()
                    .contains("resolves to a path that is not valid UTF-8"),
                "{error}"
            );
        }
    }
}
