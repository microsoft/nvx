//! Live virtio-fs shares of host directories and files inside the sandbox's container rootfs.
//!
//! A [`SandboxMount`] is one `--mount` request: a host directory or file at a guest target, with
//! its access mode, owner, and access-policy paths. [`plan_mounts`] plans the shares of a sandbox
//! through the planner of every NVX caller, [`filesystem::plan`], for the container root, whose
//! runtime keeps the [`RESERVED_MOUNT_TARGETS`] and the [`RESERVED_EXACT_MOUNT_TARGETS`], and the
//! sandbox records the resulting [`ExportPlan`].
//!
//! OpenVMM attaches one virtio-fs device, with the tag [`MOUNT_TAG`], which exports the plan at
//! [`GUEST_EXPORT`], from which the guest maps it as the plan's mapping table orders.
//!
//! A share's access-policy paths are host paths: `--mount-deny` hides a path, `--mount-allow`
//! exposes a path inside a denied path again, and `--mount-write` makes a path one of the only
//! writable parts of a read-write share. A relative path names a path in its share, and each
//! must lie in the host directory of its share.
//!
//! [`filesystem::plan`]: crate::host::filesystem::plan
//! [`ExportPlan`]: crate::host::filesystem::ExportPlan
//! [`GUEST_EXPORT`]: crate::host::filesystem::GUEST_EXPORT

mod aggregate;
mod boot;
mod host_paths;
mod request;

use std::fmt;

use super::refused;
use crate::error::Result;

pub use crate::protocol::share::{RESERVED_EXACT_MOUNT_TARGETS, RESERVED_MOUNT_TARGETS};

pub use self::aggregate::plan_mounts;
pub use self::host_paths::validate_mount_host_paths;
#[cfg_attr(
    not(feature = "cli"),
    allow(
        unused_imports,
        reason = "only the CLI's sandboxes check their shares again"
    )
)]
pub(crate) use self::host_paths::{HostQueries, check_distinct_export_roots};
pub use self::request::{
    SandboxMount, mount_kind, parse_mount_value, require_mount_owner_supported,
    validate_mount_target, validate_mounts,
};
pub(crate) use self::request::{
    has_forbidden_character, has_irregular_component, os_error, targets_overlap,
};

/// The tag of OpenVMM's virtio-fs device.
pub const MOUNT_TAG: &str = "microvm";
/// The guest directory at which the guest mounts the aggregate of several shares.
pub const AGGREGATE_MOUNT_TARGET: &str = "/run/nvx/shares";
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

#[cfg(test)]
pub(super) mod tests;
