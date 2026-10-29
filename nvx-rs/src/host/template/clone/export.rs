//! The export of a clone: the host paths that it shares with its root, as
//! [`filesystem::plan`](crate::host::filesystem::plan) plans them, which
//! [`ClonePlan::new`](super::ClonePlan::new) checks before the clone's launch, and the forms in
//! which the clone's restore attaches the export, on the template's dormant filesystem slot, and
//! in which its restore sequence sends the export's mapping table.
//!
//! A template's capture leaves its filesystem slot dormant, so that each restore binds a clone's
//! own export to it, or none. A template is a platform snapshot, to whose dormant slot OpenVMM lets
//! a restore bind an aggregate, which it checks as it checks a cold boot's, before any processor
//! runs. So a clone exports what a cold launch exports, any export plan that `filesystem::plan`
//! makes for the container root: an aggregate of up to [`MAX_CHILDREN`] host directories and
//! files, each read-only or read-write, with OpenVMM's hidden, re-exposed, and writable paths, and
//! the plan's owner. The restore attaches it as a cold launch attaches it,
//! [`ExportPlan::openvmm_arguments`], and the clone's guest mounts its root, which lists its
//! children by their indices, so the clone's mapping table is a cold launch's,
//! [`ExportPlan::guest_table`], whose sources name each path by its child's index.
//!
//! [`MAX_CHILDREN`]: crate::host::filesystem::MAX_CHILDREN

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::host::filesystem::{ExportPlan, ReservedTargets};
use crate::protocol::HostMap;

/// The export of a clone that [`ClonePlan::new`](super::ClonePlan::new) checked: an export plan
/// that [`filesystem::plan`](crate::host::filesystem::plan) could have made for the container
/// root, which its restore binds to the template's dormant filesystem slot.
#[derive(Debug, Clone)]
pub(super) struct CloneExport {
    /// The plan.
    plan: ExportPlan,
}

impl CloneExport {
    /// Returns the export of `plan`, an export plan of a clone's request, without reading
    /// anything.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a plan that
    /// [`filesystem::plan`](crate::host::filesystem::plan) could not have made for the container
    /// root, [`ReservedTargets::CONTAINER_ROOT`], as [`ExportPlan::check`] finds it, such as one
    /// that maps paths but exports no directory or file, exports some but maps no path, or exports
    /// more directories and files than OpenVMM attaches.
    pub(super) fn new(plan: &ExportPlan) -> Result<Self> {
        plan.check(ReservedTargets::CONTAINER_ROOT)?;
        Ok(Self { plan: plan.clone() })
    }

    /// Returns the export's plan.
    pub(super) fn plan(&self) -> &ExportPlan {
        &self.plan
    }

