//! Host directories exposed to the guest through OpenVMM's single virtio-fs export.
//!
//! OpenVMM offers one virtio-fs export and can hide existing paths inside it. To expose several
//! host paths, the backend exports their deepest common directory to a guest directory that only
//! the guest's root can enter, and the guest agent bind-mounts each mapped path at its target,
//! read-only or read-write. Denied paths inside the export are hidden by OpenVMM itself.
//!
//! Guest targets follow MXC's convention for Linux guests: a Windows path `C:\work\src` appears at
//! `/mnt/c/work/src`, and a Linux path appears at the same path.

use std::fs;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};

use super::platform;
use crate::error::{Error, Result};
use crate::model::FilesystemPolicy;

/// Guest directory where the guest's init mounts the export; only the guest's root can enter
/// its parent.
pub(crate) const GUEST_EXPORT: &str = "/run/nvx/hostfs/root";

/// Largest number of denied paths, and their largest combined length, that OpenVMM accepts.
const MAX_DENIED_PATHS: usize = 128;
const MAX_DENIED_BYTES: usize = 16 * 1024;

/// Guest directories that a mapping must not cover, because the guest's own system lives there.
const RESERVED_TARGETS: [&str; 13] = [
    "/bin", "/boot", "/dev", "/etc", "/lib", "/proc", "/root", "/run", "/sbin", "/sys", "/usr",
    "/var", "/init",
];

