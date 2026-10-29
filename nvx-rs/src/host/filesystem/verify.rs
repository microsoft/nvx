//! Checks and rendering of a recorded export plan.

use std::fs;
use std::path::PathBuf;

use super::guest_path::depth;
use super::host_path::{display_path, relative_text};
use super::limits::policy_limits;
use super::policy::{Writable, nearest_rule, policy_rule_problem};
use super::{
    Bind, CALLER_OWNER_ON_WINDOWS, ExportChild, ExportPlan, FileIdentity, GUEST_EXPORT,
    MAX_AGGREGATE_POLICY_BYTES, MAX_CHILDREN, MAX_GUEST_PATH, Owner, ReservedTargets,
    changed_object, check_caller_roots, outermost, resolved_path_problem, target_problem,
    uninspected, volume_root_refusal,
};
use crate::error::{Error, Result};
use crate::host::platform;
use crate::protocol::{HostMap, MAPS_TOKEN, MAX_HOST_MAPPINGS};

impl ExportPlan {
    /// Checks that a plan read back from a record is one that [`crate::host::filesystem::plan`] could have made for a root
    /// that reserves `reserved`: at least one and at most [`MAX_CHILDREN`] children, none inside
    /// another, each an absolute host path below a volume's root, with its host paths in the form
    /// in which `plan` resolves them, whose policy paths lie inside it within OpenVMM's limits and
    /// keep its rules, with an identity slot for each denied path, and writable exactly when a bind
    /// maps a path of it read-write; and at least one and at most [`MAX_HOST_MAPPINGS`] binds,
    /// each of a known child, from a relative source that holds the host's names joined with `/`,
    /// that a guest path can hold, and that the child does not hide, unless it shows paths inside
    /// it again, to a free guest target, parents before children, with a bind of every child's
    /// root. A target inside another's must show what the other shows there. A child's write
    /// paths must be the outermost paths of its read-write binds, unless one maps the whole child,
    /// when they may narrow its writes only to paths that keep its other read-write binds
    /// writable; and each denied or mapped object that a workload could move, strictly inside a
    /// part of the child that the guest may write, must carry its identity. It reads nothing from
    /// the host; [`verify`](Self::verify) checks the host.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
    pub fn check(&self, reserved: ReservedTargets<'_>) -> Result<()> {
        let invalid = |reason: String| {
            Err(Error::unsupported(format!(
                "the recorded export plan is invalid: {reason}"
            )))
        };
        if self.children.is_empty() || self.children.len() > MAX_CHILDREN {
            return invalid(format!(
                "it exports {} directories and files, but OpenVMM exports from 1 to \
                 {MAX_CHILDREN}",
                self.children.len()
            ));
        }
        if self.binds.is_empty() || self.binds.len() > MAX_HOST_MAPPINGS {
            return invalid(format!(
                "it maps {} paths, but a sandbox maps from 1 to {MAX_HOST_MAPPINGS}",
                self.binds.len()
            ));
        }
        let mut policy_bytes = 0;
        for (index, child) in self.children.iter().enumerate() {
            if !child.root.is_absolute() || volume_root_refusal(&child.root).is_some() {
                return invalid(format!(
                    "child {index}, {}, is not an absolute path below a volume's root",
                    child.root.display()
                ));
            }
            if let Some((path, problem)) = std::iter::once(&child.root)
                .chain(&child.denied)
                .chain(&child.allowed)
                .chain(&child.write)
                .find_map(|path| resolved_path_problem(path).map(|problem| (path, problem)))
            {
                return invalid(format!(
                    "child {index} has the host path {path:?}, which {problem}"
                ));
            }
            if child.denied_identities.len() != child.denied.len() {
                return invalid(format!(
                    "child {index} has {} denied paths but {} identities",
                    child.denied.len(),
                    child.denied_identities.len()
                ));
            }
            if let Some(path) = [&child.denied, &child.allowed, &child.write]
                .into_iter()
                .flatten()
                .find(|path| !path.starts_with(&child.root) || (child.file && *path != &child.root))
            {
                return invalid(format!(
                    "child {index} has the policy path {}, which is not inside it",
                    path.display()
                ));
            }
            for (paths, action) in [
                (&child.denied, "hide"),
                (&child.allowed, "show again"),
                (&child.write, "make writable"),
            ] {
                match policy_limits(&child.root, paths, action) {
                    Ok(bytes) => policy_bytes += bytes,
                    Err(problem) => return invalid(format!("child {index}: {problem}")),
                }
            }
            if let Some(other) = self.children[..index].iter().position(|other| {
                other.root.starts_with(&child.root) || child.root.starts_with(&other.root)
            }) {
                return invalid(format!(
                    "child {index}, {}, overlaps child {other}, {}",
                    child.root.display(),
                    self.children[other].root.display()
                ));
            }
            if let Some(problem) = policy_rule_problem(child) {
                return invalid(format!("child {index}: {problem}"));
            }
            if !child.writable && !child.write.is_empty() {
                return invalid(format!("child {index} is read-only but has writable paths"));
            }
            if !self
                .binds
                .iter()
                .any(|bind| bind.child == index && bind.source.is_empty())
            {
                return invalid(format!("it maps no path at the root of child {index}"));
            }
            // OpenVMM lets the guest write a child that a read-write bind needs, and no other.
            let read_write = self
                .binds
                .iter()
                .any(|bind| bind.child == index && !bind.read_only);
            if child.writable != read_write {
                return invalid(if child.writable {
                    format!("child {index} is writable but maps no path read-write")
                } else {
                    format!("child {index} is read-only but maps a path read-write")
                });
            }
        }
        if policy_bytes > MAX_AGGREGATE_POLICY_BYTES {
            return invalid(format!(
                "the paths that OpenVMM is to hide, show again, or make writable exceed its \
                 {MAX_AGGREGATE_POLICY_BYTES}-byte limit"
            ));
        }
        let mut previous: Option<&Bind> = None;
        for (index, bind) in self.binds.iter().enumerate() {
            let Some(child) = self.children.get(bind.child) else {
                return invalid(format!(
                    "it maps a path of the unknown child {}",
                    bind.child
                ));
            };
            if !bind.source.is_empty()
                && (child.file
                    || bind
                        .source
                        .split('/')
                        .any(|component| matches!(component, "" | "." | "..")))
            {
                return invalid(format!(
                    "it maps the path {:?} of child {}, which is not a path inside a directory",
                    bind.source, bind.child
                ));
            }
            // The guest receives the source as a path below its mount of the export.
            if bind.source.contains('\0') {
                return invalid(format!(
                    "it maps the path {:?} of child {}, which contains a NUL character",
                    bind.source, bind.child
                ));
            }
            if GUEST_EXPORT.len() + 1 + bind.guest_source().len() > MAX_GUEST_PATH {
                return invalid(format!(
                    "it maps a path of {} bytes in child {}, which is too long for a guest path",
                    bind.source.len(),
                    bind.child
                ));
            }
            let path = if bind.source.is_empty() {
                child.root.clone()
            } else {
                child.root.join(&bind.source)
            };
            // The host checks the object at that path, which must be the one that the guest maps:
            // the source must hold the host's names below the root, joined with `/`, as `plan`
            // spells them. A Windows host would read a backslash in a name as a separator, and a
            // name with a root or a drive as another path.
            if relative_text(&child.root, &path).as_deref() != Some(bind.source.as_str()) {
                return invalid(format!(
                    "it maps the path {:?} of child {}, which the host reads as other names",
                    bind.source, bind.child
                ));
            }
            // A mapped path shows something: OpenVMM does not hide it, or it shows the allowed
            // paths inside it again.
            if matches!(
                nearest_rule(&path, &child.denied, &child.allowed, true),
                Some((_, true))
            ) && !child
                .allowed
                .iter()
                .any(|shown| *shown != path && shown.starts_with(&path))
            {
                return invalid(format!(
                    "it maps {} at {}, but child {} hides it",
                    path.display(),
                    bind.target,
                    bind.child
                ));
            }
            if let Some(problem) = target_problem(&bind.target) {
                return invalid(format!("its guest target {:?} {problem}", bind.target));
            }
            if let Some(reserved) = reserved.reservation(&bind.target) {
                return invalid(format!(
                    "its guest target {} would cover the guest's {reserved}",
                    bind.target
                ));
            }
            if let Some(previous) = previous
                && (depth(&previous.target), previous.target.as_str())
                    >= (depth(&bind.target), bind.target.as_str())
            {
                return invalid(format!(
                    "it maps {} after {}, out of order or twice",
                    bind.target, previous.target
                ));
            }
            // A target inside another shows what the other shows there, as `plan` requires.
            for outer in &self.binds[..index] {
                let Some(rest) = bind
                    .target
                    .strip_prefix(outer.target.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
                else {
                    continue;
                };
                let shown = if outer.source.is_empty() {
                    rest.to_owned()
                } else {
                    format!("{}/{rest}", outer.source)
                };
                if bind.child != outer.child || bind.source != shown {
                    return invalid(format!(
                        "it maps {} inside {}, where it would hide part of that path",
                        bind.target, outer.target
                    ));
                }
            }
            previous = Some(bind);
        }
        for (index, child) in self.children.iter().enumerate() {
            let bound = |bind: &Bind| {
                if bind.source.is_empty() {
                    child.root.clone()
                } else {
                    child.root.join(&bind.source)
                }
            };
            let read_write: Vec<PathBuf> = self
                .binds
                .iter()
                .filter(|bind| bind.child == index && !bind.read_only)
                .map(bound)
                .collect();
            // A child that a read-write bind maps whole is writable throughout, unless write paths
            // narrow its writes, which must then keep its other read-write binds writable; any
            // other child's writes are narrowed to exactly what its read-write binds map.
            if read_write.contains(&child.root) {
                if let Some(path) = read_write.iter().find(|path| {
                    **path != child.root
                        && !child.write.is_empty()
                        && !child.write.iter().any(|write| path.starts_with(write))
                }) {
                    return invalid(format!(
                        "child {index} narrows its writes so that {}, which it maps read-write, \
                         stays read-only",
                        path.display()
                    ));
                }
            } else {
                let mut write = child.write.clone();
                write.sort();
                if outermost(read_write) != write {
                    return invalid(format!(
                        "child {index} lets the guest write other paths than those that it maps \
                         read-write"
                    ));
                }
            }
            // A workload that may write a part of the child could move an object there and put a
            // decoy at its path, so the plan pins each denied and mapped object in such a part, as
            // `plan` does, for `verify` to check it at the next start.
            let part = Writable {
                writable: child.writable,
                write: child.write.clone(),
            };
            if let Some((path, _)) = child
                .denied
                .iter()
                .zip(&child.denied_identities)
                .find(|(path, identity)| identity.is_none() && part.moves(&child.root, path))
            {
                return invalid(format!(
                    "child {index} does not pin the denied path {}, which a workload could move",
                    path.display()
                ));
            }
            if let Some(bind) = self.binds.iter().find(|bind| {
                bind.child == index
                    && bind.identity.is_none()
                    && part.moves(&child.root, &bound(bind))
            }) {
                return invalid(format!(
                    "it does not pin the object that it maps at {}, which a workload could move",
                    bind.target
                ));
            }
        }
        Ok(())
    }

    /// Checks that the host keeps every read-only mapping read-only, that every child's root is
    /// still the kind of object that [`crate::host::filesystem::plan`] exported, that every mapped and denied path inside a
    /// writable part still names the object it named then, and that the host still supports the
    /// owner.
    ///
    /// [`crate::host::filesystem::plan`] refuses a read-only path inside a part that the guest may write, which only the
    /// guest's bind mount would keep read-only; a recorded plan that holds one fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). A file that a directory replaced
    /// would otherwise grant the whole directory, and a workload that may write a directory could
    /// rename a denied or mapped object inside it and put a decoy at its path, so the next start
    /// would hide or bind the decoy instead; these fail with
    /// [`ErrorKind::Other`](crate::ErrorKind::Other), as does a path that no longer reaches its
    /// object, as the host sees it: one whose object went, one a component of which is no longer a
    /// directory or is a link that loops, as a link that names itself does, and one that the host
    /// may no longer search or open. With the caller owner, the
    /// host must be a Linux host, and no exported directory or file may belong to UID 0 or GID 0;
    /// these fail with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). Any other
    /// failure to inspect an object, which shows nothing of the object, fails with
    /// [`ErrorKind::Io`](crate::ErrorKind::Io), with its cause. Run it while the VM is stopped, so
    /// that no workload can race with it; OpenVMM checks each child's kind again when it opens it.
    pub fn verify(&self) -> Result<()> {
        for bind in self.binds.iter().filter(|bind| bind.read_only) {
            let (child, path) = self.bound(bind)?;
            // Without writable paths, the guest may write all of a writable child.
            if child.writable
                && (child.write.is_empty()
                    || child.write.iter().any(|write| path.starts_with(write)))
            {
                return Err(Error::unsupported(format!(
                    "the sandbox maps {} read-only inside a part of {} that the guest may write, \
                     where the host cannot keep it read-only; deprovision the sandbox, and \
                     provision it again without read-only paths inside read-write directories",
                    path.display(),
                    child.root.display()
                )));
            }
        }
        for child in &self.children {
            let kind_matches = match fs::symlink_metadata(&child.root) {
                Ok(metadata) if child.file => metadata.is_file(),
                Ok(metadata) => metadata.is_dir(),
                Err(error) => return Err(uninspected(&child.root, error)),
            };
            if !kind_matches {
                return Err(changed_object(&child.root));
            }
        }
        for bind in &self.binds {
            let Some(expected) = bind.identity else {
                continue;
            };
            let (_, path) = self.bound(bind)?;
            match platform::file_identity(&path) {
                Ok((device, index)) if (FileIdentity { device, index }) == expected => {}
                Ok(_) => return Err(changed_object(&path)),
                Err(error) => return Err(uninspected(&path, error)),
            }
        }
        for child in &self.children {
            for (path, expected) in child.denied.iter().zip(&child.denied_identities) {
                let Some(expected) = expected else {
                    continue;
                };
                match platform::file_identity(path) {
                    Ok((device, index)) if (FileIdentity { device, index }) == *expected => {}
                    Ok(_) => return Err(changed_object(path)),
                    Err(error) => return Err(uninspected(path, error)),
                }
            }
        }
        if self.owner == Owner::Caller {
            if cfg!(windows) {
                return Err(Error::unsupported(CALLER_OWNER_ON_WINDOWS));
            }
            check_caller_roots(&self.children, uninspected)?;
        }
        Ok(())
    }

    /// Returns the child that holds `bind` and the host path that `bind` maps.
    fn bound(&self, bind: &Bind) -> Result<(&ExportChild, PathBuf)> {
        let Some(child) = self.children.get(bind.child) else {
            return Err(Error::io(
                "the sandbox state maps a path of an unknown host directory",
            ));
        };
        let path = if bind.source.is_empty() {
            child.root.clone()
        } else {
            child.root.join(&bind.source)
        };
        Ok((child, path))
    }

    /// Kernel command-line token that tells the guest agent how many bind mounts to expect over
    /// the control channel.
    pub fn kernel_token(&self) -> String {
        format!("{MAPS_TOKEN}{}", self.binds.len())
    }

    /// The bind mounts in the order and form in which the guest agent receives them.
    pub fn guest_table(&self) -> Vec<HostMap> {
        self.binds
            .iter()
            .map(|bind| HostMap {
                source: bind.guest_source(),
                target: bind.target.clone(),
                read_only: bind.read_only,
            })
            .collect()
    }

    /// OpenVMM options that export the plan's directories and files at [`GUEST_EXPORT`], with
    /// their access policies and the owner. A file child carries the `file` flag, so OpenVMM
    /// refuses a directory that replaced the file.
    pub fn openvmm_arguments(&self) -> Vec<String> {
        let mut arguments = vec!["--mount-aggregate".to_owned(), GUEST_EXPORT.to_owned()];
        for (index, child) in self.children.iter().enumerate() {
            let mode = if child.writable { "rw" } else { "ro" };
            let kind = if child.file { ",file" } else { "" };
            arguments.push("--mount-child".to_owned());
            arguments.push(format!(
                "{index},{},{mode}{kind}",
                display_path(&child.root)
            ));
        }
        for (option, paths) in [
            (
                "--mount-deny",
                ExportChild::denied_paths as fn(&ExportChild) -> &[PathBuf],
            ),
            ("--mount-allow", ExportChild::allowed_paths),
            ("--mount-write", ExportChild::write_paths),
        ] {
            for path in self.children.iter().flat_map(paths) {
                arguments.push(option.to_owned());
                arguments.push(display_path(path));
            }
        }
        if self.owner == Owner::Caller {
            arguments.extend(["--mount-owner".to_owned(), "caller".to_owned()]);
        }
        arguments
    }
}

impl ExportChild {
    /// Returns the paths that OpenVMM hides.
    fn denied_paths(&self) -> &[PathBuf] {
        &self.denied
    }

    /// Returns the paths that stay visible inside denied paths.
    fn allowed_paths(&self) -> &[PathBuf] {
        &self.allowed
    }

    /// Returns the paths to which OpenVMM limits the guest's writes.
    fn write_paths(&self) -> &[PathBuf] {
        &self.write
    }
}
