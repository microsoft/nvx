//! Translation of host paths and aggregate sources into guest paths.

use std::path::{Component, Path, Prefix};

use super::host_path::canonicalize;

/// Joins an aggregate child index and child-relative source into a guest export path.
pub(super) fn guest_source(child: usize, source: &str) -> String {
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
pub(super) fn depth(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}