    /// Refuses the export if one of its directories or files lies inside the directory of a
    /// template store, `resolved`, or one of its directories holds it: the guest could then reach
    /// the payload of a generation that clones lease, and write it through a writable child,
    /// though nothing may change a leased payload or its snapshot. Refuses it as well if it lets
    /// the guest write in one of `through`, the directories through which the store's path,
    /// `store`, leads: the host reaches the store, and OpenVMM the snapshots in it, by that path,
    /// so the guest could replace a link or a directory of the path, and turn the path elsewhere,
    /// or make a directory of it unsearchable, and keep the host from following it.
    ///
    /// Refuses it too if one of its directories is, or holds, the directory of the clone's
    /// sandbox, `sandbox`, which the lease names by its path with its links resolved, or if one of
    /// its files lies in that directory itself: the host writes the clone's records there, by
    /// their paths, so the guest could otherwise read or change them, or replace the directory, or
    /// one above it, with a link, and turn the host's writes elsewhere, into a leased snapshot,
    /// say. The directories above it are the only ones through which that path leads, and only an
    /// export that holds it could let the guest change one. A directory inside the sandbox's,
    /// among the caller's files, holds none of the host's records, nor is a file in such a
    /// directory one of them.
    ///
    /// The check reads nothing: it compares the paths of the export's directories and files, and
    /// the paths in which the guest may write, as the plan resolved them, with `resolved`,
    /// `through`, and `sandbox`, which the caller and the lease resolved as
    /// [`filesystem::canonicalize`] resolves the plan's paths, so that no link, and on Windows no
    /// other letter case, short name, or verbatim prefix, hides an overlap. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for such an export.
    ///
    /// Since the check compares the paths that the export and the store resolve to, an alias of
    /// the store that an administrator configures is outside it, as the directories above the
    /// store are: a bind mount or a mount point of the store, or of a directory above or inside
    /// it, a loopback share, or a hard link of a payload. Whoever names an export must not name
    /// such an alias, nor a directory that holds one. Nor does the check see what changes after
    /// it: OpenVMM resolves the paths of an export again at each launch and restore, so the
    /// directories above each of the export's directories and files must be ones that no guest may
    /// change, as those above the clone's block devices must, which [Block devices by
    /// path](crate::host::template::Store#block-devices-by-path) describes: whoever names an
    /// export must not name one inside a directory that a writable export of another sandbox or
    /// clone holds.
    ///
    /// [`filesystem::canonicalize`]: crate::host::filesystem::canonicalize
    pub(super) fn check_outside(
        &self,
        store: &Path,
        resolved: &Path,
        through: &[PathBuf],
        sandbox: &Path,
    ) -> Result<()> {
        for child in &self.plan.children {
            if child.root.starts_with(resolved) || resolved.starts_with(&child.root) {
                return Err(Error::unsupported(format!(
                    "the export of a clone cannot share {}, which overlaps the template store {}, \
                     whose leased payloads nothing may change",
                    child.root.display(),
                    resolved.display()
                )));
            }
            if sandbox.starts_with(&child.root) {
                return Err(Error::unsupported(format!(
                    "the export of a clone cannot share {}, which holds the directory of the \
                     clone's sandbox {}, in which the host writes the clone's records",
                    child.root.display(),
                    sandbox.display()
                )));
            }
            // A file in the sandbox's directory itself may be one of the host's records there.
            if child.file && child.root.parent() == Some(sandbox) {
                return Err(Error::unsupported(format!(
                    "the export of a clone cannot share {}, a file in the directory of the \
                     clone's sandbox {}, in which the host writes the clone's records",
                    child.root.display(),
                    sandbox.display()
                )));
            }
            // The guest may write anywhere in a writable directory, and a writable file, unless the
            // plan limits its writes to some of the directory's paths.
            let written: &[PathBuf] = match (child.writable, child.write.is_empty()) {
                (false, _) => &[],
                (true, true) => std::slice::from_ref(&child.root),
                (true, false) => child.write.as_slice(),
            };
            let directory = through
                .iter()
                .find(|directory| written.iter().any(|path| directory.starts_with(path)));
            if let Some(directory) = directory {
                return Err(Error::unsupported(format!(
                    "the export of a clone cannot let its guest write in {}, through which the \
                     path of the template store {} leads, since the guest could then turn that \
                     path elsewhere, or keep the host from following it",
                    directory.display(),
                    store.display()
                )));
            }
        }
        Ok(())
    }

    /// Returns the export's mapping table, which the clone's restore sequence sends once the
    /// clone is ready, as a cold launch's guest receives it, [`ExportPlan::guest_table`]: the
    /// plan's binds, in their order, each from its path in the export, which a source names by
    /// its child's index, to its target, with its access.
    pub(super) fn table(&self) -> Vec<HostMap> {
        self.plan.guest_table()
    }

    /// Returns OpenVMM's arguments that bind the export to the template's dormant filesystem slot
    /// at a restore, as a cold launch attaches it, [`ExportPlan::openvmm_arguments`]:
    /// `--mount-aggregate` with [`GUEST_EXPORT`](crate::host::filesystem::GUEST_EXPORT), a
    /// `--mount-child` of each of its directories and files, named by its index, with its mode,
    /// `rw` if the guest may write it, and `ro` otherwise, and `file` for a file; then their
    /// hidden, re-exposed, and writable paths, with `--mount-deny`, `--mount-allow`, and
    /// `--mount-write`; and `--mount-owner caller` for an export of the caller owner. Each path is
    /// absolute, as the plan resolved it.
    pub(super) fn arguments(&self) -> Vec<String> {
        self.plan.openvmm_arguments()
    }
}
