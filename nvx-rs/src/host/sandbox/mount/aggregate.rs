//! The planning of the export of a sandbox's shares.

use std::io;
use std::path::PathBuf;

use super::{
    MountAccess, MountKind, MountOwner, PolicyKind, SandboxMount, validate_mount_host_paths,
    validate_mounts,
};
use crate::error::Result;
use crate::host::filesystem::{
    self, ExportPlan, ExportRequest, Mapping, Owner, PathList, ReservedTargets, Target,
};
use crate::host::paths;
use crate::host::sandbox::refused;

/// Plans the export of `mounts`, the live shares of one sandbox, through the planner of every
/// NVX caller, [`filesystem::plan`], after it checks them with [`validate_mounts`], each share
/// with [`SandboxMount::validated`], and their host paths with [`validate_mount_host_paths`].
///
/// Each share maps its host path at its guest target, read-only or read-write, and its denied,
/// allowed, and writable paths, joined to its absolute host path when relative, become the
/// export's denied, allowed, and write paths. The owner of the shares, which [`validate_mounts`]
/// requires to be one, becomes the export's, and the container root reserves the trees of its
/// runtime. Each share keeps the kind with which it was requested: a host path that the planner
/// finds to be of the other kind is refused, as [`SandboxMount::validated`] refuses it. Returns
/// `None` without shares.
///
/// Fails as those checks and [`filesystem::plan`] do, and with
/// [`ErrorKind::Io`](crate::ErrorKind::Io) if a relative host path meets an unreadable current
/// directory, or Windows cannot qualify a drive-relative path.
pub fn plan_mounts(mounts: &[SandboxMount]) -> Result<Option<ExportPlan>> {
    validate_mounts(mounts)?;
    for mount in mounts {
        mount.validated()?;
    }
    validate_mount_host_paths(mounts)?;
    plan_mounts_with(mounts, &paths::current_dir)
}

/// Plans the export of `mounts` as [`plan_mounts`] does, without checking them against the host
/// first, with `cwd` as the current directory.
pub(crate) fn plan_mounts_with(
    mounts: &[SandboxMount],
    cwd: &dyn Fn() -> io::Result<PathBuf>,
) -> Result<Option<ExportPlan>> {
    let Some(first) = mounts.first() else {
        return Ok(None);
    };
    let mut hosts = Vec::with_capacity(mounts.len());
    let (mut denied, mut allowed, mut write) = (Vec::new(), Vec::new(), Vec::new());
    for mount in mounts {
        hosts.push(mount.absolute_with(cwd)?.host_path);
        for (kind, paths) in mount.absolute_policy_paths_with(cwd)? {
            match kind {
                PolicyKind::Denied => &mut denied,
                PolicyKind::Allowed => &mut allowed,
                PolicyKind::Writable => &mut write,
            }
            .extend(paths.into_iter().map(PathBuf::from));
        }
    }
    let mappings: Vec<Mapping<'_>> = mounts
        .iter()
        .zip(&hosts)
        .map(|(mount, host)| Mapping {
            name: "--mount",
            path: host,
            target: Target::Guest(&mount.guest_target),
            read_only: mount.access == MountAccess::ReadOnly,
        })
        .collect();
    let list = |kind: PolicyKind, paths| PathList {
        name: kind.option(),
        paths,
    };
    let plan = filesystem::plan(&ExportRequest {
        mappings: &mappings,
        denied: list(PolicyKind::Denied, &denied),
        allowed: list(PolicyKind::Allowed, &allowed),
        write: list(PolicyKind::Writable, &write),
        owner: match first.owner {
            MountOwner::Vmm => Owner::Vmm,
            MountOwner::Caller => Owner::Caller,
        },
        reserved: ReservedTargets::CONTAINER_ROOT,
    })?;
    // The planner reads each host path's kind again; a directory that replaced a shared file
    // since the share was requested is refused rather than shared whole, and so is a host path
    // that now leads into another share's directory, where the share would have no root of its
    // own.
    if let Some(plan) = &plan {
        for mount in mounts {
            let Some(bind) = plan
                .binds
                .iter()
                .find(|bind| bind.target == mount.guest_target && bind.source.is_empty())
            else {
                return Err(refused(format!(
                    "sandbox mount host path {} lies inside the host directory of another share; \
                     a share could reach files of another under its access mode",
                    mount.host_path.display()
                )));
            };
            let file = plan.children.get(bind.child).map(|child| child.file);
            if file != Some(mount.kind == MountKind::File) {
                return Err(mount.kind_refusal());
            }
        }
    }
    Ok(plan)
}
