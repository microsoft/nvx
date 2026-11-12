//! The requests of shares: the parsing and checks of `--mount` values and guest targets, and
//! [`SandboxMount`], one share with its access, owner, kind, and access-policy paths.

use std::io;
use std::path::Path;
#[cfg(feature = "cli")]
use std::path::{Component, PathBuf};

#[cfg(feature = "cli")]
use super::{
    MAX_MOUNT_POLICY_PATHS, MAX_MOUNT_TARGET_BYTES, MountAccess, MountKind, MountOwner,
    MountPolicy, POLICY_KINDS, PolicyKind,
};
use crate::error::Error;
#[cfg(feature = "cli")]
use crate::error::Result;
#[cfg(feature = "cli")]
use crate::host::filesystem::within;
#[cfg(feature = "cli")]
use crate::host::paths;
#[cfg(feature = "cli")]
use crate::host::sandbox::{has_kernel_separator, refused};
#[cfg(feature = "cli")]
use crate::protocol::share::{self, RESERVED_TARGET_CHARACTERS, Reservation};

/// Splits a `--mount` value, `GUEST_TARGET,HOST_PATH[,ro|rw]`, into its guest target, host path,
/// and access mode, which defaults to `ro`.
///
/// The guest target ends at the first comma, and with another comma the mode follows the last
/// one, so a host path may contain commas when the mode is given. OpenVMM reads its `--mount`
/// and `--mount-child` values the same way. Fails with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
#[cfg(feature = "cli")]
pub(crate) fn parse_mount_value(value: &str) -> Result<(String, String, MountAccess)> {
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
/// [`RESERVED_TARGET_CHARACTERS`], NUL, `\`, `=`, `,`, and `"`.
#[cfg(feature = "cli")]
pub(crate) fn has_forbidden_character(target: &str) -> bool {
    has_kernel_separator(target)
        || target
            .chars()
            .any(|character| RESERVED_TARGET_CHARACTERS.contains(character))
}

/// Returns whether a guest target has an empty, `.`, or `..` component after its leading slash.
#[cfg(feature = "cli")]
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
///
/// [`RESERVED_MOUNT_TARGETS`]: crate::protocol::share::RESERVED_MOUNT_TARGETS
/// [`RESERVED_EXACT_MOUNT_TARGETS`]: crate::protocol::share::RESERVED_EXACT_MOUNT_TARGETS
#[cfg(feature = "cli")]
pub(crate) fn validate_mount_target(target: &str) -> Result<()> {
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
#[cfg(feature = "cli")]
pub(crate) fn require_mount_owner_supported(owner: MountOwner) -> Result<()> {
    require_mount_owner_supported_on(owner, cfg!(windows))
}

/// Implements [`require_mount_owner_supported`] for a Windows host or another.
#[cfg(feature = "cli")]
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
#[cfg(feature = "cli")]
pub(crate) fn targets_overlap(left: &str, right: &str) -> bool {
    within(left, right) || within(right, left)
}

/// Returns the kind of share that `host_path` requests: a file for a regular file, and
/// otherwise a directory, which [`SandboxMount::validated`] then requires.
#[cfg(feature = "cli")]
pub(crate) fn mount_kind(host_path: &Path) -> MountKind {
    if host_path.is_file() {
        MountKind::File
    } else {
        MountKind::Directory
    }
}

/// Returns a native I/O error with its path, when available.
pub(crate) fn os_error(error: &io::Error, path: Option<&Path>) -> Error {
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
#[cfg(feature = "cli")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SandboxMount {
    /// The guest directory or file at which the share appears.
    pub(super) guest_target: String,
    /// The host directory or file, with its components normalized.
    pub(super) host_path: PathBuf,
    /// The access mode.
    pub(super) access: MountAccess,
    /// The access-policy paths.
    policy: MountPolicy,
    /// The identity of the share's file operations.
    pub(super) owner: MountOwner,
    /// Whether the share binds a directory or a file.
    pub(super) kind: MountKind,
}

#[cfg(feature = "cli")]
impl SandboxMount {
    /// Returns the share of `host_path` at `guest_target`, with `host_path` spelled with its
    /// components alone, as [`Path::components`] reads them.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a guest target
    /// that [`validate_mount_target`] refuses, a host path with a NUL character or a `..`
    /// component, policy paths for a file, more than [`MAX_MOUNT_POLICY_PATHS`] policy paths of a
    /// kind, empty or repeated ones, allowed paths without denied paths, and writable paths of a
    /// read-only share.
    pub(crate) fn new(
        guest_target: &str,
        host_path: &str,
        access: MountAccess,
        policy: MountPolicy,
        owner: MountOwner,
        kind: MountKind,
    ) -> Result<Self> {
        Self::checked(Self {
            guest_target: guest_target.to_owned(),
            host_path: paths::normalized(Path::new(host_path)),
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
    pub(crate) fn parse(
        value: &str,
        policy: MountPolicy,
        owner: MountOwner,
        kind: MountKind,
    ) -> Result<Self> {
        let (guest_target, host_path, access) = parse_mount_value(value)?;
        Self::new(&guest_target, &host_path, access, policy, owner, kind)
    }

    /// Checks the target, host path, kind, and access policy, in that order.
    fn checked(self) -> Result<Self> {
        validate_mount_target(&self.guest_target)?;
        let raw_path = self.host_path.as_os_str();
        if raw_path.as_encoded_bytes().contains(&0) {
            return Err(refused(format!(
                "sandbox mount host path contains a NUL character: {raw_path:?}",
            )));
        }
        if self
            .host_path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(refused(format!(
                "sandbox mount host path contains a parent component: {}",
                self.host_path.display()
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
    pub(crate) fn with_kind(&self, kind: MountKind) -> Result<Self> {
        Self::checked(Self {
            kind,
            ..self.clone()
        })
    }

    /// Returns the host directory or file.
    pub(crate) fn host_path(&self) -> &Path {
        &self.host_path
    }

    /// Checks the share against the host: its owner must be one that the host supports, its
    /// host path must be a regular file or a plain directory, as its kind requires, without
    /// being a symbolic link, which on Windows includes a junction, as [`Path::is_symlink`] tells
    /// them, and with the `caller` owner on Linux, it must not belong to UID 0 or GID 0, to which
    /// guest root would be squashed.
    ///
    /// The kind was fixed when the share was requested, so a directory that replaced a shared
    /// file is not shared in its place. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), or
    /// [`ErrorKind::Io`](crate::ErrorKind::Io) if the host path cannot be examined.
    pub(crate) fn validated(&self) -> Result<()> {
        require_mount_owner_supported(self.owner)?;
        let host_path = self.host_path.as_path();
        let kind_matches = match self.kind {
            MountKind::File => host_path.is_file(),
            MountKind::Directory => host_path.is_dir(),
        };
        if host_path.is_symlink() || !kind_matches {
            return Err(self.kind_refusal());
        }
        #[cfg(unix)]
        if self.owner == MountOwner::Caller {
            use std::os::unix::fs::MetadataExt;

            let metadata =
                std::fs::metadata(host_path).map_err(|error| os_error(&error, Some(host_path)))?;
            if metadata.uid() == 0 || metadata.gid() == 0 {
                return Err(refused(format!(
                    "--mount-owner caller squashes guest root to the owner of the shared {}, so \
                     {} must not be owned by UID 0 or GID 0",
                    self.kind,
                    host_path.display()
                )));
            }
        }
        Ok(())
    }

    /// Returns the error for a host path that is not of the share's kind.
    pub(super) fn kind_refusal(&self) -> Error {
        let shown = self.host_path.display();
        refused(match self.kind {
            MountKind::File => format!("sandbox mount host path is not a regular file: {shown}"),
            MountKind::Directory => {
                format!("sandbox mount host path is not a plain directory: {shown}")
            }
        })
    }

    /// Returns the share with its host path made absolute, as the one function of NVX that makes
    /// host paths absolute makes it, with `cwd` as the current directory: joined to the current
    /// directory if it is relative, without resolving it, so OpenVMM still sees, and refuses,
    /// every symbolic-link component of the path that was requested. On Windows, a path with
    /// another drive and no root, such as `D:relative`, is below the current directory of its
    /// drive.
    ///
    /// Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) if a relative host path meets an
    /// unreadable current directory, or Windows cannot qualify a drive-relative path.
    pub(crate) fn absolute_with(&self, cwd: &dyn Fn() -> io::Result<PathBuf>) -> Result<Self> {
        if self.host_path.is_absolute() {
            return Ok(self.clone());
        }
        let directory = cwd().map_err(|error| os_error(&error, None))?;
        Self::checked(Self {
            host_path: absolute_host_path(&directory, &self.host_path)?,
            ..self.clone()
        })
    }

    /// Returns each kind of access-policy path with its requested paths, relative paths joined
    /// to the absolute host path of the share, with `cwd` as the current directory, as OpenVMM
    /// attributes them to the children of an aggregate.
    ///
    /// Fails as [`absolute_with`](Self::absolute_with) does.
    pub(crate) fn absolute_policy_paths_with(
        &self,
        cwd: &dyn Fn() -> io::Result<PathBuf>,
    ) -> Result<Vec<(PolicyKind, Vec<String>)>> {
        let host_path = self.absolute_with(cwd)?.host_path;
        POLICY_KINDS
            .iter()
            .map(|&kind| {
                let paths = self
                    .policy
                    .paths(kind)
                    .iter()
                    .map(|path| {
                        if Path::new(path).is_absolute() {
                            return Ok(path.clone());
                        }
                        let joined =
                            paths::normalized(&absolute_host_path(&host_path, Path::new(path))?);
                        paths::text(&joined)
                            .map(str::to_owned)
                            .map_err(|error| os_error(&error, None))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok((kind, paths))
            })
            .collect()
    }
}

/// Returns `path` joined to `directory` without resolving links, as [`paths::absolute_in`] joins
/// them.
#[cfg(feature = "cli")]
fn absolute_host_path(directory: &Path, path: &Path) -> Result<PathBuf> {
    paths::absolute_in(directory, path).map_err(|error| os_error(&error, Some(path)))
}

/// Checks the live shares that one sandbox attaches, in child order: no target may overlap
/// another, so that no share hides another, and every share has the same owner.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
#[cfg(feature = "cli")]
pub(crate) fn validate_mounts(mounts: &[SandboxMount]) -> Result<()> {
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
