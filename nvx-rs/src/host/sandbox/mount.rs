//! Live virtio-fs shares of host directories and files inside the sandbox's container rootfs.
//!
//! A [`SandboxMount`] is one `--mount` request: a host directory or file at a guest target, with
//! its access mode, owner, and access-policy paths. [`plan_mounts`] plans the shares of a sandbox
//! through the planner of every NVX caller, [`filesystem::plan`], for the container root, whose
//! runtime keeps the [`RESERVED_MOUNT_TARGETS`] and the [`RESERVED_EXACT_MOUNT_TARGETS`], and the
//! sandbox records the resulting [`ExportPlan`].
//!
//! [`boot_shares`] renders such a plan in the form that the guest's shell reads at boot, as the
//! one-shot lifecycle delivers it. OpenVMM attaches one virtio-fs device, with the tag
//! [`MOUNT_TAG`]. One directory uses it directly, and the guest mounts it at its target. Several
//! directories or files, or a file, share it as the children of an aggregate, each named by
//! [`aggregate_child_name`], which the guest mounts at [`AGGREGATE_MOUNT_TARGET`], a directory
//! that only the guest's root can enter, before it binds each child at its target, as the
//! `nvx_share=` kernel parameters tell it. The managed lifecycle exports the same plan at
//! [`GUEST_EXPORT`] instead, from which the guest agent maps it as the plan's mapping table
//! orders; see [`SandboxPlan`](super::SandboxPlan).
//!
//! A share's access-policy paths are host paths: `--mount-deny` hides a path, `--mount-allow`
//! exposes a path inside a denied path again, and `--mount-write` makes a path one of the only
//! writable parts of a read-write share. A relative path names a path in its share, and each
//! must lie in the host directory of its share.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{has_kernel_separator, refused};
use crate::error::{Error, Result};
use crate::host::filesystem::{
    self, ExportPlan, ExportRequest, GUEST_EXPORT, Mapping, Owner, PathList, ReservedTargets,
    Target, within,
};
use crate::host::python::fs;
use crate::host::python::path::{Flavor, PurePath};
use crate::protocol::share::{self, RESERVED_TARGET_CHARACTERS, Reservation};

pub use crate::protocol::share::{RESERVED_EXACT_MOUNT_TARGETS, RESERVED_MOUNT_TARGETS};

/// The tag of OpenVMM's virtio-fs device.
pub const MOUNT_TAG: &str = "microvm";
/// The guest directory at which the guest mounts the aggregate of several shares.
pub const AGGREGATE_MOUNT_TARGET: &str = "/run/nvx/shares";
/// The most denied, allowed, and writable paths each that OpenVMM accepts for a share.
pub const MAX_MOUNT_POLICY_PATHS: usize = 128;
/// The longest guest target, in UTF-8 bytes.
pub const MAX_MOUNT_TARGET_BYTES: usize = 4096;

/// The bootstrap tokens that OpenVMM appends to the kernel command line for its share, with the
/// guest directory, the tag, and the mode.
const MOUNT_COMMAND_LINE_FRAGMENT: [&str; 3] = [" virtfs_dir=", " virtfs_tag=", " virtfs_mode="];
/// The token that OpenVMM appends after those of an aggregate.
const AGGREGATE_COMMAND_LINE_SUFFIX: &str = " virtfs_aggregate=1";

/// The access mode of a share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MountAccess {
    /// `ro`, the default.
    ReadOnly,
    /// `rw`.
    ReadWrite,
}

impl MountAccess {
    /// Returns the mode's name: `ro` or `rw`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }

    /// Returns the mode that `name` names, as a recorded share gives it.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for another name.
    pub fn from_name(name: &str) -> Result<Self> {
        setting_from_name(
            "mode",
            [Self::ReadOnly, Self::ReadWrite],
            Self::as_str,
            name,
        )
    }
}

impl fmt::Display for MountAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The host identity of a share's file operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MountOwner {
    /// `vmm`, the default: every operation runs as OpenVMM.
    Vmm,
    /// `caller`: each operation runs as the guest caller's identity, with guest root squashed to
    /// the owner of the shared directory or file. Linux hosts only.
    Caller,
}

impl MountOwner {
    /// Returns the owner's name: `vmm` or `caller`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vmm => "vmm",
            Self::Caller => "caller",
        }
    }

    /// Returns the owner that `name` names, as a recorded share gives it.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for another name.
    pub fn from_name(name: &str) -> Result<Self> {
        setting_from_name("owner", [Self::Vmm, Self::Caller], Self::as_str, name)
    }
}

