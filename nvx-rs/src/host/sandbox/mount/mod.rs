//! Live virtio-fs shares of host directories and files inside the sandbox's container rootfs.
//!
//! A [`SandboxMount`] is one `--mount` request: a host directory or file at a guest target, with
//! its access mode, owner, and access-policy paths. [`plan_mounts`] plans the shares of a sandbox
//! through the planner of every NVX caller, [`filesystem::plan`], for the container root, whose
//! runtime keeps the [`RESERVED_MOUNT_TARGETS`] and the [`RESERVED_EXACT_MOUNT_TARGETS`], and the
//! sandbox records the resulting [`ExportPlan`].
//!
//! OpenVMM attaches one virtio-fs device, with the tag [`MOUNT_TAG`], which exports the plan at
//! [`GUEST_EXPORT`], from which the guest maps it as the plan's mapping table orders. The tag,
//! which a snapshot's key names, is the one part of the shares that the crate uses outside the
//! CLI; the rest is the CLI's (feature `cli`).
//!
//! A share's access-policy paths are host paths: `--mount-deny` hides a path, `--mount-allow`
//! exposes a path inside a denied path again, and `--mount-write` makes a path one of the only
//! writable parts of a read-write share. A relative path names a path in its share, and each
//! must lie in the host directory of its share.
//!
//! [`filesystem::plan`]: crate::host::filesystem::plan
//! [`ExportPlan`]: crate::host::filesystem::ExportPlan
//! [`GUEST_EXPORT`]: crate::host::filesystem::GUEST_EXPORT
//! [`RESERVED_MOUNT_TARGETS`]: crate::protocol::share::RESERVED_MOUNT_TARGETS
//! [`RESERVED_EXACT_MOUNT_TARGETS`]: crate::protocol::share::RESERVED_EXACT_MOUNT_TARGETS

#[cfg(feature = "cli")]
mod aggregate;
#[cfg(feature = "cli")]
mod boot;
#[cfg(feature = "cli")]
mod host_paths;
mod request;

#[cfg(feature = "cli")]
use std::fmt;

#[cfg(feature = "cli")]
pub(crate) use self::aggregate::plan_mounts;
#[cfg(feature = "cli")]
pub(crate) use self::host_paths::{
    HostQueries, check_distinct_export_roots, validate_mount_host_paths,
};
pub(crate) use self::request::os_error;
#[cfg(feature = "cli")]
pub(crate) use self::request::{
    SandboxMount, has_forbidden_character, has_irregular_component, mount_kind, parse_mount_value,
    require_mount_owner_supported, targets_overlap, validate_mounts,
};

/// The tag of OpenVMM's virtio-fs device.
pub(crate) const MOUNT_TAG: &str = "microvm";
/// The guest directory at which the guest mounts the aggregate of several shares.
#[cfg(feature = "cli")]
pub(crate) const AGGREGATE_MOUNT_TARGET: &str = "/run/nvx/shares";
/// The most denied, allowed, and writable paths each that OpenVMM accepts for a share.
#[cfg(feature = "cli")]
pub(crate) const MAX_MOUNT_POLICY_PATHS: usize = 128;
/// The longest guest target, in UTF-8 bytes.
#[cfg(feature = "cli")]
pub(crate) const MAX_MOUNT_TARGET_BYTES: usize = 4096;

/// The access mode of a share.
#[cfg(feature = "cli")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MountAccess {
    /// `ro`, the default.
    ReadOnly,
    /// `rw`.
    ReadWrite,
}

#[cfg(feature = "cli")]
impl MountAccess {
    /// Returns the mode's name: `ro` or `rw`.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }
}

#[cfg(feature = "cli")]
impl fmt::Display for MountAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The host identity of a share's file operations.
#[cfg(feature = "cli")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MountOwner {
    /// `vmm`, the default: every operation runs as OpenVMM.
    Vmm,
    /// `caller`: each operation runs as the guest caller's identity, with guest root squashed to
    /// the owner of the shared directory or file. Linux hosts only.
    Caller,
}

#[cfg(feature = "cli")]
impl MountOwner {
    /// Returns the owner's name: `vmm` or `caller`.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Vmm => "vmm",
            Self::Caller => "caller",
        }
    }
}

#[cfg(feature = "cli")]
impl fmt::Display for MountOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a share binds: a host directory, or one regular host file without its directory, which
/// only the child of an aggregate can share.
#[cfg(feature = "cli")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MountKind {
    /// `directory`, the default.
    Directory,
    /// `file`.
    File,
}

#[cfg(feature = "cli")]
impl MountKind {
    /// Returns the kind's name: `directory` or `file`.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
        }
    }
}

#[cfg(feature = "cli")]
impl fmt::Display for MountKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A kind of access-policy path.
#[cfg(feature = "cli")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PolicyKind {
    /// A path that the share hides: `--mount-deny`.
    Denied,
    /// A path inside a denied path that the share exposes again: `--mount-allow`.
    Allowed,
    /// One of the only writable paths of a read-write share: `--mount-write`.
    Writable,
}

/// The kinds of access-policy paths, in the order in which OpenVMM's options carry them.
#[cfg(feature = "cli")]
pub(crate) const POLICY_KINDS: [PolicyKind; 3] = [
    PolicyKind::Denied,
    PolicyKind::Allowed,
    PolicyKind::Writable,
];

#[cfg(feature = "cli")]
impl PolicyKind {
    /// Returns the kind's name in messages: `denied`, `allowed`, or `writable`.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Allowed => "allowed",
            Self::Writable => "writable",
        }
    }

    /// Returns the option that requests a path of this kind: `--mount-deny`, `--mount-allow`, or
    /// `--mount-write`.
    pub(crate) const fn option(self) -> &'static str {
        match self {
            Self::Denied => "--mount-deny",
            Self::Allowed => "--mount-allow",
            Self::Writable => "--mount-write",
        }
    }
}

#[cfg(feature = "cli")]
impl fmt::Display for PolicyKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The access-policy paths of a share, as given: host paths, relative to the share or absolute.
#[cfg(feature = "cli")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MountPolicy {
    /// Paths that the share hides.
    pub(crate) denied: Vec<String>,
    /// Paths inside denied paths that the share exposes again.
    pub(crate) allowed: Vec<String>,
    /// The only paths of a read-write share that the workload can modify, if any.
    pub(crate) writable: Vec<String>,
}

#[cfg(feature = "cli")]
impl MountPolicy {
    /// Returns the paths of `kind`.
    pub(crate) fn paths(&self, kind: PolicyKind) -> &[String] {
        match kind {
            PolicyKind::Denied => &self.denied,
            PolicyKind::Allowed => &self.allowed,
            PolicyKind::Writable => &self.writable,
        }
    }

    /// Returns whether the policy has no paths.
    pub(crate) fn is_empty(&self) -> bool {
        POLICY_KINDS.iter().all(|&kind| self.paths(kind).is_empty())
    }
}

#[cfg(all(test, feature = "cli"))]
pub(super) mod tests;
