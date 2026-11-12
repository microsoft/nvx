//! The checks of the host paths of shares: shares whose host paths overlap, and policy paths
//! outside their share.

use std::io;
use std::path::{Path, PathBuf};

use super::boot::path_text;
use super::{PolicyKind, SandboxMount, os_error};
use crate::error::Result;
use crate::host::filesystem::ExportPlan;
use crate::host::sandbox::refused;
use crate::host::{paths, platform};

/// The queries of the host that [`validate_mount_host_paths`] makes, which tests replace.
pub(crate) struct HostQueries<'a> {
    /// The current directory, which [`paths::current_dir`] gives.
    pub(crate) cwd: &'a dyn Fn() -> io::Result<PathBuf>,
    /// A path with its links resolved, as [`paths::resolve`] resolves them.
    pub(crate) resolve: &'a dyn Fn(&Path) -> io::Result<PathBuf>,
    /// The identity of the file or directory at a path, following links, which
    /// [`platform::file_identity`] gives.
    pub(crate) identity: &'a dyn Fn(&Path) -> io::Result<(u64, u64)>,
}

impl HostQueries<'_> {
    /// Returns the queries of this host.
    pub(crate) fn native() -> HostQueries<'static> {
        HostQueries {
            cwd: &paths::current_dir,
            resolve: &paths::resolve,
            identity: &platform::file_identity,
        }
    }
}

/// Refuses shares whose host paths overlap, and policy paths outside their share.
///
/// Two host paths overlap if one contains the other once both are resolved, or, since resolving
/// follows symbolic links but not bind mounts, if one has the file identity of the other or of
/// one of its ancestors, as a hard link or a path inside a bind mount of it does. A shared file
/// therefore overlaps a shared directory that contains it, or another share of the same file,
/// but not a file beside it. Each policy path must name a path inside the host directory of its
/// share, and only a denied path may name the share itself.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), or
/// [`ErrorKind::Io`](crate::ErrorKind::Io) if a path cannot be resolved or examined.
pub(crate) fn validate_mount_host_paths(mounts: &[SandboxMount]) -> Result<()> {
    validate_mount_host_paths_with(mounts, &HostQueries::native())
}

/// Implements [`validate_mount_host_paths`] with the given host queries.
pub(crate) fn validate_mount_host_paths_with(
    mounts: &[SandboxMount],
    host: &HostQueries<'_>,
) -> Result<()> {
    let roots = mounts
        .iter()
        .map(|mount| {
            (host.resolve)(&mount.host_path)
                .map_err(|error| os_error(&error, Some(&mount.host_path)))
        })
        .collect::<Result<Vec<_>>>()?;
    if mounts.len() > 1 {
        check_distinct_host_paths(&roots, host)?;
    }
    for (mount, root) in mounts.iter().zip(&roots) {
        for (kind, paths) in mount.absolute_policy_paths_with(host.cwd)? {
            let option = kind.option();
            for path in paths {
                let resolved = (host.resolve)(Path::new(&path))
                    .map_err(|error| os_error(&error, Some(Path::new(&path))))?;
                // A denied root hides the share behind its allowed paths, which OpenVMM requires;
                // it accepts no other policy path at a root.
                if resolved.starts_with(root) && (resolved != *root || kind == PolicyKind::Denied) {
                    continue;
                }
                let several = if mounts.len() > 1 {
                    format!(
                        "; with several shares, each {option} follows the --mount whose \
                         directory it names"
                    )
                } else {
                    String::new()
                };
                return Err(refused(format!(
                    "{option} {path} is not inside the host directory of --mount {}{several}",
                    mount.guest_target
                )));
            }
        }
    }
    Ok(())
}

/// Refuses directories and files of `export` that came to overlap since it was planned: one that
/// a hard link or bind mount made the same object as another, or put inside another, so that a
/// share could reach the files of another under its access mode. [`plan_mounts`] refused such
/// host paths when it planned them, and a launch checks them again, since the host may have
/// changed meanwhile.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), or
/// [`ErrorKind::Io`](crate::ErrorKind::Io) if a path cannot be examined.
///
/// [`plan_mounts`]: crate::host::sandbox::mount::plan_mounts
pub(crate) fn check_distinct_export_roots(
    export: &ExportPlan,
    host: &HostQueries<'_>,
) -> Result<()> {
    if export.children.len() < 2 {
        return Ok(());
    }
    let roots = export
        .children
        .iter()
        .map(|child| path_text(&child.root).map(PathBuf::from))
        .collect::<Result<Vec<_>>>()?;
    check_distinct_host_paths(&roots, host)
}

/// Refuses resolved share roots that overlap, by path or by the file identity of a root and its
/// ancestors.
fn check_distinct_host_paths(roots: &[PathBuf], host: &HostQueries<'_>) -> Result<()> {
    // The identity of each root, then of each of its ancestors.
    let lineages = roots
        .iter()
        .map(|root| {
            root.ancestors()
                .map(|path| (host.identity)(path).map_err(|error| os_error(&error, Some(path))))
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    for (index, root) in roots.iter().enumerate() {
        for (other_index, other) in roots[..index].iter().enumerate() {
            let (shown, other_shown) = (root.display(), other.display());
            if root.starts_with(other) || other.starts_with(root) {
                return Err(refused(format!(
                    "sandbox mount host paths {other_shown} and {shown} overlap; a share could \
                     reach files of another under its access mode"
                )));
            }
            if lineages[other_index].contains(&lineages[index][0])
                || lineages[index].contains(&lineages[other_index][0])
            {
                return Err(refused(format!(
                    "sandbox mount host paths {other_shown} and {shown} name the same directory \
                     or file, or one is inside the other through a bind mount"
                )));
            }
        }
    }
    Ok(())
}
