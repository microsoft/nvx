//! Live virtio-fs shares of host directories and files inside the sandbox's container rootfs.
//!
//! OpenVMM attaches one virtio-fs device, with the tag [`MOUNT_TAG`]. One directory share uses
//! it directly, and the guest mounts it at its target. Several shares, or a file, share it as the
//! children of an aggregate, each named by [`aggregate_child_name`], which the guest mounts at
//! [`AGGREGATE_MOUNT_TARGET`], a directory that only the guest's root can enter, before it binds
//! each child at its target, as the `nvx_share=` kernel parameters of [`aggregate_share_tokens`]
//! tell it. [`protocol::share`](crate::protocol::share) writes those parameters, and the ones
//! that OpenVMM appends, and reads them all in the guest.
//!
//! A share's access-policy paths are host paths: `--mount-deny` hides a path, `--mount-allow`
//! exposes a path inside a denied path again, and `--mount-write` makes a path one of the only
//! writable parts of a read-write share. OpenVMM resolves relative paths in the only share, and
//! attributes absolute paths to the child that contains them.

use std::fmt;
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::refused;
use crate::error::{Error, Result};
use crate::host::filesystem::within;
use crate::host::python::builtins::{is_space, repr};
use crate::host::python::fs;
use crate::host::python::path::{Flavor, PurePath};
use crate::protocol::share::{self, RESERVED_TARGET_CHARACTERS, Reservation};

pub use crate::protocol::share::{
    AGGREGATE_MOUNT_TARGET, RESERVED_EXACT_MOUNT_TARGETS, RESERVED_MOUNT_TARGETS,
};

/// The tag of OpenVMM's virtio-fs device.
pub const MOUNT_TAG: &str = "microvm";
/// The most denied, allowed, and writable paths each that OpenVMM accepts for a share.
pub const MAX_MOUNT_POLICY_PATHS: usize = 128;
/// The longest guest target, in UTF-8 bytes.
pub const MAX_MOUNT_TARGET_BYTES: usize = 4096;

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
                "unsupported sandbox mount {setting} {}; choose {first} or {second}",
                repr(name)
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
                "unsupported --mount mode {}; choose ro or rw (if HOST_PATH contains commas, end \
                 the value with ,ro or ,rw)",
                repr(access)
            )));
        }
    };
    Ok((target.to_owned(), host_path.to_owned(), access))
}