impl fmt::Display for MountOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a share binds: a host directory, or one regular host file without its directory, which
/// only the child of an aggregate can share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MountKind {
    /// `directory`, the default.
    Directory,
    /// `file`.
    File,
}

impl MountKind {
    /// Returns the kind's name: `directory` or `file`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
        }
    }

    /// Returns the kind that `name` names, as a recorded share gives it.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for another name.
    pub fn from_name(name: &str) -> Result<Self> {
        setting_from_name("kind", [Self::Directory, Self::File], Self::as_str, name)
    }
}

impl fmt::Display for MountKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Returns the one of `values`, the two values of a share's `setting`, whose name, as `as_str`
/// gives it, is `name`, as a recorded share gives it.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for another name, with a
/// message that names the setting and its two values.
fn setting_from_name<T: Copy>(
    setting: &str,
    values: [T; 2],
    as_str: fn(T) -> &'static str,
    name: &str,
) -> Result<T> {
    values
        .into_iter()
        .find(|&value| as_str(value) == name)
        .ok_or_else(|| {
            let [first, second] = values.map(as_str);
            refused(format!(
                "unsupported sandbox mount {setting} {name:?}; choose {first} or {second}",
            ))
        })
}

/// A kind of access-policy path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PolicyKind {
    /// A path that the share hides: `--mount-deny`.
    Denied,
    /// A path inside a denied path that the share exposes again: `--mount-allow`.
    Allowed,
    /// One of the only writable paths of a read-write share: `--mount-write`.
    Writable,
}

/// The kinds of access-policy paths, in the order in which OpenVMM's options carry them.
pub const POLICY_KINDS: [PolicyKind; 3] = [
    PolicyKind::Denied,
    PolicyKind::Allowed,
    PolicyKind::Writable,
];

impl PolicyKind {
    /// Returns the kind's name in messages: `denied`, `allowed`, or `writable`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Allowed => "allowed",
            Self::Writable => "writable",
        }
    }

    /// Returns the option that requests a path of this kind: `--mount-deny`, `--mount-allow`, or
    /// `--mount-write`.
    pub const fn option(self) -> &'static str {
        match self {
            Self::Denied => "--mount-deny",
            Self::Allowed => "--mount-allow",
            Self::Writable => "--mount-write",
        }
    }
}

impl fmt::Display for PolicyKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The access-policy paths of a share, as given: host paths, relative to the share or absolute.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountPolicy {
    /// Paths that the share hides.
    pub denied: Vec<String>,
    /// Paths inside denied paths that the share exposes again.
    pub allowed: Vec<String>,
    /// The only paths of a read-write share that the workload can modify, if any.
    pub writable: Vec<String>,
}

impl MountPolicy {
    /// Returns the paths of `kind`.
    pub fn paths(&self, kind: PolicyKind) -> &[String] {
        match kind {
            PolicyKind::Denied => &self.denied,
            PolicyKind::Allowed => &self.allowed,
            PolicyKind::Writable => &self.writable,
        }
    }

    /// Returns whether the policy has no paths.
    pub fn is_empty(&self) -> bool {
        POLICY_KINDS.iter().all(|&kind| self.paths(kind).is_empty())
    }
}

/// Splits a `--mount` value, `GUEST_TARGET,HOST_PATH[,ro|rw]`, into its guest target, host path,
/// and access mode, which defaults to `ro`.
///
/// The guest target ends at the first comma, and with another comma the mode follows the last
/// one, so a host path may contain commas when the mode is given. OpenVMM reads its `--mount`
/// and `--mount-child` values the same way. Fails with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn parse_mount_value(value: &str) -> Result<(String, String, MountAccess)> {
    let Some((target, rest)) = value.split_once(',') else {
        return Err(refused("--mount must be GUEST_TARGET,HOST_PATH[,ro|rw]"));
    };
    let (host_path, access) = rest.rsplit_once(',').unwrap_or((rest, "ro"));
    if host_path.is_empty() {
        return Err(refused("--mount host path is empty"));
    }
    let access = match access {
        "ro" => MountAccess::ReadOnly,
        "rw" => MountAccess::ReadWrite,
        _ => {
            return Err(refused(format!(
                "unsupported --mount mode {access:?}; choose ro or rw (if HOST_PATH contains commas, end \
                 the value with ,ro or ,rw)",
            )));
        }
    };
    Ok((target.to_owned(), host_path.to_owned(), access))
}

/// Returns whether `target` cannot stay one kernel token or holds one of the
/// [`RESERVED_TARGET_CHARACTERS`], NUL, `\`, `=`, and `,`.
pub(crate) fn has_forbidden_character(target: &str) -> bool {
    has_kernel_separator(target)
        || target
            .chars()
            .any(|character| RESERVED_TARGET_CHARACTERS.contains(character))
}

