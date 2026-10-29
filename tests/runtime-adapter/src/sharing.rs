//! The host directories and regular files that the host program shares with a layered root, as
//! [`HostFsSharing`]: each a [`Share`] at an absolute guest path in the root, read-only or
//! read-write, inside the host directories that the adapter authorizes, with host paths hidden
//! inside them, and paths inside hidden ones shown again, and never at the guest targets that the
//! root keeps for itself.
//!
//! [`HostFsSharing::plan`] plans their export with NVX's `filesystem::plan`, an `ExportPlan`,
//! which a launch attaches itself, since an adapter's plan holds none; see
//! [`LayeredLaunch::share`](crate::layered::LayeredLaunch::share).

use std::fs;
use std::path::{Path, PathBuf};

use nvx::host::filesystem::{
    self, ExportPlan, ExportRequest, Mapping, PathList, ReservedTargets, Target,
};
use nvx::{Error, Result};

use crate::root::path_problem;

/// A host directory or regular file that a launch shares with the root, at a guest path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    /// The host path.
    host: PathBuf,
    /// The absolute guest path in the root at which the guest maps it.
    target: String,
    /// Whether the guest may only read it.
    read_only: bool,
}

impl Share {
    /// Returns the share of the host path `host` at the guest path `target`, read-only if
    /// `read_only`, and read-write otherwise. [`HostFsSharing::plan`] checks both paths.
    pub fn new(host: impl Into<PathBuf>, target: impl Into<String>, read_only: bool) -> Self {
        Self {
            host: host.into(),
            target: target.into(),
            read_only,
        }
    }

    /// Reads a share from the text of the host program's `--share`: `HOST:TARGET`, or
    /// `HOST:TARGET:ro` for a read-only share. The guest path TARGET starts at the last `:/`, so
    /// that the host path HOST may hold colons, as a Windows path does after its drive.
    ///
    /// Returns `None` for text without a host path, or without a guest path that starts with `/`.
    pub fn parse(text: &str) -> Option<Self> {
        let (rest, read_only) = match text.strip_suffix(":ro") {
            Some(rest) => (rest, true),
            None => (text, false),
        };
        let (host, target) = rest.rsplit_once(":/")?;
        (!host.is_empty()).then(|| Self::new(host, format!("/{target}"), read_only))
    }

    /// Returns the host path.
    pub fn host(&self) -> &Path {
        &self.host
    }

    /// Sets the host path to `host`.
    pub fn set_host(&mut self, host: impl Into<PathBuf>) {
        self.host = host.into();
    }

    /// Returns the absolute guest path in the root at which the guest maps the share.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Sets the guest path at which the guest maps the share to `target`.
    pub fn set_target(&mut self, target: impl Into<String>) {
        self.target = target.into();
    }

    /// Returns whether the guest may only read the share.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Sets whether the guest may only read the share.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }
}

/// The host paths that a launch shares with the root, as the host program's options give them:
/// the shares, the host paths hidden inside them, those inside hidden ones that stay visible, the
/// guest trees that the adapter keeps for itself, and the host directories that the adapter
/// authorizes.
///
/// Each list has a getter, named after it, a setter, which replaces it, and a method that adds an
/// entry after the others, as its option does. [`plan`](Self::plan) plans the export of the
/// shares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostFsSharing {
    /// The shares, in order: `--share`.
    shares: Vec<Share>,
    /// Host paths that the export hides inside the shares: `--deny`.
    denied: Vec<PathBuf>,
    /// Host paths inside hidden ones that the export shows again: `--allow`.
    allowed: Vec<PathBuf>,
    /// Absolute guest paths of the trees that the adapter keeps for itself, beside those that the
    /// container root keeps: `--reserved`.
    reserved: Vec<String>,
    /// The host directories that the adapter authorizes, inside which each share must lie:
    /// `--share-root`.
    roots: Vec<PathBuf>,
}

impl HostFsSharing {
    /// Returns the shares, in order: `--share`.
    pub fn shares(&self) -> &[Share] {
        &self.shares
    }

    /// Sets the shares to `shares`, in order, in place of the earlier ones.
    pub fn set_shares(&mut self, shares: Vec<Share>) {
        self.shares = shares;
    }

    /// Adds `share` after the other shares, as `--share` does.
    pub fn add_share(&mut self, share: Share) {
        self.shares.push(share);
    }

    /// Returns the host paths that the export hides inside the shares: `--deny`.
    pub fn denied(&self) -> &[PathBuf] {
        &self.denied
    }

    /// Sets the host paths that the export hides inside the shares to `denied`, in place of the
    /// earlier ones.
    pub fn set_denied(&mut self, denied: Vec<PathBuf>) {
        self.denied = denied;
    }

    /// Adds `path` after the other host paths that the export hides, as `--deny` does.
    pub fn add_denied(&mut self, path: impl Into<PathBuf>) {
        self.denied.push(path.into());
    }

    /// Returns the host paths inside hidden ones that the export shows again: `--allow`.
    pub fn allowed(&self) -> &[PathBuf] {
        &self.allowed
    }

