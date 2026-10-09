//! Host paths exposed to the guest through OpenVMM's aggregate virtio-fs export.
//!
//! OpenVMM offers one virtio-fs device. The backend attaches it as an aggregate: a synthetic,
//! read-only root that lists one child per exported host directory, named by its index, which
//! the guest's init mounts at a directory that only the guest's root can enter. The guest agent
//! then bind-mounts each mapped path at its target, read-only or read-write, from the mapping
//! table that it receives over the control channel.
//!
//! The children are the outermost mapped directories and the parents of the outermost mapped
//! files, so mapped paths may lie anywhere, on any volume, except that neither a whole volume nor
//! a file directly in a volume's root, whose parent would export that root, can be mapped. A
//! child whose root is not mapped itself, the parent of mapped files, hides everything but its
//! mapped paths. A child is read-write only if it holds a read-write mapping, and OpenVMM then
//! limits writes to its read-write mappings, so the host enforces each mapping's access, except
//! that of a read-only path inside a read-write mapping, which only the guest's bind mount keeps
//! read-only. OpenVMM also hides the denied paths.
//!
//! Guest targets follow MXC's convention for Linux guests: a Windows path `C:\work\src` appears at
//! `/mnt/c/work/src`, and a Linux path appears at the same path.

use std::fs;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};

use super::platform;
use super::protocol::{HostMap, MAX_HOST_MAPPINGS};
use crate::error::{Error, Result};
use crate::model::FilesystemPolicy;

/// Guest directory where the guest's init mounts the export; only the guest's root can enter
/// its parent.
pub(crate) const GUEST_EXPORT: &str = "/run/nvx/hostfs/root";

/// Largest number of host directories that OpenVMM exports through one aggregate.
const MAX_CHILDREN: usize = 256;
/// Largest number of hidden, allowed, or writable paths that OpenVMM accepts in one exported
/// directory, and their largest combined length.
const MAX_POLICY_PATHS: usize = 128;
const MAX_POLICY_BYTES: usize = 16 * 1024;
/// Largest length of one such path, and of all of them together.
const MAX_POLICY_PATH_BYTES: usize = 4096;
const MAX_AGGREGATE_POLICY_BYTES: usize = 128 * 1024;
/// Longest guest path, without the terminating NUL.
const MAX_GUEST_PATH: usize = 4095;

/// Guest directories that a mapping must not cover, because the guest's own system lives there.
const RESERVED_TARGETS: [&str; 13] = [
    "/bin", "/boot", "/dev", "/etc", "/lib", "/proc", "/root", "/run", "/sbin", "/sys", "/usr",
    "/var", "/init",
];

/// Exported host directories and per-path bind mounts of one sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HostMapping {
    /// Children of the aggregate export; each is named by its index.
    pub(crate) children: Vec<Child>,
    /// Bind mounts the guest agent creates, parents before children.
    pub(crate) binds: Vec<Bind>,
}

/// One host directory of the aggregate export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Child {
    /// Canonical host directory.
    pub(crate) root: PathBuf,
    /// Whether the guest may modify it, which a read-write mapping inside it requires.
    pub(crate) writable: bool,
    /// Whether OpenVMM hides everything but the allowed paths, because the root is no mapping.
    pub(crate) hidden: bool,
    /// Canonical host paths inside the child that OpenVMM hides.
    pub(crate) denied: Vec<PathBuf>,
    /// Identities, at provision, of the denied paths that a workload could move: those inside
    /// a read-write mapping.
    pub(crate) denied_identities: Vec<Option<FileIdentity>>,
    /// Canonical mapped paths that stay visible below a hidden root.
    pub(crate) allowed: Vec<PathBuf>,
    /// Canonical paths to which OpenVMM limits the guest's writes; empty if the guest may modify
    /// everything that it can reach in a writable child.
    pub(crate) write: Vec<PathBuf>,
}

/// Device and file index of a host object, which survive renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FileIdentity {
    pub(crate) device: u64,
    pub(crate) index: u64,
}