/// Returns whether a guest target has an empty, `.`, or `..` component after its leading slash.
pub(crate) fn has_irregular_component(target: &str) -> bool {
    target
        .split('/')
        .skip(1)
        .any(|component| matches!(component, "" | "." | ".."))
}

/// Checks a live share's guest target inside the sandbox container rootfs: an absolute non-root
/// path of at most [`MAX_MOUNT_TARGET_BYTES`] bytes, without empty, dot, or parent components,
/// whitespace, control characters, quotes, `\`, `=`, or `,`, and outside the
/// [`RESERVED_MOUNT_TARGETS`] trees and the
/// [`RESERVED_EXACT_MOUNT_TARGETS`].
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn validate_mount_target(target: &str) -> Result<()> {
    if !target.starts_with('/')
        || target == "/"
        || target.len() > MAX_MOUNT_TARGET_BYTES
        || has_forbidden_character(target)
        || has_irregular_component(target)
    {
        return Err(refused(format!(
            "invalid sandbox mount target {target:?}: expected an absolute non-root path without empty, \
             dot, or parent components, whitespace, control characters, quotes, '\\', '=', or ','",
        )));
    }
    match share::reservation(target.as_bytes()) {
        Some(Reservation::Tree(reserved)) => Err(refused(format!(
            "sandbox mount target {target} overlaps the reserved {reserved} tree"
        ))),
        Some(Reservation::Exact) => Err(refused(format!(
            "sandbox mount target {target} is reserved"
        ))),
        None => Ok(()),
    }
}

/// Refuses an owner that this host cannot enforce: `caller` on Windows, which has no
/// per-request POSIX identity for OpenVMM to switch to.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn require_mount_owner_supported(owner: MountOwner) -> Result<()> {
    require_mount_owner_supported_on(owner, cfg!(windows))
}

/// Implements [`require_mount_owner_supported`] for a Windows host or another.
pub(crate) fn require_mount_owner_supported_on(owner: MountOwner, windows: bool) -> Result<()> {
    if owner == MountOwner::Caller && windows {
        return Err(refused(
            "--mount-owner caller requires a Linux host; Windows has no per-request POSIX \
             identity for OpenVMM to switch to",
        ));
    }
    Ok(())
}

/// Returns whether two guest targets overlap: whether they are equal, or one contains the other.
pub(crate) fn targets_overlap(left: &str, right: &str) -> bool {
    within(left, right) || within(right, left)
}

/// Returns the kind of share that `host_path` requests: a file for a regular file, and
/// otherwise a directory, which [`SandboxMount::validated`] then requires.
pub fn mount_kind(host_path: &Path) -> MountKind {
    if fs::is_file(host_path) {
        MountKind::File
    } else {
        MountKind::Directory
    }
}

/// Returns a native I/O error with its path, when available.
pub(crate) fn os_error(error: &io::Error, path: Option<&str>) -> Error {
    Error::io(match path {
        Some(path) => format!("{path:?}: {error}"),
        None => error.to_string(),
    })
}

/// A live virtio-fs share mounted inside the sandbox container rootfs.
///
/// Its denied paths hide paths in the share, its allowed paths expose paths inside denied paths
/// again, and its writable paths, when present, are the only paths of a read-write share that
/// the workload can modify. A share of a file binds one regular host file, whose mode applies to
/// the whole file, so it has none of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxMount {
    /// The guest directory or file at which the share appears.
    guest_target: String,
    /// The host directory or file, as `pathlib` normalizes it.
    host_path: PurePath,
    /// The access mode.
    access: MountAccess,
    /// The access-policy paths.
    policy: MountPolicy,
    /// The identity of the share's file operations.
    owner: MountOwner,
    /// Whether the share binds a directory or a file.
    kind: MountKind,
}

impl SandboxMount {
    /// Returns the share of `host_path` at `guest_target`, with `host_path` normalized as
    /// `pathlib.Path` normalizes it.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a guest target
    /// that [`validate_mount_target`] refuses, a host path with a NUL character or a `..`
    /// component, policy paths for a file, more than [`MAX_MOUNT_POLICY_PATHS`] policy paths of a
    /// kind, empty or repeated ones, allowed paths without denied paths, and writable paths of a
    /// read-only share.
    pub fn new(
        guest_target: &str,
        host_path: &str,
        access: MountAccess,
        policy: MountPolicy,
        owner: MountOwner,
        kind: MountKind,
    ) -> Result<Self> {
        Self::new_in(
            Flavor::NATIVE,
            guest_target,
            host_path,
            access,
            policy,
            owner,
            kind,
        )
    }