    /// Sets the host paths inside hidden ones that the export shows again to `allowed`, in place
    /// of the earlier ones.
    pub fn set_allowed(&mut self, allowed: Vec<PathBuf>) {
        self.allowed = allowed;
    }

    /// Adds `path` after the other host paths that the export shows again, as `--allow` does.
    pub fn add_allowed(&mut self, path: impl Into<PathBuf>) {
        self.allowed.push(path.into());
    }

    /// Returns the absolute guest paths of the trees that the adapter keeps for itself, beside
    /// those that the container root keeps: `--reserved`.
    pub fn reserved(&self) -> &[String] {
        &self.reserved
    }

    /// Sets the guest trees that the adapter keeps for itself to `reserved`, in place of the
    /// earlier ones.
    pub fn set_reserved(&mut self, reserved: Vec<String>) {
        self.reserved = reserved;
    }

    /// Adds `target` after the other guest trees that the adapter keeps, as `--reserved` does.
    pub fn add_reserved(&mut self, target: impl Into<String>) {
        self.reserved.push(target.into());
    }

    /// Returns the host directories that the adapter authorizes, inside which each share must
    /// lie: `--share-root`.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Sets the host directories that the adapter authorizes to `roots`, in place of the earlier
    /// ones.
    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
    }

    /// Adds `root` after the other host directories that the adapter authorizes, as
    /// `--share-root` does.
    pub fn add_root(&mut self, root: impl Into<PathBuf>) {
        self.roots.push(root.into());
    }

    /// Plans the export of the shares with NVX's `filesystem::plan`, or returns `None` without
    /// shares.
    ///
    /// Each share must lie inside one of the [`roots`](Self::roots) once links are resolved, which
    /// this checks before it plans, and again of the directories and files that the plan exports,
    /// which `filesystem::plan` resolves anew. The plan maps each share at its target, read-only or
    /// read-write, hides the [`denied`](Self::denied) paths inside the shares, but for the
    /// [`allowed`](Self::allowed) paths inside them, which stay visible, with the directories on
    /// the way to them listing only them, and refuses targets that the container root keeps,
    /// `ReservedTargets::CONTAINER_ROOT`, or that would take or cover one of the
    /// [`reserved`](Self::reserved) trees.
    ///
    /// Fails with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) for a reserved tree that
    /// is not an absolute and canonical guest path, a root that is not an accessible directory, a
    /// share that is not accessible or lies outside every root, and as `filesystem::plan` refuses
    /// the request, as for a target that the root keeps, or a read-only share inside a read-write
    /// one.
    pub fn plan(&self) -> Result<Option<ExportPlan>> {
        for target in &self.reserved {
            if let Some(problem) = path_problem(target) {
                return Err(Error::unsupported(format!(
                    "--reserved entry {target:?} {problem}"
                )));
            }
        }
        if self.shares.is_empty() {
            return Ok(None);
        }
        let roots = self
            .roots
            .iter()
            .map(|root| {
                let canonical = resolve("--share-root", root)?;
                if !canonical.is_dir() {
                    return Err(Error::unsupported(format!(
                        "--share-root entry {} is not a directory",
                        root.display()
                    )));
                }
                Ok(canonical)
            })
            .collect::<Result<Vec<_>>>()?;
        for share in &self.shares {
            authorize(&roots, &share.host)?;
        }
        let mappings = self
            .shares
            .iter()
            .map(|share| Mapping {
                name: "--share",
                path: &share.host,
                target: Target::Guest(&share.target),
                read_only: share.read_only,
            })
            .collect::<Vec<_>>();
        let container = ReservedTargets::CONTAINER_ROOT;
        let trees = container
            .trees
            .iter()
            .copied()
            .chain(self.reserved.iter().map(String::as_str))
            .collect::<Vec<_>>();
        let mut request = ExportRequest::new(
            &mappings,
            ReservedTargets {
                trees: &trees,
                exact: container.exact,
            },
        );
        request.denied = PathList {
            name: "--deny",
            paths: &self.denied,
        };
        request.allowed = PathList {
            name: "--allow",
            paths: &self.allowed,
        };
        let export = filesystem::plan(&request)?;
        for child in export.iter().flat_map(|export| &export.children) {
            authorize(&roots, &child.root)?;
        }
        Ok(export)
    }
}

/// Resolves the links of `path`, an entry of the option `name`, with `fs::canonicalize`.
///
/// Fails with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) for a path that does not
/// resolve, as `filesystem::plan` does.
fn resolve(name: &str, path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| {
        Error::unsupported(format!("{name} entry {} is not accessible", path.display()))
            .with_source(error)
    })
}

/// Checks that the share at `path` lies inside one of `roots`, the authorized host directories,
/// resolved as [`resolve`] resolves it.
///
/// Fails with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) for a share that does not
/// resolve, or lies outside every root.
fn authorize(roots: &[PathBuf], path: &Path) -> Result<()> {
    let canonical = resolve("--share", path)?;
    if roots.iter().any(|root| canonical.starts_with(root)) {
        return Ok(());
    }
    Err(Error::unsupported(format!(
        "--share entry {} lies outside the host directories that --share-root authorizes",
        path.display()
    )))
}
