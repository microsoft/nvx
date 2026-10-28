//! Resolution of the denied, allowed, and writable paths of an export.

use std::path::{Path, PathBuf};

use super::host_path::{canonicalize, canonicalize_lenient, identity};
use super::{ExportChild, FileIdentity, Mapped, PathList, outermost};
use crate::error::{Error, Result};

/// A path of an access policy, resolved.
pub(super) struct PolicyPath {
    /// Host path as the caller supplied it, for diagnostics.
    given: PathBuf,
    /// Link-resolved host path.
    canonical: PathBuf,
}

/// Returns the resolved paths of `paths`.
pub(super) fn canonicals(paths: &[PolicyPath]) -> Vec<PathBuf> {
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
pub(super) fn write_paths(
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
pub(super) fn check_read_only(
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
pub(super) fn distinct_roots(roots: &[PathBuf]) -> Result<()> {
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
pub(super) fn allowed_paths(list: PathList<'_>, roots: &[PathBuf]) -> Result<Vec<PolicyPath>> {
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
pub(super) fn denied_paths(
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
pub(super) fn check_allowed_paths(
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
pub(super) fn check_write_paths(
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
pub(super) fn needed_denied_paths(denied: &[PathBuf], allowed: &[PathBuf]) -> Vec<PathBuf> {
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
/// another. [`crate::host::filesystem::plan`] checks these rules on the paths that it resolves. It reads nothing from the
/// host.
pub(super) fn policy_rule_problem(child: &ExportChild) -> Option<String> {
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
pub(super) fn nearest_rule<'p>(
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
pub(super) struct Writable {
    /// Whether the guest may modify the child.
    pub(super) writable: bool,
    /// The paths to which OpenVMM limits the guest's writes; empty if the guest may modify all of
    /// a writable child.
    pub(super) write: Vec<PathBuf>,
}

impl Writable {
    /// Returns whether a workload could move `path`, which lies in the child at `root`: whether it
    /// lies strictly inside a writable part, in a directory that the workload may change.
    pub(super) fn moves(&self, root: &Path, path: &Path) -> bool {
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
pub(super) fn writable_part(root: &Path, mapped: &[Mapped<'_>], write: &[PolicyPath]) -> Writable {
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