/// One mapped host path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Bind {
    /// Index of the child that holds the mapped path.
    pub(crate) child: usize,
    /// Path relative to the child's root, `/`-separated; empty for the root itself.
    pub(crate) source: String,
    /// Absolute guest path.
    pub(crate) target: String,
    /// Whether the guest mounts it read-only.
    pub(crate) read_only: bool,
    /// Identity of the host object at provision, when it lies inside a read-write mapping and a
    /// workload could therefore move it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) identity: Option<FileIdentity>,
}

impl Bind {
    /// Path of the mapped object relative to the guest's mount of the export.
    pub(crate) fn guest_source(&self) -> String {
        guest_source(self.child, &self.source)
    }
}

fn guest_source(child: usize, source: &str) -> String {
    if source.is_empty() {
        child.to_string()
    } else {
        format!("{child}/{source}")
    }
}

/// Returns the guest path at which the backend exposes the host path `path`, or `None` if it
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

/// Plans the mapping of `policy`, or returns `None` if it maps nothing.
///
/// Mapped paths must exist, and their guest paths derive from their resolved host paths, so two
/// spellings of one object (links, letter case, or short names) map to one guest path. A path
/// that is both read-only and read-write is mapped read-only. Neither a whole volume nor a file
/// directly in a volume's root can be mapped. A denied path must not contain a mapped path, and
/// a denied path that does not exist yet must not lie inside a mapped path, because nothing
/// could hide it once a workload creates it.
pub(crate) fn plan(policy: &FilesystemPolicy) -> Result<Option<HostMapping>> {
    let unsupported = |message: String| Err(Error::policy_validation(message));
    let mut mapped: Vec<Mapped> = Vec::new();
    for (paths, read_only, field) in [
        (&policy.readonly_paths, true, "filesystem.readonlyPaths"),
        (&policy.readwrite_paths, false, "filesystem.readwritePaths"),
    ] {
        for path in paths {
            let canonical = canonicalize(path).map_err(|error| {
                Error::policy_validation(format!(
                    "{field} entry {} is not accessible",
                    path.display()
                ))
                .with_source(error)
            })?;
            let metadata = fs::metadata(&canonical).map_err(|error| {
                Error::policy_validation(format!("cannot inspect {}", path.display()))
                    .with_source(error)
            })?;
            if !metadata.is_dir() && !metadata.is_file() {
                return unsupported(format!(
                    "{field} entry {} is neither a directory nor a regular file",
                    path.display()
                ));
            }
            if let Some(reason) = volume_root_refusal(&canonical, metadata.is_dir()) {
                return unsupported(format!("{field} entry {} {reason}", path.display()));
            }
            let Some(target) = guest_path(&canonical) else {
                return unsupported(format!(
                    "{field} entry {} has no guest path; use an absolute local path",
                    path.display()
                ));
            };
            if let Some(reserved) = RESERVED_TARGETS
                .iter()
                .find(|reserved| target == "/" || within(&target, reserved))
            {
                return unsupported(format!(
                    "{field} entry {} would cover the guest's {reserved}",
                    path.display()
                ));
            }
            match mapped.iter_mut().find(|other| other.canonical == canonical) {
                // The same object listed twice keeps its most restrictive access.
                Some(other) => other.read_only |= read_only,
                None => mapped.push(Mapped {
                    given: path.clone(),
                    canonical,
                    target,
                    read_only,
                    directory: metadata.is_dir(),
                }),
            }
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
    // A Windows host opens files case-insensitively, while the guest lets a file have several
    // names, so a read-only file inside a read-write directory stays writable under another
    // spelling of its name.
    if cfg!(windows)
        && let Some((file, directory)) = mapped.iter().find_map(|file| {
            (file.read_only && !file.directory)
                .then(|| {
                    mapped.iter().find(|directory| {
                        directory.directory
                            && !directory.read_only
                            && file.canonical.starts_with(&directory.canonical)
                    })
                })
                .flatten()
                .map(|directory| (file, directory))
        })
    {
        return unsupported(format!(
            "the read-only file {} lies inside the read-write directory {}, which cannot keep \
             it read-only on a Windows host",
            file.given.display(),
            directory.given.display()
        ));
    }

    let roots = export_roots(&mapped)?;
    if roots.len() > MAX_CHILDREN {
        return unsupported(format!(
            "the mapped paths lie in {} separate host directories, but OpenVMM exports at most \
             {MAX_CHILDREN}",
            roots.len()
        ));
    }
    let denied = denied_paths(policy, &mapped)?;
    // Only objects inside a read-write mapping can be swapped by a workload; pinning others
    // would make ordinary host edits, which often replace files, block the next start.
    let movable = |path: &Path| {
        mapped.iter().any(|entry| {
            entry.directory
                && !entry.read_only
                && path != entry.canonical
                && path.starts_with(&entry.canonical)
        })
    };

    let mut children = Vec::with_capacity(roots.len());
    let mut policy_bytes = 0;
    for root in roots {
        let inside: Vec<&Mapped> = mapped
            .iter()
            .filter(|entry| entry.canonical.starts_with(&root))
            .collect();
        let hidden = !inside.iter().any(|entry| entry.canonical == root);
        let visible = outermost(&inside);
        let read_write: Vec<&Mapped> = inside
            .iter()
            .copied()
            .filter(|entry| !entry.read_only)
            .collect();
        let writable = !read_write.is_empty();
        // Unless the read-write mappings cover everything that the guest reaches in the child,
        // OpenVMM limits writes to them.
        let mut write: Vec<PathBuf> = if writable && visible.iter().any(|entry| entry.read_only) {
            outermost(&read_write)
                .iter()
                .map(|entry| entry.canonical.clone())
                .collect()
        } else {
            Vec::new()
        };
        write.sort();
        let mut allowed: Vec<PathBuf> = if hidden {
            visible
                .iter()
                .map(|entry| entry.canonical.clone())
                .collect()
        } else {
            Vec::new()
        };
        allowed.sort();
        // Below a hidden root, only paths inside an allowed path need hiding.
        let child_denied: Vec<PathBuf> = denied
            .iter()
            .filter(|path| {
                path.starts_with(&root)
                    && (!hidden || allowed.iter().any(|allowed| path.starts_with(allowed)))
            })
            .cloned()
            .collect();
        check_root(&root)?;
        policy_bytes += check_policy_paths(&root, &child_denied, "hide", hidden)?;
        policy_bytes += check_policy_paths(&root, &allowed, "expose", false)?;
        policy_bytes += check_policy_paths(&root, &write, "make writable", false)?;
        let denied_identities = child_denied
            .iter()
            .map(|path| movable(path).then(|| identity(path)).transpose())
            .collect::<Result<Vec<_>>>()?;
        children.push(Child {
            root,
            writable,
            hidden,
            denied: child_denied,
            denied_identities,
            allowed,
            write,
        });
    }
    if policy_bytes > MAX_AGGREGATE_POLICY_BYTES {
        return unsupported(format!(
            "the paths that OpenVMM is to hide, expose, or make writable exceed its \
             {MAX_AGGREGATE_POLICY_BYTES}-byte limit"
        ));
    }

    let mut binds = Vec::with_capacity(mapped.len());
    for entry in &mapped {
        let Some(child) = children
            .iter()
            .position(|child| entry.canonical.starts_with(&child.root))
        else {
            return Err(Error::backend_error(format!(
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
    Ok(Some(HostMapping { children, binds }))
}

/// Explains why a mapped host path, given in canonical form, cannot be exported, or returns
/// `None` if it can: a whole volume, or a file directly in a volume's root, which would export
/// that root as the file's parent. It inspects only the path, not the host.
fn volume_root_refusal(canonical: &Path, directory: bool) -> Option<&'static str> {
    let is_volume_root = |path: &Path| {
        !path
            .components()
            .any(|component| matches!(component, Component::Normal(_)))
    };
    if directory {
        is_volume_root(canonical).then_some(
            "is a whole volume, which the openvmm backend does not map; map the directories \
             inside it",
        )
    } else {
        canonical.parent().is_none_or(is_volume_root).then_some(
            "lies directly in a volume's root, which the openvmm backend does not export; move \
             the file into a directory",
        )
    }
}

/// Returns the directories to export: the outermost mapped directories and the parents of the
/// outermost mapped files, in order. Distinct paths must not name one directory.
fn export_roots(mapped: &[Mapped]) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::with_capacity(mapped.len());
    for entry in mapped {
        if entry.directory {
            candidates.push(entry.canonical.clone());
        } else {
            let Some(parent) = entry.canonical.parent() else {
                return Err(Error::policy_validation(format!(
                    "{} has no parent directory",
                    entry.given.display()
                )));
            };
            candidates.push(parent.to_path_buf());
        }
    }
    candidates.sort();
    candidates.dedup();
    let roots: Vec<PathBuf> = candidates
        .iter()
        .filter(|candidate| {
            !candidates
                .iter()
                .any(|other| other != *candidate && candidate.starts_with(other))
        })
        .cloned()
        .collect();
    // OpenVMM refuses one directory exported twice, which bind mounts make possible.
    let mut identities: Vec<(FileIdentity, &Path)> = Vec::with_capacity(roots.len());
    for root in &roots {
        let current = identity(root)?;
        if let Some((_, other)) = identities.iter().find(|(other, _)| *other == current) {
            return Err(Error::policy_validation(format!(
                "{} and {} are the same host directory; map it once",
                other.display(),
                root.display()
            )));
        }
        identities.push((current, root));
    }
    Ok(roots)
}

/// Resolves the denied paths that OpenVMM needs to hide: those that exist, without the ones
/// that other denied paths contain.
fn denied_paths(policy: &FilesystemPolicy, mapped: &[Mapped]) -> Result<Vec<PathBuf>> {
    let unsupported = |message: String| Err(Error::policy_validation(message));
    let mut denied: Vec<PathBuf> = Vec::new();
    for path in &policy.denied_paths {
        let (canonical, exists) = canonicalize_lenient(path).map_err(|error| {
            Error::policy_validation(format!(
                "filesystem.deniedPaths entry {} cannot be resolved",
                path.display()
            ))
            .with_source(error)
        })?;
        if let Some(entry) = mapped
            .iter()
            .find(|entry| entry.canonical.starts_with(&canonical))
        {
            return unsupported(format!(
                "filesystem.deniedPaths entry {} contains the mapped path {}",
                path.display(),
                entry.given.display()
            ));
        }
        if !exists {
            if let Some(entry) = mapped
                .iter()
                .find(|entry| canonical.starts_with(&entry.canonical))
            {
                return unsupported(format!(
                    "filesystem.deniedPaths entry {} does not exist, so it cannot be hidden \
                     inside the mapped path {}",
                    path.display(),
                    entry.given.display()
                ));
            }
            continue;
        }
        denied.push(canonical);
    }
    denied.sort();
    denied.dedup();
    let nested: Vec<PathBuf> = denied
        .iter()
        .filter(|path| {
            denied
                .iter()
                .any(|other| other != *path && path.starts_with(other))
        })
        .cloned()
        .collect();
    denied.retain(|path| !nested.contains(path));
    Ok(denied)
}

/// Returns the entries that no other entry contains.
fn outermost<'a>(entries: &[&'a Mapped]) -> Vec<&'a Mapped> {
    entries
        .iter()
        .copied()
        .filter(|entry| {
            !entries.iter().any(|other| {
                other.canonical != entry.canonical && entry.canonical.starts_with(&other.canonical)
            })
        })
        .collect()
}

/// Checks that every mapped and denied path inside a read-write mapping still names the object
/// it named at provision.
///
/// A workload with a read-write mapping could otherwise rename a denied or read-only object and
/// put a decoy at its path, and the next start would protect the decoy instead. The VM is
/// stopped while this runs, so no workload can race with it.
pub(crate) fn verify(mapping: &HostMapping) -> Result<()> {
    let changed = |path: &Path| {
        Err(Error::backend_error(format!(
            "{} no longer names the host object it named at provision; deprovision the sandbox \
             and provision it again",
            path.display()
        )))
    };
    for bind in &mapping.binds {
        let Some(expected) = bind.identity else {
            continue;
        };
        let Some(child) = mapping.children.get(bind.child) else {
            return Err(Error::backend_error(
                "the sandbox state maps a path of an unknown host directory",
            ));
        };
        let path = if bind.source.is_empty() {
            child.root.clone()
        } else {
            child.root.join(&bind.source)
        };
        match platform::file_identity(&path) {
            Ok((device, index)) if (FileIdentity { device, index }) == expected => {}
            _ => return changed(&path),
        }
    }
    for child in &mapping.children {
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
    Ok(())
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
            Error::policy_validation(format!("cannot inspect {}", current.display()))
                .with_source(error)
        })?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(Error::policy_validation(format!(
                "OpenVMM cannot export {}, because {} is a link or reparse point",
                root.display(),
                current.display()
            )));
        }
    }
    Ok(())
}

