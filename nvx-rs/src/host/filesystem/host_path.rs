//! Canonical host paths, file identities, and the text of relative host paths.

use std::fs;
use std::path::{Path, PathBuf};

use super::FileIdentity;
use crate::error::{Error, Result};
use crate::host::platform;

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
pub(super) fn identity(path: &Path) -> Result<FileIdentity> {
    platform::file_identity(path)
        .map(|(device, index)| FileIdentity { device, index })
        .map_err(|error| {
            Error::unsupported(format!("cannot identify {}", path.display())).with_source(error)
        })
}

/// Path of `path` relative to `root`, `/`-separated; empty for the root itself.
pub(super) fn relative_text(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        parts.push(component.as_os_str().to_str()?);
    }
    Some(parts.join("/"))
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
pub(super) fn canonicalize_lenient(path: &Path) -> std::io::Result<(PathBuf, bool)> {
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
pub(super) fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