    /// Implements [`new`](Self::new) for host paths of `flavor`.
    pub(crate) fn new_in(
        flavor: Flavor,
        guest_target: &str,
        host_path: &str,
        access: MountAccess,
        policy: MountPolicy,
        owner: MountOwner,
        kind: MountKind,
    ) -> Result<Self> {
        Self::checked(Self {
            guest_target: guest_target.to_owned(),
            host_path: PurePath::new(flavor, host_path),
            access,
            policy,
            owner,
            kind,
        })
    }

    /// Parses a `--mount` value with [`parse_mount_value`] into a share of the given policy,
    /// owner, and kind.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), as
    /// [`parse_mount_value`] and [`new`](Self::new) do.
    pub fn parse(
        value: &str,
        policy: MountPolicy,
        owner: MountOwner,
        kind: MountKind,
    ) -> Result<Self> {
        Self::parse_in(Flavor::NATIVE, value, policy, owner, kind)
    }

    /// Implements [`parse`](Self::parse) for host paths of `flavor`.
    pub(crate) fn parse_in(
        flavor: Flavor,
        value: &str,
        policy: MountPolicy,
        owner: MountOwner,
        kind: MountKind,
    ) -> Result<Self> {
        let (guest_target, host_path, access) = parse_mount_value(value)?;
        Self::new_in(
            flavor,
            &guest_target,
            &host_path,
            access,
            policy,
            owner,
            kind,
        )
    }

    /// Checks the target, host path, kind, and access policy, in that order.
    fn checked(self) -> Result<Self> {
        validate_mount_target(&self.guest_target)?;
        let raw_path = self.host_path.as_str();
        if raw_path.contains('\0') {
            return Err(refused(format!(
                "sandbox mount host path contains a NUL character: {raw_path:?}",
            )));
        }
        if self.host_path.parts().iter().any(|part| part == "..") {
            return Err(refused(format!(
                "sandbox mount host path contains a parent component: {raw_path}"
            )));
        }
        if self.kind == MountKind::File && !self.policy.is_empty() {
            return Err(refused(format!(
                "--mount-deny, --mount-allow, and --mount-write name paths inside a shared \
                 directory, but --mount {} shares a file, whose mode applies to the whole file",
                self.guest_target
            )));
        }
        for kind in POLICY_KINDS {
            let paths = self.policy.paths(kind);
            if paths.len() > MAX_MOUNT_POLICY_PATHS {
                return Err(refused(format!(
                    "a sandbox mount permits at most {MAX_MOUNT_POLICY_PATHS} {kind} paths"
                )));
            }
            if paths
                .iter()
                .any(|path| path.is_empty() || path.contains('\0'))
            {
                return Err(refused(format!(
                    "sandbox mount {kind} paths must be nonempty"
                )));
            }
            let unique: std::collections::HashSet<&String> = paths.iter().collect();
            if unique.len() != paths.len() {
                return Err(refused(format!(
                    "sandbox mount {kind} paths must be unique"
                )));
            }
        }
        if !self.policy.allowed.is_empty() && self.policy.denied.is_empty() {
            return Err(refused(
                "--mount-allow exposes a path inside a --mount-deny path of the same share, \
                 which has none",
            ));
        }
        if !self.policy.writable.is_empty() && self.access != MountAccess::ReadWrite {
            return Err(refused(format!(
                "--mount-write narrows the writes of a read-write share; --mount {} is read-only",
                self.guest_target
            )));
        }
        Ok(self)
    }

    /// Returns the share with another kind, checked again, as `dataclasses.replace` does.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a file share
    /// with policy paths.
    pub fn with_kind(&self, kind: MountKind) -> Result<Self> {
        Self::checked(Self {
            kind,
            ..self.clone()
        })
    }

    /// Returns the guest target.
    pub fn guest_target(&self) -> &str {
        &self.guest_target
    }

    /// Returns the host directory or file.
    pub fn host_path(&self) -> &Path {
        self.host_path.as_path()
    }

    /// Returns the access mode.
    pub fn access(&self) -> MountAccess {
        self.access
    }

    /// Returns the access-policy paths.
    pub fn policy(&self) -> &MountPolicy {
        &self.policy
    }

    /// Returns the identity of the share's file operations.
    pub fn owner(&self) -> MountOwner {
        self.owner
    }

    /// Returns whether the share binds a directory or a file.
    pub fn kind(&self) -> MountKind {
        self.kind
    }

