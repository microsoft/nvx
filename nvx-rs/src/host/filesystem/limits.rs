//! OpenVMM's limits and host checks for exported roots and access-policy paths.

use std::fs;
use std::path::{Component, Path, PathBuf};

use super::host_path::{is_reparse_point, relative_text};
use super::{MAX_POLICY_BYTES, MAX_POLICY_PATH_BYTES, MAX_POLICY_PATHS};
use crate::error::{Error, Result};
#[cfg(unix)]
use crate::host::platform;

/// Applies OpenVMM's rules for exported directories, which it reaches without links.
pub(super) fn check_root(root: &Path) -> Result<()> {
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