/// Returns whether `target` holds a character that no guest target may: Python whitespace, or
/// one of the [`RESERVED_TARGET_CHARACTERS`], NUL, `\`, `=`, and `,`.
pub(crate) fn has_forbidden_character(target: &str) -> bool {
    target
        .chars()
        .any(|character| is_space(character) || RESERVED_TARGET_CHARACTERS.contains(character))
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
/// whitespace, `\`, `=`, or `,`, and outside the [`RESERVED_MOUNT_TARGETS`] trees and the
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
            "invalid sandbox mount target {}: expected an absolute non-root path without empty, \
             dot, or parent components, whitespace, '\\', '=', or ','",
            repr(target)
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

/// Returns the error for an `OSError` that an operation on `path` raised.
pub(crate) fn os_error(error: &io::Error, path: Option<&str>) -> Error {
    Error::io(fs::os_error_message(error, path))
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

    /// Checks a share's fields in the order in which Python's `__post_init__` checks them.
    fn checked(self) -> Result<Self> {
        validate_mount_target(&self.guest_target)?;
        let raw_path = self.host_path.as_str();
        if raw_path.contains('\0') {
            return Err(refused(format!(
                "sandbox mount host path contains a NUL character: {}",
                repr(raw_path)
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
        match self.kind {
            MountKind::File if fs::is_symlink(host_path) || !fs::is_file(host_path) => {
                return Err(refused(format!(
                    "sandbox mount host path is not a regular file: {}",
                    self.host_path
                )));
            }
            MountKind::Directory if fs::is_symlink(host_path) || !fs::is_dir(host_path) => {
                return Err(refused(format!(
                    "sandbox mount host path is not a plain directory: {}",
                    self.host_path
                )));
            }
            _ => {}
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

    /// Returns the share with its host path made absolute by joining it to the current
    /// directory, without normalizing it, so OpenVMM still sees, and refuses, every
    /// symbolic-link component of the path that was requested.
    ///
    /// Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) if a relative host path meets an
    /// unreadable current directory.
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
            host_path: directory.join(self.host_path.as_str()),
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
        Ok(POLICY_KINDS
            .iter()
            .map(|&kind| {
                let paths = self
                    .policy
                    .paths(kind)
                    .iter()
                    .map(|path| {
                        if PurePath::new(flavor, path).is_absolute() {
                            path.clone()
                        } else {
                            host_path.join(path).as_str().to_owned()
                        }
                    })
                    .collect();
                (kind, paths)
            })
            .collect())
    }

    /// Returns the bootstrap tokens that OpenVMM appends to the kernel command line for this
    /// share alone: its target, the device tag, and its mode.
    pub fn command_line_fragment(&self) -> String {
        command_line_fragment(&self.guest_target, self.access, false)
    }
}

/// Returns the bootstrap tokens, each after a space, of a share at `target` with `access`, which
/// is an aggregate if `aggregate` is.
fn command_line_fragment(target: &str, access: MountAccess, aggregate: bool) -> String {
    let read_only = access == MountAccess::ReadOnly;
    format!(
        " {}",
        share::share_tokens(target, MOUNT_TAG, read_only, aggregate)
    )
}

/// Returns whether `mounts` share the virtio-fs device as the children of an aggregate: several
/// shares do, and so does a file, because the guest mounts the device's root, which is a
/// directory.
pub fn mounts_use_aggregate(mounts: &[SandboxMount]) -> bool {
    mounts.len() > 1 || mounts.iter().any(|mount| mount.kind == MountKind::File)
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
/// but not a file beside it. With several shares, each policy path must name a path inside the
/// host directory of its share, and only a denied path may name the share itself. One share has
/// nothing to compare.
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
    if mounts.len() < 2 {
        return Ok(());
    }
    let roots = mounts
        .iter()
        .map(|mount| {
            (host.resolve)(&mount.host_path)
                .map_err(|error| os_error(&error, Some(mount.host_path.as_str())))
        })
        .collect::<Result<Vec<_>>>()?;
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
                return Err(refused(format!(
                    "{option} {path} is not inside the host directory of --mount {}; with several \
                     shares, each {option} follows the --mount whose directory it names",
                    mount.guest_target
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

/// Returns the OpenVMM arguments that attach `mounts`: one directory share directly, and
/// several shares, or a file, as the children of an aggregate, in order. Then come the
/// access-policy paths of each share, absolute for an aggregate, and the owner unless it is
/// `vmm`.
///
/// Fails as [`SandboxMount::absolute`] does.
pub fn mounts_openvmm_arguments(mounts: &[SandboxMount]) -> Result<Vec<String>> {
    mounts_openvmm_arguments_with(mounts, &fs::cwd)
}

/// Implements [`mounts_openvmm_arguments`] with `cwd` as `Path.cwd()`.
pub(crate) fn mounts_openvmm_arguments_with(
    mounts: &[SandboxMount],
    cwd: &dyn Fn() -> io::Result<PurePath>,
) -> Result<Vec<String>> {
    let mut arguments = Vec::new();
    let aggregate = mounts_use_aggregate(mounts);
    match mounts {
        [] => {}
        [mount] if !aggregate => arguments.extend([
            "--mount".to_owned(),
            mount_argument(&mount.guest_target, mount.host_path.as_str(), mount.access),
        ]),
        _ => {
            arguments.extend([
                "--mount-aggregate".to_owned(),
                AGGREGATE_MOUNT_TARGET.to_owned(),
            ]);
            for (index, mount) in mounts.iter().enumerate() {
                let name = aggregate_child_name(index, &mount.guest_target);
                arguments.push("--mount-child".to_owned());
                arguments.push(aggregate_child_argument(
                    &name,
                    mount.host_path.as_str(),
                    mount.access,
                    mount.kind,
                ));
            }
        }
    }
    for mount in mounts {
        let policy = if aggregate {
            mount.absolute_policy_paths_with(cwd)?
        } else {
            POLICY_KINDS
                .iter()
                .map(|&kind| (kind, mount.policy.paths(kind).to_vec()))
                .collect()
        };
        for (kind, paths) in policy {
            for path in paths {
                arguments.push(kind.option().to_owned());
                arguments.push(path);
            }
        }
    }
    if let Some(first) = mounts.first()
        && first.owner != MountOwner::Vmm
    {
        arguments.extend(["--mount-owner".to_owned(), first.owner.as_str().to_owned()]);
    }
    Ok(arguments)
}

/// Returns the kernel command-line tokens that tell the guest where to bind each child of an
/// aggregate, given each share's guest target, access, and kind. A file child's token ends in
/// `,file`, so the guest binds it at a file.
pub fn aggregate_share_tokens(shares: &[(&str, MountAccess, MountKind)]) -> Vec<String> {
    shares
        .iter()
        .enumerate()
        .map(|(index, &(target, access, kind))| {
            share::child_token(
                &aggregate_child_name(index, target),
                target,
                access == MountAccess::ReadOnly,
                kind == MountKind::File,
            )
        })
        .collect()
}

/// Returns the bootstrap tokens that OpenVMM appends for an aggregate of shares with
/// `accesses`, which it mounts read-write if any child is.
pub fn aggregate_command_line_fragment(accesses: &[MountAccess]) -> String {
    let access = if accesses.contains(&MountAccess::ReadWrite) {
        MountAccess::ReadWrite
    } else {
        MountAccess::ReadOnly
    };
    command_line_fragment(AGGREGATE_MOUNT_TARGET, access, true)
}

/// Returns the bootstrap tokens that OpenVMM appends for `mounts`.
pub fn mounts_command_line_fragment(mounts: &[SandboxMount]) -> String {
    match mounts {
        [] => String::new(),
        [mount] if !mounts_use_aggregate(mounts) => mount.command_line_fragment(),
        _ => aggregate_command_line_fragment(
            &mounts.iter().map(|mount| mount.access).collect::<Vec<_>>(),
        ),
    }
}

#[cfg(test)]
mod tests;
