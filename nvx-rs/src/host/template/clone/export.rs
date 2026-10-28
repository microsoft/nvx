//! The export of a clone: the host paths that it shares with its root, as
//! [`filesystem::plan`](crate::host::filesystem::plan) plans them, which
//! [`ClonePlan::new`](super::ClonePlan::new) checks before the clone's launch, and the forms in
//! which the clone's restore attaches the export, on the template's dormant filesystem slot, and
//! in which its restore sequence sends the export's mapping table.
//!
//! A template's capture leaves its filesystem slot dormant, so that each restore binds a clone's
//! own export to it, or none. OpenVMM binds one directory to a dormant slot, with `--mount`, but
//! no aggregate, so a clone's export plan must export exactly one child, a directory: the
//! directory is then the export's root, which the clone's guest mounts, and the sources of the
//! plan's binds name paths in it without the child's index, which only an aggregate's root lists.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::host::filesystem::{ExportPlan, GUEST_EXPORT, Owner, ReservedTargets};
use crate::protocol::HostMap;

/// The export of a clone that [`ClonePlan::new`](super::ClonePlan::new) checked: an export plan
/// of one child, a directory, which its restore binds to the template's dormant filesystem slot.
#[derive(Debug, Clone)]
pub(super) struct DirectoryExport {
    /// The plan, whose one child is a directory.
    plan: ExportPlan,
}

impl DirectoryExport {
    /// Returns the export of `plan`, an export plan of a clone's request, without reading
    /// anything.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a plan that
    /// [`filesystem::plan`](crate::host::filesystem::plan) could not have made for the container
    /// root, [`ReservedTargets::CONTAINER_ROOT`], as [`ExportPlan::check`] finds it, such as one
    /// that maps paths but exports no directory or file, or exports some but maps no path; and for
    /// a plan that exports more than one directory or file, or a file, which a dormant filesystem
    /// slot does not bind.
    pub(super) fn new(plan: &ExportPlan) -> Result<Self> {
        plan.check(ReservedTargets::CONTAINER_ROOT)?;
        match plan.children.as_slice() {
            [child] if !child.file => Ok(Self { plan: plan.clone() }),
            [child] => Err(Error::unsupported(format!(
                "the export of a clone binds one directory to its template's dormant filesystem \
                 slot, but its export plan exports the file {}",
                child.root.display()
            ))),
            children => Err(Error::unsupported(format!(
                "the export of a clone binds one directory to its template's dormant filesystem \
                 slot, but its export plan exports {} directories and files",
                children.len()
            ))),
        }
    }

    /// Returns the export's plan.
    pub(super) fn plan(&self) -> &ExportPlan {
        &self.plan
    }

    /// Refuses the export if its directory lies inside the directory of a template store,
    /// `resolved`, or holds it: the guest could then reach the payload of a generation that clones
    /// lease, and write it through a writable export, though nothing may change a leased payload
    /// or its snapshot. Refuses it as well if it lets the guest write in one of `through`, the
    /// directories through which the store's path, `store`, leads: the host reaches the store,
    /// and OpenVMM the snapshots in it, by that path, so the guest could replace a link or a
    /// directory of the path, and turn the path elsewhere.
    ///
    /// The check reads nothing: it compares the directory's path, and the paths in which the
    /// guest may write, as the plan resolved them, with `resolved` and `through`, which the caller
    /// resolved as [`filesystem::canonicalize`] resolves the plan's paths, so that no link, and on
    /// Windows no other letter case, short name, or verbatim prefix, hides an overlap. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for such an export.
    ///
    /// [`filesystem::canonicalize`]: crate::host::filesystem::canonicalize
    pub(super) fn check_outside(
        &self,
        store: &Path,
        resolved: &Path,
        through: &[PathBuf],
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
            // The guest may write anywhere in a writable directory, unless the plan limits its
            // writes to some of the directory's paths.
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
                     path elsewhere",
                    directory.display(),
                    store.display()
                )));
            }
        }
        Ok(())
    }

    /// Returns the export's mapping table, which the clone's restore sequence sends once the
    /// clone is ready: the plan's binds, in their order, each from its path in the export's
    /// directory, which a source names without the child's index, and an empty source for the
    /// directory itself, to its target, with its access.
    pub(super) fn table(&self) -> Vec<HostMap> {
        self.plan
            .binds
            .iter()
            .map(|bind| HostMap {
                source: bind.source.clone(),
                target: bind.target.clone(),
                read_only: bind.read_only,
            })
            .collect()
    }

    /// Returns OpenVMM's arguments that bind the export to the template's dormant filesystem slot
    /// at a restore: `--mount` with [`GUEST_EXPORT`], the directory, and its mode, `rw` if the
    /// guest may write it, and `ro` otherwise, always given, so that the directory's path may hold
    /// commas; then the directory's hidden, re-exposed, and writable paths, with `--mount-deny`,
    /// `--mount-allow`, and `--mount-write`; and `--mount-owner caller` for an export of the
    /// caller owner. Each path is absolute, as the plan resolved it.
    pub(super) fn arguments(&self) -> Vec<String> {
        let mut arguments = Vec::new();
        // The export has one child, which `new` requires.
        for child in &self.plan.children {
            let mode = if child.writable { "rw" } else { "ro" };
            arguments.push("--mount".to_owned());
            arguments.push(format!(
                "{GUEST_EXPORT},{},{mode}",
                child.root.to_string_lossy()
            ));
            for (option, paths) in [
                ("--mount-deny", &child.denied),
                ("--mount-allow", &child.allowed),
                ("--mount-write", &child.write),
            ] {
                for path in paths {
                    arguments.push(option.to_owned());
                    arguments.push(path.to_string_lossy().into_owned());
                }
            }
        }
        if self.plan.owner == Owner::Caller {
            arguments.extend(["--mount-owner".to_owned(), "caller".to_owned()]);
        }
        arguments
    }
}
