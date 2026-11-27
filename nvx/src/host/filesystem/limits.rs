//! OpenVMM's limits and host checks for exported roots and access-policy paths.

#[cfg(feature = "cli")]
use std::fs;
#[cfg(feature = "cli")]
use std::path::Component;
use std::path::{Path, PathBuf};

#[cfg(all(unix, feature = "cli"))]
use super::host_path::identity;
#[cfg(feature = "cli")]
use super::host_path::is_reparse_point;
use super::host_path::{display_path, relative_text};
#[cfg(feature = "cli")]
use super::unsupported_inspection;
use super::{MAX_AGGREGATE_ROOT_BYTES, MAX_POLICY_BYTES, MAX_POLICY_PATH_BYTES, MAX_POLICY_PATHS};
#[cfg(feature = "cli")]
use crate::error::{Error, Result};

/// Applies OpenVMM's rules for exported directories, which it reaches without links.
#[cfg(feature = "cli")]
pub(super) fn check_root(root: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in root.components() {
        current.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| unsupported_inspection(&current, error))?;
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

/// Explains why OpenVMM cannot export the file `file` alone, as a child, or returns `None` if it
/// can. OpenVMM reaches the file by its name, in which it refuses a colon or a backslash, as in
/// every path of an export, such as a Windows path that names an alternate data stream of a file.
pub(super) fn file_name_problem(file: &Path) -> Option<&'static str> {
    file.file_name()
        .is_some_and(|name| name.to_string_lossy().contains([':', '\\']))
        .then_some("has a colon or a backslash in its name")
}

/// Explains why OpenVMM cannot reach `source`, the names of a mapped path below the directory that
/// a child exports, joined with `/`, or returns `None` if it can. OpenVMM looks up each of those
/// names, and refuses a colon or a backslash in them, as [`file_name_problem`] describes; it opens
/// the directory itself by its host path, so the directory's own path may hold either.
pub(super) fn source_name_problem(source: &str) -> Option<&'static str> {
    source
        .contains([':', '\\'])
        .then_some("has a colon or a backslash in a name below its exported directory")
}

/// Applies OpenVMM's limit on the host paths of an aggregate's children, `roots`: at most
/// [`MAX_AGGREGATE_ROOT_BYTES`] bytes together, as OpenVMM counts them, or why OpenVMM would refuse
/// them.
pub(super) fn root_limits<'a>(
    roots: impl IntoIterator<Item = &'a Path>,
) -> std::result::Result<(), String> {
    // OpenVMM counts each root as it canonicalizes it, which on Windows adds the verbatim prefix.
    let prefix = if cfg!(windows) { r"\\?\".len() } else { 0 };
    let total: usize = roots
        .into_iter()
        .map(|root| prefix + display_path(root).len())
        .sum();
    if total > MAX_AGGREGATE_ROOT_BYTES {
        return Err(format!(
            "the host paths that OpenVMM is to export hold {total} bytes, which exceed its \
             {MAX_AGGREGATE_ROOT_BYTES}-byte limit"
        ));
    }
    Ok(())
}

/// Applies OpenVMM's rules for the paths that it is to `action` in the exported directory `root`,
/// so a policy that OpenVMM would refuse fails at provision rather than at every start: those of
/// [`policy_limits`], and on the host, no link or reparse point below `root` and no other file
/// system. Returns their combined length.
#[cfg(feature = "cli")]
pub(super) fn check_policy_paths(root: &Path, paths: &[PathBuf], action: &str) -> Result<usize> {
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
            let metadata = fs::symlink_metadata(&current)
                .map_err(|error| unsupported_inspection(&current, error))?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return unsupported(format!(
                    "OpenVMM cannot {action} {}, because {} is a link or reparse point",
                    path.display(),
                    current.display()
                ));
            }
        }
        #[cfg(unix)]
        if identity(path)?.device != identity(root)?.device {
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
pub(super) fn policy_limits(
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
        let length = relative.len();
        if length > MAX_POLICY_PATH_BYTES {
            return Err(format!(
                "OpenVMM cannot {action} {}: below the exported directory {}, a path must hold \
                 at most {MAX_POLICY_PATH_BYTES} bytes, but it holds {length}",
                path.display(),
                root.display()
            ));
        }
        // A name may contain spaces, but not begin or end with one.
        if relative.chars().any(|character| {
            (character.is_whitespace() && character != ' ') || matches!(character, ':' | '\\')
        }) || relative
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
        total += length;
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