/// Exported host directory, hidden paths, and per-path bind mounts of one sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HostMapping {
    /// Canonical host directory exported through virtio-fs.
    pub(crate) root: PathBuf,
    /// Whether the export is writable, which any read-write mapping requires.
    pub(crate) writable: bool,
    /// Canonical host paths inside the export that OpenVMM hides.
    pub(crate) denied: Vec<PathBuf>,
    /// Identities, at provision, of the denied paths that a workload could move: those inside
    /// a read-write mapping.
    #[serde(default)]
    pub(crate) denied_identities: Vec<Option<FileIdentity>>,
    /// Bind mounts the guest agent creates, parents before children.
    pub(crate) binds: Vec<Bind>,
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
    /// Path relative to the export root, `/`-separated; empty for the root itself.
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
/// that is both read-only and read-write is mapped read-only. A denied path must not contain a
/// mapped path, and a denied path that does not exist yet must not lie inside a mapped path,
/// because nothing could hide it once a workload creates it.
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

    let root = common_directory(&mapped).ok_or_else(|| {
        Error::policy_validation(
            "the openvmm backend exports one host directory, and the mapped paths share none; \
             place them on one volume",
        )
    })?;
    if !root
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return unsupported(format!(
            "the mapped paths share only {}, and exporting a whole volume is refused; place \
             them under one directory",
            root.display()
        ));
    }
    let root_text = display_path(&root);
    if root_text.contains(',') {
        return unsupported(format!(
            "the common directory {root_text} of the mapped paths contains a comma, which \
             OpenVMM does not accept"
        ));
    }

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
        if !canonical.starts_with(&root) {
            continue;
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
    check_denied_paths(&root, &denied)?;
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
    let denied_identities = denied
        .iter()
        .map(|path| movable(path).then(|| identity(path)).transpose())
        .collect::<Result<Vec<_>>>()?;

    let mut binds = Vec::with_capacity(mapped.len());
    for entry in &mapped {
        binds.push(Bind {
            source: relative_text(&root, &entry.canonical).ok_or_else(|| {
                Error::policy_validation(format!("{} is not valid UTF-8", entry.given.display()))
            })?,
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
    Ok(Some(HostMapping {
        root,
        writable: mapped.iter().any(|entry| !entry.read_only),
        denied,
        denied_identities,
        binds,
    }))
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
        let path = if bind.source.is_empty() {
            mapping.root.clone()
        } else {
            mapping.root.join(&bind.source)
        };
        match platform::file_identity(&path) {
            Ok((device, index)) if (FileIdentity { device, index }) == expected => {}
            _ => return changed(&path),
        }
    }
    for (path, expected) in mapping.denied.iter().zip(&mapping.denied_identities) {
        let Some(expected) = expected else {
            continue;
        };
        match platform::file_identity(path) {
            Ok((device, index)) if (FileIdentity { device, index }) == *expected => {}
            _ => return changed(path),
        }
    }
    Ok(())
}

/// Applies OpenVMM's rules for hidden paths, so a policy that OpenVMM would refuse fails at
/// provision rather than at every start.
fn check_denied_paths(root: &Path, denied: &[PathBuf]) -> Result<()> {
    let unsupported = |message: String| Err(Error::policy_validation(message));
    if denied.len() > MAX_DENIED_PATHS {
        return unsupported(format!(
            "OpenVMM hides at most {MAX_DENIED_PATHS} paths inside the mapped directories"
        ));
    }
    let mut total = 0;
    for path in denied {
        let Some(relative) = relative_text(root, path) else {
            return unsupported(format!("{} is not valid UTF-8", path.display()));
        };
        if relative
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, ':' | '\\'))
        {
            return unsupported(format!(
                "OpenVMM cannot hide {}: hidden paths must not contain whitespace, colons, or \
                 backslashes below the mapped directories",
                path.display()
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
                    "OpenVMM cannot hide {} through the link {}",
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
                "OpenVMM cannot hide {}, which lies on another file system than {}",
                path.display(),
                root.display()
            ));
        }
    }
    if total > MAX_DENIED_BYTES {
        return unsupported(format!(
            "the hidden paths exceed OpenVMM's {MAX_DENIED_BYTES}-byte limit"
        ));
    }
    Ok(())
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

/// Kernel command-line tokens that tell the guest agent which bind mounts to create.
pub(crate) fn kernel_tokens(mapping: &HostMapping) -> Vec<String> {
    mapping
        .binds
        .iter()
        .map(|bind| {
            let source = if bind.source.is_empty() {
                ".".to_owned()
            } else {
                encode(&bind.source)
            };
            let mode = if bind.read_only { "ro" } else { "rw" };
            format!("nvx_map={source},{},{mode}", encode(&bind.target))
        })
        .collect()
}

/// OpenVMM options that export the mapping's root and hide its denied paths.
pub(crate) fn openvmm_arguments(mapping: &HostMapping) -> Vec<String> {
    let mode = if mapping.writable { "rw" } else { "ro" };
    let mut arguments = vec![
        "--mount".to_owned(),
        format!("{GUEST_EXPORT},{},{mode}", display_path(&mapping.root)),
    ];
    for path in &mapping.denied {
        arguments.push("--mount-deny".to_owned());
        arguments.push(display_path(path));
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

/// Percent-encodes everything but unreserved characters and `/`, so a value is one kernel
/// command-line token without commas.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'~' | b'-') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Deepest directory that contains every mapped path.
fn common_directory(mapped: &[Mapped]) -> Option<PathBuf> {
    let mut common: Option<PathBuf> = None;
    for entry in mapped {
        let directory = if entry.directory {
            entry.canonical.clone()
        } else {
            entry.canonical.parent()?.to_path_buf()
        };
        common = Some(match common {
            None => directory,
            Some(common) => {
                let shared: PathBuf = common
                    .components()
                    .zip(directory.components())
                    .take_while(|(left, right)| left == right)
                    .map(|(component, _)| component)
                    .collect();
                if !shared.has_root() {
                    return None;
                }
                shared
            }
        });
    }
    common
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
        for name in ["work/src", "work/out", "work/src/secret", "tools"] {
            fs::create_dir_all(directory.path().join(name)).unwrap();
        }
        fs::write(directory.path().join("work/config.json"), b"{}").unwrap();
        directory
    }

    fn canonical(path: &Path) -> PathBuf {
        canonicalize(path).unwrap()
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

    #[test]
    fn multiple_paths_share_one_export() {
        let directory = root();
        let base = directory.path();
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("work/src"), base.join("work/config.json")],
            readwrite_paths: vec![base.join("work/out")],
            denied_paths: vec![base.join("work/src/secret"), base.join("elsewhere")],
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert_eq!(mapping.root, canonical(&base.join("work")));
        assert!(mapping.writable);
        assert_eq!(mapping.denied, [canonical(&base.join("work/src/secret"))]);
        let sources: Vec<(&str, bool)> = mapping
            .binds
            .iter()
            .map(|bind| (bind.source.as_str(), bind.read_only))
            .collect();
        assert_eq!(
            sources,
            [("config.json", true), ("out", false), ("src", true)]
        );
        let target = guest_path(&canonical(&base.join("work/out"))).unwrap();
        assert!(
            mapping.binds.iter().any(|bind| bind.target == target),
            "missing canonical guest target {target:?} in {:?}",
            mapping.binds
        );

        let arguments = openvmm_arguments(&mapping);
        assert_eq!(arguments[0], "--mount");
        assert!(arguments[1].starts_with(&format!("{GUEST_EXPORT},")));
        assert!(arguments[1].ends_with(",rw"));
        assert_eq!(arguments[2], "--mount-deny");
        let tokens = kernel_tokens(&mapping);
        assert_eq!(tokens.len(), 3);
        assert!(tokens.iter().all(|token| token.starts_with("nvx_map=")));
        assert!(tokens[0].ends_with(",ro"));
    }

    #[test]
    fn a_single_directory_is_exported_itself_and_parents_mount_first() {
        let directory = root();
        let base = directory.path();
        let policy = FilesystemPolicy {
            readonly_paths: vec![base.join("work/src")],
            readwrite_paths: vec![base.join("work"), base.join("work/src")],
            ..FilesystemPolicy::default()
        };
        let mapping = plan(&policy).unwrap().unwrap();
        assert_eq!(mapping.root, canonical(&base.join("work")));
        let binds: Vec<(&str, bool)> = mapping
            .binds
            .iter()
            .map(|bind| (bind.source.as_str(), bind.read_only))
            .collect();
        // The read-write duplicate of the read-only path is tightened to read-only.
        assert_eq!(binds, [("", false), ("src", true)]);
        assert_eq!(
            kernel_tokens(&mapping)[0],
            format!("nvx_map=.,{},rw", encode(&mapping.binds[0].target))
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
    fn denied_paths_follow_openvmm_rules() {
        let directory = root();
        let base = directory.path();
        fs::create_dir_all(base.join("work/out/My Secrets")).unwrap();
        let spaced = FilesystemPolicy {
            readwrite_paths: vec![base.join("work/out")],
            denied_paths: vec![base.join("work/out/My Secrets")],
            ..FilesystemPolicy::default()
        };
        assert_eq!(
            plan(&spaced).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
        if cfg!(windows) {
            let volume = FilesystemPolicy {
                readonly_paths: vec![base.join("work"), PathBuf::from(r"C:\Windows")],
                ..FilesystemPolicy::default()
            };
            if canonical(base).starts_with(r"C:\") {
                assert_eq!(
                    plan(&volume).unwrap_err().code(),
                    ErrorCode::PolicyValidation
                );
            }
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
        assert!(mapping.denied_identities.iter().all(Option::is_some));
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
        fs::remove_dir(base.join("tools")).unwrap();
        fs::create_dir(base.join("tools")).unwrap();
        verify(&mapping).unwrap();
    }

    #[test]
    fn encoding_keeps_tokens_free_of_separators() {
        assert_eq!(encode("/mnt/c/My Work,1%"), "/mnt/c/My%20Work%2C1%25");
        assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");
    }
}