    /// Checks the share against the host: its owner must be one that the host supports, its
    /// host path must be a regular file or a plain directory, as its kind requires, without
    /// being a symbolic link, and with the `caller` owner on Linux, it must not belong to UID 0
    /// or GID 0, to which guest root would be squashed.
    ///
    /// The kind was fixed when the share was requested, so a directory that replaced a shared
    /// file is not shared in its place. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), or
    /// [`ErrorKind::Io`](crate::ErrorKind::Io) if the host path cannot be examined.
    pub fn validated(&self) -> Result<()> {
        require_mount_owner_supported(self.owner)?;
        let host_path = self.host_path.as_path();
        let kind_matches = match self.kind {
            MountKind::File => fs::is_file(host_path),
            MountKind::Directory => fs::is_dir(host_path),
        };
        if fs::is_symlink(host_path) || !kind_matches {
            return Err(self.kind_refusal());
        }
        if self.owner == MountOwner::Caller && !cfg!(windows) {
            let status = fs::stat(host_path)
                .map_err(|error| os_error(&error, Some(self.host_path.as_str())))?;
            if status.uid == 0 || status.gid == 0 {
                return Err(refused(format!(
                    "--mount-owner caller squashes guest root to the owner of the shared {}, so \
                     {} must not be owned by UID 0 or GID 0",
                    self.kind, self.host_path
                )));
            }
        }
        Ok(())
    }

    /// Returns the error for a host path that is not of the share's kind.
    fn kind_refusal(&self) -> Error {
        refused(match self.kind {
            MountKind::File => format!(
                "sandbox mount host path is not a regular file: {}",
                self.host_path
            ),
            MountKind::Directory => format!(
                "sandbox mount host path is not a plain directory: {}",
                self.host_path
            ),
        })
    }

    /// Returns the share with its host path made absolute by joining it to the current
    /// directory, without normalizing it, so OpenVMM still sees, and refuses, every
    /// symbolic-link component of the path that was requested.
    ///
    /// Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) if a relative host path meets an
    /// unreadable current directory, or Windows cannot qualify a drive-relative path.
    pub fn absolute(&self) -> Result<Self> {
        self.absolute_with(&fs::cwd)
    }

    /// Implements [`absolute`](Self::absolute) with `cwd` as `Path.cwd()`.
    pub(crate) fn absolute_with(&self, cwd: &dyn Fn() -> io::Result<PurePath>) -> Result<Self> {
        if self.host_path.is_absolute() {
            return Ok(self.clone());
        }
        let directory = cwd().map_err(|error| os_error(&error, None))?;
        Self::checked(Self {
            host_path: absolute_host_path(&directory, self.host_path.as_str())?,
            ..self.clone()
        })
    }

    /// Returns each kind of access-policy path with its requested paths, relative paths joined
    /// to the absolute host path of the share, as OpenVMM attributes them to the children of an
    /// aggregate.
    ///
    /// Fails as [`absolute`](Self::absolute) does.
    pub fn absolute_policy_paths(&self) -> Result<Vec<(PolicyKind, Vec<String>)>> {
        self.absolute_policy_paths_with(&fs::cwd)
    }