/// Applies OpenVMM's rules for the paths that it is to `action` in the exported directory
/// `root`, so a policy that OpenVMM would refuse fails at provision rather than at every start.
/// `with_root` counts the root itself among them. Returns their combined length.
fn check_policy_paths(
    root: &Path,
    paths: &[PathBuf],
    action: &str,
    with_root: bool,
) -> Result<usize> {
    let unsupported = |message: String| Err(Error::policy_validation(message));
    let count = paths.len() + usize::from(with_root);
    if count > MAX_POLICY_PATHS {
        return unsupported(format!(
            "OpenVMM can {action} at most {MAX_POLICY_PATHS} paths in one exported directory, \
             but {} needs {count}",
            root.display()
        ));
    }
    let mut total = 0;
    for path in paths {
        let Some(relative) = relative_text(root, path) else {
            return unsupported(format!("{} is not valid UTF-8", path.display()));
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
            return unsupported(format!(
                "OpenVMM cannot {action} {}: below the exported directory {}, a path must not \
                 contain colons, backslashes, or whitespace other than spaces, and no name may \
                 begin or end with a space",
                path.display(),
                root.display()
            ));
        }
        total += relative.len();
        let mut current = root.to_path_buf();
        for component in relative.split('/') {
            current.push(component);
            let metadata = fs::symlink_metadata(&current).map_err(|error| {
                Error::policy_validation(format!("cannot inspect {}", current.display()))
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
    if total > MAX_POLICY_BYTES {
        return unsupported(format!(
            "the paths that OpenVMM is to {action} in {} exceed its {MAX_POLICY_BYTES}-byte \
             limit",
            root.display()
        ));
    }
    Ok(total)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn identity(path: &Path) -> Result<FileIdentity> {
    platform::file_identity(path)
        .map(|(device, index)| FileIdentity { device, index })
        .map_err(|error| {
            Error::policy_validation(format!("cannot identify {}", path.display()))
                .with_source(error)
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

/// Kernel command-line token that tells the guest agent how many bind mounts to expect over
/// the control channel.
pub(crate) fn kernel_token(mapping: &HostMapping) -> String {
    format!("nvx_maps={}", mapping.binds.len())
}

/// The bind mounts in the order and form in which the guest agent receives them.
pub(crate) fn guest_table(mapping: &HostMapping) -> Vec<HostMap> {
    mapping
        .binds
        .iter()
        .map(|bind| HostMap {
            source: bind.guest_source(),
            target: bind.target.clone(),
            read_only: bind.read_only,
        })
        .collect()
}

/// OpenVMM options that export the mapping's directories with their access policies.
pub(crate) fn openvmm_arguments(mapping: &HostMapping) -> Vec<String> {
    let mut arguments = vec!["--mount-aggregate".to_owned(), GUEST_EXPORT.to_owned()];
    for (index, child) in mapping.children.iter().enumerate() {
        let mode = if child.writable { "rw" } else { "ro" };
        arguments.push("--mount-child".to_owned());
        arguments.push(format!("{index},{},{mode}", display_path(&child.root)));
    }
    let mut add = |option: &str, path: &Path| {
        arguments.push(option.to_owned());
        arguments.push(display_path(path));
    };
    for child in mapping.children.iter().filter(|child| child.hidden) {
        add("--mount-deny", &child.root);
    }
    for child in &mapping.children {
        for path in &child.denied {
            add("--mount-deny", path);
        }
    }
    for child in &mapping.children {
        for path in &child.allowed {
            add("--mount-allow", path);
        }
    }
    for child in &mapping.children {
        for path in &child.write {
            add("--mount-write", path);
        }
    }
    arguments
}

struct Mapped {
    given: PathBuf,
    canonical: PathBuf,
    target: String,
    read_only: bool,
    directory: bool,
}

fn within(path: &str, directory: &str) -> bool {
    path == directory
        || path
            .strip_prefix(directory)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn depth(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}

/// Resolves links and returns a path without the Windows verbatim prefix.
fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    match strip_verbatim(&canonical) {
        Some(stripped) => Ok(stripped),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "network paths are unsupported",
        )),
    }
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
fn strip_verbatim(path: &Path) -> Option<PathBuf> {
    Some(path.to_path_buf())
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

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

    fn sources(mapping: &HostMapping) -> Vec<(String, bool)> {
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
        let children: Vec<(PathBuf, bool, bool)> = mapping
            .children
            .iter()
            .map(|child| (child.root.clone(), child.writable, child.hidden))
            .collect();
        assert_eq!(
            children,
            [
                (canonical(&base.join("build-output")), true, false),
                (canonical(&base.join("projects/app")), true, false),
                (canonical(&base.join("tools")), false, false),
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

        let arguments = openvmm_arguments(&mapping);
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
        assert_eq!(kernel_token(&mapping), "nvx_maps=3");
        let table = guest_table(&mapping);
        assert_eq!(table.len(), 3);
        assert_eq!(
            table[1].target,
            guest_path(&canonical(&base.join("tools"))).unwrap()
        );
        assert!(table[1].read_only);
    }

    #[test]
    fn mapped_files_are_exposed_through_their_hidden_parents() {
        let directory = root();
        let base = directory.path();
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("work/config.json"), base.join("work/src")],
            readwrite_paths: vec![base.join("work/run.sh"), base.join("work/out")],
            denied_paths: vec![
                // Hidden already: the parent hides everything but its mapped paths.
                base.join("work/private"),
                base.join("work/src/secret"),
            ],
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert_eq!(mapping.children.len(), 1);
        let child = &mapping.children[0];
        let work = canonical(&base.join("work"));
        assert_eq!(child.root, work);
        assert!(child.hidden && child.writable);
        assert_eq!(
            child.allowed,
            ["config.json", "out", "run.sh", "src"].map(|name| work.join(name))
        );
        // The read-only mappings stay read-only on the host.
        assert_eq!(child.write, ["out", "run.sh"].map(|name| work.join(name)));
        assert_eq!(child.denied, [work.join("src").join("secret")]);
        assert_eq!(
            sources(&mapping),
            [
                ("0/config.json".to_owned(), true),
                ("0/out".to_owned(), false),
                ("0/run.sh".to_owned(), false),
                ("0/src".to_owned(), true),
            ]
        );

        let arguments = openvmm_arguments(&mapping);
        assert_eq!(
            option_values(&arguments, "--mount-deny"),
            [
                display_path(&work),
                display_path(&work.join("src").join("secret"))
            ]
        );
        assert_eq!(option_values(&arguments, "--mount-allow").len(), 4);
        assert_eq!(option_values(&arguments, "--mount-write").len(), 2);
    }

    #[test]
    fn nested_mappings_merge_into_the_outermost_directory() {
        let directory = root();
        let base = directory.path();
        fs::create_dir_all(base.join("work/out/cache")).unwrap();
        let policy = FilesystemPolicy {
            readonly_paths: vec![
                base.join("work"),
                base.join("work/src"),
                base.join("work/out/cache"),
            ],
            readwrite_paths: vec![base.join("work/out")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert_eq!(mapping.children.len(), 1);
        let child = &mapping.children[0];
        let work = canonical(&base.join("work"));
        assert!(!child.hidden && child.writable);
        // The read-only root is read-only on the host; the cache inside the read-write mapping
        // is left to the guest's bind mount.
        assert_eq!(child.write, [work.join("out")]);
        assert_eq!(
            sources(&mapping),
            [
                ("0".to_owned(), true),
                ("0/out".to_owned(), false),
                ("0/src".to_owned(), true),
                ("0/out/cache".to_owned(), true),
            ]
        );

        // A read-write root covers everything, so OpenVMM need not limit writes.
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("work/src")],
            readwrite_paths: vec![base.join("work"), base.join("work/src")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert!(mapping.children[0].writable && mapping.children[0].write.is_empty());
        // The read-write duplicate of the read-only path is tightened to read-only.
        assert_eq!(
            sources(&mapping),
            [("0".to_owned(), false), ("0/src".to_owned(), true)]
        );
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
            option_values(&openvmm_arguments(&mapping), "--mount-deny").len(),
            2
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
                plan(&policy).unwrap_err().code(),
                ErrorCode::PolicyValidation,
                "{policy:?}"
            );
        }
        if !cfg!(windows) {
            let system = FilesystemPolicy {
                readonly_paths: vec![PathBuf::from("/usr")],
                ..FilesystemPolicy::default()
            };
            assert_eq!(
                plan(&system).unwrap_err().code(),
                ErrorCode::PolicyValidation
            );
        }
        let empty = FilesystemPolicy {
            denied_paths: vec![base.join("tools")],
            ..FilesystemPolicy::default()
        };
        assert_eq!(plan(&empty).unwrap(), None);
    }

    #[test]
    fn volumes_and_files_directly_in_their_roots_are_refused() {
        // The paths are only inspected, so nothing is created at a volume root.
        let (volumes, directories, files_in_roots, nested_files): (
            &[&str],
            &[&str],
            &[&str],
            &[&str],
        ) = if cfg!(windows) {
            (
                &[r"C:\", r"D:\"],
                &[r"C:\work", r"D:\a\b"],
                &[r"C:\notes.txt", r"D:\build.log"],
                &[r"C:\work\notes.txt", r"D:\a\b\build.log"],
            )
        } else {
            (
                &["/"],
                &["/work", "/srv/a/b"],
                &["/notes.txt", "/build.log"],
                &["/work/notes.txt", "/srv/a/b/build.log"],
            )
        };
        for volume in volumes {
            let reason = volume_root_refusal(Path::new(volume), true).unwrap();
            assert!(reason.contains("whole volume"), "{volume}: {reason}");
        }
        for file in files_in_roots {
            let reason = volume_root_refusal(Path::new(file), false).unwrap();
            assert!(
                reason.contains("move the file into a directory"),
                "{file}: {reason}"
            );
        }
        for directory in directories {
            assert_eq!(volume_root_refusal(Path::new(directory), true), None);
        }
        for file in nested_files {
            assert_eq!(volume_root_refusal(Path::new(file), false), None);
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

    #[test]
    fn policy_paths_follow_openvmm_rules() {
        let directory = root();
        let base = directory.path();
        for name in ["work/out/My Secrets", "work/out/ leading", "my files"] {
            fs::create_dir_all(base.join(name)).unwrap();
        }
        fs::write(base.join("my files/notes one.txt"), b"notes").unwrap();
        // Names may contain spaces.
        let spaced = FilesystemPolicy {
            readonly_paths: vec![base.join("my files/notes one.txt")],
            readwrite_paths: vec![base.join("work/out")],
            denied_paths: vec![base.join("work/out/My Secrets")],
        };
        let mapping = plan(&spaced).unwrap().unwrap();
        let arguments = openvmm_arguments(&mapping);
        assert!(
            option_values(&arguments, "--mount-allow")[0].ends_with("notes one.txt"),
            "{arguments:?}"
        );
        assert!(option_values(&arguments, "--mount-deny")[1].ends_with("My Secrets"));
        // But they must not begin or end with one.
        let leading = FilesystemPolicy {
            readwrite_paths: vec![base.join("work/out")],
            denied_paths: vec![base.join("work/out/ leading")],
            ..FilesystemPolicy::default()
        };
        assert_eq!(
            plan(&leading).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
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
                    plan(&policy).unwrap_err().code(),
                    ErrorCode::PolicyValidation,
                    "{name}"
                );
            }
        }
        if cfg!(windows) {
            let nested_file = FilesystemPolicy {
                readonly_paths: vec![base.join("work/config.json")],
                readwrite_paths: vec![base.join("work")],
                ..FilesystemPolicy::default()
            };
            assert_eq!(
                plan(&nested_file).unwrap_err().code(),
                ErrorCode::PolicyValidation
            );
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
        // A hidden root exposes at most 128 mapped paths.
        let policy = FilesystemPolicy {
            readonly_paths: files(128),
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert_eq!(mapping.children[0].allowed.len(), 128);
        let policy = FilesystemPolicy {
            readonly_paths: files(129),
            ..FilesystemPolicy::default()
        };
        let error = plan(&policy).unwrap_err();
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert!(error.message().contains("at most 128"), "{error}");
    }

    #[test]
    fn swapped_objects_fail_verification() {
        let directory = root();
        let base = directory.path();
        // Everything below the read-write work directory is pinned.
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("work/src")],
            readwrite_paths: vec![base.join("work")],
            denied_paths: vec![base.join("work/src/secret")],
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert!(
            mapping.children[0]
                .denied_identities
                .iter()
                .all(Option::is_some)
        );
        verify(&mapping).unwrap();
        fs::rename(base.join("work/src/secret"), base.join("work/src/moved")).unwrap();
        fs::create_dir(base.join("work/src/secret")).unwrap();
        assert_eq!(
            verify(&mapping).unwrap_err().code(),
            ErrorCode::BackendError
        );
        fs::remove_dir(base.join("work/src/secret")).unwrap();
        fs::rename(base.join("work/src/moved"), base.join("work/src/secret")).unwrap();
        verify(&mapping).unwrap();
        fs::rename(base.join("work/src"), base.join("work/src-moved")).unwrap();
        fs::create_dir_all(base.join("work/src/secret")).unwrap();
        assert_eq!(
            verify(&mapping).unwrap_err().code(),
            ErrorCode::BackendError
        );

        // Without a read-write parent, nothing a workload could move is pinned, so host edits
        // that replace objects do not block the next start.
        let read_only = FilesystemPolicy {
            readonly_paths: vec![base.join("tools")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&read_only).unwrap().unwrap();
        fs::remove_dir_all(base.join("tools")).unwrap();
        fs::create_dir(base.join("tools")).unwrap();
        verify(&mapping).unwrap();
    }

    /// A directory on another volume than the temporary directory, if the host has one: the one
    /// that `ACI_EDGE_SANDBOXES_TEST_SECOND_VOLUME` names, or the runner's temporary directory
    /// on GitHub's Windows runners. Tests create their files below it. Linux's `/dev/shm` does
    /// not qualify, because it lies in the guest's reserved `/dev`.
    fn second_volume() -> Option<PathBuf> {
        let candidate = std::env::var_os("ACI_EDGE_SANDBOXES_TEST_SECOND_VOLUME")
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
}