    /// Implements [`absolute_policy_paths`](Self::absolute_policy_paths) with `cwd` as
    /// `Path.cwd()`.
    pub(crate) fn absolute_policy_paths_with(
        &self,
        cwd: &dyn Fn() -> io::Result<PurePath>,
    ) -> Result<Vec<(PolicyKind, Vec<String>)>> {
        let host_path = self.absolute_with(cwd)?.host_path;
        let flavor = host_path.flavor();
        POLICY_KINDS
            .iter()
            .map(|&kind| {
                let paths = self
                    .policy
                    .paths(kind)
                    .iter()
                    .map(|path| {
                        if PurePath::new(flavor, path).is_absolute() {
                            Ok(path.clone())
                        } else {
                            absolute_host_path(&host_path, path)
                                .map(|path| path.as_str().to_owned())
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok((kind, paths))
            })
            .collect()
    }
}

/// Joins a host path without resolving links. A different drive's relative path on Windows
/// needs the OS's per-drive current directory, which joining to this drive cannot supply.
fn absolute_host_path(directory: &PurePath, path: &str) -> Result<PurePath> {
    let joined = directory.join(path);
    #[cfg(windows)]
    if !joined.is_absolute() {
        let absolute = std::path::absolute(joined.as_path())
            .map_err(|error| os_error(&error, Some(joined.as_str())))?;
        let text = absolute
            .to_str()
            .ok_or_else(|| refused("absolute sandbox mount path is not valid Unicode"))?;
        return Ok(PurePath::native(text));
    }
    Ok(joined)
}

/// Returns the bootstrap tokens of a share at `target` with `access`.
fn command_line_fragment(target: &str, access: MountAccess) -> String {
    let [directory, tag, mode] = MOUNT_COMMAND_LINE_FRAGMENT;
    format!("{directory}{target}{tag}{MOUNT_TAG}{mode}{access}")
}

/// Checks the live shares that one sandbox attaches, in child order: no target may overlap
/// another, so that no share hides another, and every share has the same owner.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn validate_mounts(mounts: &[SandboxMount]) -> Result<()> {
    for (index, mount) in mounts.iter().enumerate() {
        for other in &mounts[..index] {
            if targets_overlap(&other.guest_target, &mount.guest_target) {
                return Err(refused(format!(
                    "sandbox mount targets {} and {} overlap; a share cannot hide another",
                    other.guest_target, mount.guest_target
                )));
            }
        }
    }
    if mounts.iter().any(|mount| mount.owner != mounts[0].owner) {
        return Err(refused(
            "every sandbox --mount must use the same --mount-owner",
        ));
    }
    Ok(())
}

/// The queries of the host that [`validate_mount_host_paths`] makes, which tests replace.
pub(crate) struct HostQueries<'a> {
    /// `Path.cwd()`.
    pub(crate) cwd: &'a dyn Fn() -> io::Result<PurePath>,
    /// `Path.resolve()`.
    pub(crate) resolve: &'a dyn Fn(&PurePath) -> io::Result<PurePath>,
    /// `(st_dev, st_ino)` of `Path.stat()`.
    pub(crate) identity: &'a dyn Fn(&PurePath) -> io::Result<(u64, u128)>,
}

impl HostQueries<'_> {
    /// Returns the queries of this host.
    pub(crate) fn native() -> HostQueries<'static> {
        HostQueries {
            cwd: &fs::cwd,
            resolve: &fs::resolve,
            identity: &|path| fs::stat(path.as_path()).map(|status| status.identity()),
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
pub fn validate_mount_host_paths(mounts: &[SandboxMount]) -> Result<()> {
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
                .map_err(|error| os_error(&error, Some(mount.host_path.as_str())))
        })
        .collect::<Result<Vec<_>>>()?;
    if mounts.len() > 1 {
        check_distinct_host_paths(&roots, host)?;
    }
    for (mount, root) in mounts.iter().zip(&roots) {
        for (kind, paths) in mount.absolute_policy_paths_with(host.cwd)? {
            let option = kind.option();
            for path in paths {
                let pure = PurePath::new(root.flavor(), &path);
                let resolved =
                    (host.resolve)(&pure).map_err(|error| os_error(&error, Some(pure.as_str())))?;
                // A denied root hides the share behind its allowed paths, which OpenVMM requires;
                // it accepts no other policy path at a root.
                if resolved.parents().any(|parent| parent == *root)
                    || (kind == PolicyKind::Denied && resolved == *root)
                {
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
        .map(|child| Ok(PurePath::native(path_text(&child.root)?)))
        .collect::<Result<Vec<_>>>()?;
    check_distinct_host_paths(&roots, host)
}

/// Refuses resolved share roots that overlap, by path or by the file identity of a root and its
/// ancestors.
fn check_distinct_host_paths(roots: &[PurePath], host: &HostQueries<'_>) -> Result<()> {
    // The identity of each root, then of each of its ancestors.
    let lineages = roots
        .iter()
        .map(|root| {
            std::iter::once(root.clone())
                .chain(root.parents())
                .map(|path| {
                    (host.identity)(&path).map_err(|error| os_error(&error, Some(path.as_str())))
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    for (index, root) in roots.iter().enumerate() {
        for (other_index, other) in roots[..index].iter().enumerate() {
            if root == other
                || root.parents().any(|parent| parent == *other)
                || other.parents().any(|parent| parent == *root)
            {
                return Err(refused(format!(
                    "sandbox mount host paths {other} and {root} overlap; a share could reach \
                     files of another under its access mode"
                )));
            }
            if lineages[other_index].contains(&lineages[index][0])
                || lineages[index].contains(&lineages[other_index][0])
            {
                return Err(refused(format!(
                    "sandbox mount host paths {other} and {root} name the same directory or \
                     file, or one is inside the other through a bind mount"
                )));
            }
        }
    }
    Ok(())
}

/// Returns the name of the aggregate child at `index` that the guest binds at `target`: the
/// index, a hyphen, and the first 32 hexadecimal digits of the SHA-256 digest of the target.
///
/// A snapshot records every child's name, so it pins each target, and OpenVMM refuses a restore
/// that requests other targets before the guest runs, rather than keep the snapshot's binds.
pub fn aggregate_child_name(index: usize, target: &str) -> String {
    let digest = Sha256::digest(target.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{index}-{hex}")
}

/// Returns the `--mount-child` value that attaches `host_path` as the child `name`. It always
/// gives the mode, which OpenVMM reads after the last comma or before the `file` flag, so the
/// host path may contain commas. A file child carries OpenVMM's `file` flag, so OpenVMM refuses
/// a directory that replaced the file instead of exporting all of it.
pub fn aggregate_child_argument(
    name: &str,
    host_path: &str,
    access: MountAccess,
    kind: MountKind,
) -> String {
    let file = if kind == MountKind::File { ",file" } else { "" };
    format!("{name},{host_path},{access}{file}")
}

/// Returns the `--mount` value that attaches `host_path` at `target`. It always gives the mode,
/// which OpenVMM reads after the last comma, so the host path may contain commas.
pub fn mount_argument(target: &str, host_path: &str, access: MountAccess) -> String {
    format!("{target},{host_path},{access}")
}

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
/// Fails as those checks, [`SandboxMount::absolute`], and [`filesystem::plan`] do.
pub fn plan_mounts(mounts: &[SandboxMount]) -> Result<Option<ExportPlan>> {
    validate_mounts(mounts)?;
    for mount in mounts {
        mount.validated()?;
    }
    validate_mount_host_paths(mounts)?;
    plan_mounts_with(mounts, &fs::cwd)
}

/// Plans the export of `mounts` as [`plan_mounts`] does, without checking them against the host
/// first, with `cwd` as `Path.cwd()`.
pub(crate) fn plan_mounts_with(
    mounts: &[SandboxMount],
    cwd: &dyn Fn() -> io::Result<PurePath>,
) -> Result<Option<ExportPlan>> {
    let Some(first) = mounts.first() else {
        return Ok(None);
    };
    let mut hosts = Vec::with_capacity(mounts.len());
    let (mut denied, mut allowed, mut write) = (Vec::new(), Vec::new(), Vec::new());
    for mount in mounts {
        hosts.push(mount.absolute_with(cwd)?.host_path.as_path().to_path_buf());
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
                    mount.host_path
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

/// The form in which a launch delivers an export plan at boot: OpenVMM's arguments that attach
/// its directories and files, the kernel parameters that the plan adds for them, and the
/// bootstrap tokens that OpenVMM appends to the kernel command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootShares {
    /// OpenVMM's arguments: one directory share or an aggregate with its children, in order, the
    /// access-policy paths of each, absolute, and the owner unless it is `vmm`.
    pub arguments: Vec<String>,
    /// The kernel parameters: as [`boot_shares`] renders them, the `nvx_share=` tokens of an
    /// aggregate, one for each child, in order, and none for one directory share; for a mapping
    /// table, the `nvx_maps=` token of its number of mappings.
    pub tokens: Vec<String>,
    /// The bootstrap tokens that OpenVMM appends after the kernel command line.
    pub fragment: String,
}

/// Renders `plan` in the form that the guest's shell reads at boot.
///
/// The shell binds each exported directory or file at one target, so every child of the plan
/// must be mapped exactly once, at its root, with the access with which OpenVMM exports it, at a
/// target that [`validate_mount_target`] accepts.
/// One directory then uses OpenVMM's virtio-fs device directly, and anything else shares it as
/// the children of an aggregate, named by [`aggregate_child_name`].
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a plan that the boot
/// tokens cannot express, such as one that maps a path below a share's root, rather than extend
/// what the shell reads.
pub fn boot_shares(plan: &ExportPlan) -> Result<BootShares> {
    let mut shares = Vec::with_capacity(plan.children.len());
    for (index, child) in plan.children.iter().enumerate() {
        let root = path_text(&child.root)?;
        let binds: Vec<_> = plan
            .binds
            .iter()
            .filter(|bind| bind.child == index)
            .collect();
        let bind = match binds[..] {
            [bind] if bind.source.is_empty() => bind,
            [bind] => {
                return Err(refused(format!(
                    "the boot tokens of a share cannot map {}, a path below the share's root {root}",
                    child.root.join(&bind.source).display()
                )));
            }
            _ => {
                return Err(refused(format!(
                    "the boot tokens of a share map {root} at one target, not {}",
                    binds.len()
                )));
            }
        };
        validate_mount_target(&bind.target)?;
        // The tokens mount a share with the access with which OpenVMM exports it, so a bind of
        // another access, such as a read-only bind of a directory whose writes OpenVMM narrows to
        // a path inside it, would let the guest write what the plan keeps read-only.
        if bind.read_only == child.writable {
            let (bound, exported) = if child.writable {
                ("read-only", "read-write")
            } else {
                ("read-write", "read-only")
            };
            return Err(refused(format!(
                "the boot tokens of a share cannot map {root} {bound} while OpenVMM exports it \
                 {exported}"
            )));
        }
        let access = if child.writable {
            MountAccess::ReadWrite
        } else {
            MountAccess::ReadOnly
        };
        shares.push((child, root, bind.target.as_str(), access));
    }
    if plan.binds.len() != shares.len() {
        return Err(refused(
            "the boot tokens of a share cannot map a path of an unknown host directory",
        ));
    }
    let mut boot = BootShares::default();
    match shares[..] {
        [] => return Ok(boot),
        [(child, root, target, access)] if !child.file => {
            boot.arguments = vec!["--mount".to_owned(), mount_argument(target, root, access)];
            boot.fragment = command_line_fragment(target, access);
        }
        _ => {
            boot.arguments = vec![
                "--mount-aggregate".to_owned(),
                AGGREGATE_MOUNT_TARGET.to_owned(),
            ];
            for (index, &(child, root, target, access)) in shares.iter().enumerate() {
                let name = aggregate_child_name(index, target);
                let (kind, file) = if child.file {
                    (MountKind::File, ",file")
                } else {
                    (MountKind::Directory, "")
                };
                boot.arguments.push("--mount-child".to_owned());
                boot.arguments
                    .push(aggregate_child_argument(&name, root, access, kind));
                boot.tokens
                    .push(format!("nvx_share={name},{target},{access}{file}"));
            }
            let accesses: Vec<MountAccess> =
                shares.iter().map(|&(_, _, _, access)| access).collect();
            boot.fragment = aggregate_command_line_fragment(&accesses);
        }
    }
    for &(child, ..) in &shares {
        for (kind, paths) in [
            (PolicyKind::Denied, &child.denied),
            (PolicyKind::Allowed, &child.allowed),
            (PolicyKind::Writable, &child.write),
        ] {
            for path in paths {
                boot.arguments.push(kind.option().to_owned());
                boot.arguments.push(path_text(path)?.to_owned());
            }
        }
    }
    if plan.owner == Owner::Caller {
        boot.arguments.push("--mount-owner".to_owned());
        boot.arguments.push(MountOwner::Caller.as_str().to_owned());
    }
    Ok(boot)
}

/// Returns the text of a host path of an export plan, which OpenVMM's options carry.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a path that is not
/// valid UTF-8.
fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| refused(format!("{} is not valid UTF-8", path.display())))
}

/// Returns the kernel command-line tokens that tell the guest where to bind each child of an
/// aggregate, given each share's guest target, access, and kind. A file child's token ends in
/// `,file`, so the guest binds it at a file.
pub fn aggregate_share_tokens(shares: &[(&str, MountAccess, MountKind)]) -> Vec<String> {
    shares
        .iter()
        .enumerate()
        .map(|(index, &(target, access, kind))| {
            let file = if kind == MountKind::File { ",file" } else { "" };
            format!(
                "nvx_share={},{target},{access}{file}",
                aggregate_child_name(index, target)
            )
        })
        .collect()
}

/// Returns the bootstrap tokens that OpenVMM appends for an aggregate of shares with
/// `accesses`, which it mounts read-write if any child is.
pub fn aggregate_command_line_fragment(accesses: &[MountAccess]) -> String {
    aggregate_fragment(AGGREGATE_MOUNT_TARGET, accesses)
}

/// Returns the bootstrap tokens that OpenVMM appends for the export of `plan` at
/// [`GUEST_EXPORT`], the aggregate from which a managed sandbox's guest agent maps the plan's
/// mapping table.
pub(crate) fn export_command_line_fragment(plan: &ExportPlan) -> String {
    let accesses: Vec<MountAccess> = plan
        .children
        .iter()
        .map(|child| {
            if child.writable {
                MountAccess::ReadWrite
            } else {
                MountAccess::ReadOnly
            }
        })
        .collect();
    aggregate_fragment(GUEST_EXPORT, &accesses)
}

/// Returns the bootstrap tokens that OpenVMM appends for an aggregate at `target` whose children
/// have `accesses`, which it mounts read-write if any child is.
fn aggregate_fragment(target: &str, accesses: &[MountAccess]) -> String {
    let access = if accesses.contains(&MountAccess::ReadWrite) {
        MountAccess::ReadWrite
    } else {
        MountAccess::ReadOnly
    };
    command_line_fragment(target, access) + AGGREGATE_COMMAND_LINE_SUFFIX
}

#[cfg(test)]
mod tests;
