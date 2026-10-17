//! The sandbox launch profile: a microVM that runs one workload in a container over read-only
//! EROFS layers and a private ext4 scratch disk, with live virtio-fs shares.
//!
//! This is the profile of `nvx.py sandbox`, ported from `scripts/nvx_tools/sandbox.py` with its
//! limits, option semantics, OpenVMM arguments, kernel command line, and messages:
//!
//! - [`SandboxLayer`]: one to three EROFS layers, `ROLE,PATH,EROFS_UUID`, each of a
//!   [`LayerRole`], which the guest stacks in role order on its own block device.
//! - [`SandboxMount`]: a live share of a host directory or regular file at a guest target, with
//!   its access mode, owner, and access-policy paths; [`mount`] holds the rules of guest targets,
//!   the aggregate of several shares, and the checks of host paths.
//! - [`SandboxLaunch`]: the layers, the scratch disk, the entrypoint and its arguments, the
//!   hostname, the workload identity, the resource limits, and the shares, with
//!   [`openvmm_arguments`](SandboxLaunch::openvmm_arguments) and
//!   [`kernel_command_line`](SandboxLaunch::kernel_command_line).
//! - [`options`]: the checks that `nvx.py sandbox` and `nvx.py run` apply to their options: the
//!   attribution of access-policy paths to shares, the systemd policy, the targets of
//!   `nvx.py run` shares, and the managed exec environment.
//!
//! Values are checked as Python checked them: whitespace is what `str.isspace` accepts,
//! integers are what `int` reads ([`Int`]), UUIDs are what `uuid.UUID` reads, host paths are
//! normalized as `pathlib` normalizes them, with backslashes on Windows, and messages quote
//! values with Python's `repr`.
//!
//! # Errors
//!
//! A request that the profile refuses fails with
//! [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), and a missing layer or scratch
//! image with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable). A host query that fails
//! is [`ErrorKind::Io`](crate::ErrorKind::Io), with Python's form of the system's error, such as
//! `[Errno 13] Permission denied: '/path'`.

pub mod layer;
pub mod mount;
pub mod options;

use std::io;
use std::path::Path;

pub use self::layer::{LAYER_ROLES, LayerRole, SandboxLayer};
pub use self::mount::{MountAccess, MountKind, MountOwner, MountPolicy, PolicyKind, SandboxMount};
use crate::error::{Error, Result};
pub use crate::host::python::builtins::{Int, IntError};
use crate::host::python::builtins::{is_space, split, strip};
use crate::host::python::fs;
use crate::host::python::path::{Flavor, PurePath};
use crate::net::is_valid_hostname;

/// The MMIO base of the scratch disk's sandbox block device.
pub const SCRATCH_MMIO_BASE: u64 = 0xD000_6000;
/// The size of the x86 kernel command line, including its NUL.
pub const KERNEL_COMMAND_LINE_MAX_SIZE: usize = 2048;
/// The part of the kernel command line that OpenVMM keeps for its own tokens.
pub const OPENVMM_COMMAND_LINE_RESERVE: usize = 1024;
/// The part of the kernel command line that the sandbox profile may use, including the NUL and
/// the bootstrap tokens that OpenVMM appends for live shares.
pub const SANDBOX_COMMAND_LINE_MAX_SIZE: usize =
    KERNEL_COMMAND_LINE_MAX_SIZE - OPENVMM_COMMAND_LINE_RESERVE;
/// The workload identity by default, `65534:65534`.
pub const DEFAULT_WORKLOAD_IDENTITY: (u32, u32) = (65534, 65534);
/// The entrypoint by default.
pub const DEFAULT_ENTRYPOINT: &str = "/bin/sh";
/// The hostname by default.
pub const DEFAULT_HOSTNAME: &str = "nvx-sandbox";

/// Returns the error of a request that the profile refuses.
pub(crate) fn refused(message: impl Into<String>) -> Error {
    Error::unsupported(message)
}

/// Parses a `--workload-user` value, `UID:GID`, with both IDs between 1 and 4294967295.
///
/// Each ID is read as Python's `int` reads it, so a sign, underscores between digits, Unicode
/// decimal digits, and surrounding whitespace are accepted. Fails with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn parse_workload_identity(value: &str) -> Result<(u32, u32)> {
    let fields: Vec<&str> = value.split(':').collect();
    let [uid, gid] = fields[..] else {
        return Err(refused("--workload-user must be UID:GID"));
    };
    let (Ok(uid), Ok(gid)) = (Int::parse(uid), Int::parse(gid)) else {
        return Err(refused("--workload-user must contain numeric UID and GID"));
    };
    let id = |value: &Int, name: &str| {
        value.to_u32().filter(|&id| id != 0).ok_or_else(|| {
            refused(format!(
                "--workload-user {name} must be between 1 and 4294967295"
            ))
        })
    };
    Ok((id(&uid, "UID")?, id(&gid, "GID")?))
}

/// Checks that `path` is a regular file, following links, as `require_file` of `nvx.py` does.
///
/// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) and the message
/// `{description} not found: {path}`, with `path` normalized as `pathlib.Path` normalizes it.
pub fn require_file(path: &Path, description: &str) -> Result<()> {
    match path.to_str() {
        Some(text) => require_pure_file(&PurePath::native(text), description),
        None if fs::is_file(path) => Ok(()),
        None => Err(Error::unavailable(format!(
            "{description} not found: {}",
            path.display()
        ))),
    }
}

/// Refuses a disk path that OpenVMM's `--microvm-sandbox-block` value could not hold: one with
/// a comma or semicolon.
fn reject_disk_path(path: &PurePath) -> Result<()> {
    let value = path.as_str();
    if value.contains([',', ';']) {
        return Err(refused(format!(
            "disk paths containing commas or semicolons are unsupported: {value}"
        )));
    }
    Ok(())
}

/// The settings of a [`SandboxLaunch`] besides its layers and scratch disk, with the defaults of
/// `nvx.py sandbox`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxSettings {
    /// The absolute path of the workload's program in the container, without whitespace.
    pub entrypoint: String,
    /// The workload's arguments, each nonempty and without whitespace.
    pub args: Vec<String>,
    /// The workload's hostname: a lowercase RFC 1123 label of up to 63 characters.
    pub hostname: String,
    /// The workload's UID and GID, neither of them root's.
    pub workload_identity: (u32, u32),
    /// The workload's memory limit in bytes, `memory.max` of its cgroup, if positive.
    pub memory_max: Option<Int>,
    /// The most processes that the workload may run, if positive; the guest's own init counts
    /// too, so its `pids.max` is one more.
    pub pids_max: Option<Int>,
    /// The live shares, in child order.
    pub mounts: Vec<SandboxMount>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            entrypoint: DEFAULT_ENTRYPOINT.to_owned(),
            args: Vec::new(),
            hostname: DEFAULT_HOSTNAME.to_owned(),
            workload_identity: DEFAULT_WORKLOAD_IDENTITY,
            memory_max: None,
            pids_max: None,
            mounts: Vec::new(),
        }
    }
}

/// The launch of a sandbox: everything that its OpenVMM arguments and kernel command line
/// depend on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxLaunch {
    /// The read-only layers, in the order given.
    layers: Vec<SandboxLayer>,
    /// The ext4 scratch image.
    scratch: PurePath,
    /// The other settings.
    settings: SandboxSettings,
}

impl SandboxLaunch {
    /// Returns the launch of `layers` over the ext4 image at `scratch`, which is normalized as
    /// `pathlib.Path` normalizes it.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), checking in this
    /// order: one to three layers, no duplicate roles, the entrypoint, the arguments, the
    /// hostname, a non-root identity, positive limits, and shares whose targets do not overlap
    /// and that have one owner.
    pub fn new(
        layers: Vec<SandboxLayer>,
        scratch: &str,
        settings: SandboxSettings,
    ) -> Result<Self> {
        Self::new_in(Flavor::NATIVE, layers, scratch, settings)
    }

    /// Implements [`new`](Self::new) for a scratch path of `flavor`.
    pub(crate) fn new_in(
        flavor: Flavor,
        layers: Vec<SandboxLayer>,
        scratch: &str,
        settings: SandboxSettings,
    ) -> Result<Self> {
        if !(1..=LAYER_ROLES.len()).contains(&layers.len()) {
            return Err(refused("a sandbox requires one to three read-only layers"));
        }
        let mut duplicates: Vec<&str> = LAYER_ROLES
            .iter()
            .filter(|&&role| layers.iter().filter(|layer| layer.role() == role).count() > 1)
            .map(|role| role.as_str())
            .collect();
        if !duplicates.is_empty() {
            duplicates.sort_unstable();
            return Err(refused(format!(
                "duplicate sandbox layer role(s): {}",
                duplicates.join(", ")
            )));
        }
        let SandboxSettings {
            entrypoint,
            args,
            hostname,
            workload_identity: (uid, gid),
            memory_max,
            pids_max,
            mounts,
        } = &settings;
        if !entrypoint.starts_with('/') || entrypoint.chars().any(is_space) {
            return Err(refused(
                "sandbox entrypoint must be an absolute path without spaces",
            ));
        }
        if args
            .iter()
            .any(|argument| argument.is_empty() || argument.chars().any(is_space))
        {
            return Err(refused(
                "sandbox arguments must be nonempty and contain no spaces",
            ));
        }
        if !is_valid_hostname(hostname) {
            return Err(refused(
                "sandbox hostname must be a lowercase RFC 1123 label up to 63 characters",
            ));
        }
        if *uid == 0 || *gid == 0 {
            return Err(refused(
                "sandbox workload identity must be a non-root UID:GID",
            ));
        }
        for (name, value) in [("memory-max", memory_max), ("pids-max", pids_max)] {
            if value.as_ref().is_some_and(|value| !value.is_positive()) {
                return Err(refused(format!("--{name} must be positive")));
            }
        }
        mount::validate_mounts(mounts)?;
        Ok(Self {
            layers,
            scratch: PurePath::new(flavor, scratch),
            settings,
        })
    }

    /// Returns the layers, in the order given.
    pub fn layers(&self) -> &[SandboxLayer] {
        &self.layers
    }

    /// Returns the scratch image.
    pub fn scratch(&self) -> &Path {
        self.scratch.as_path()
    }

    /// Returns the settings besides the layers and the scratch image.
    pub fn settings(&self) -> &SandboxSettings {
        &self.settings
    }

    /// Returns the shares.
    pub fn mounts(&self) -> &[SandboxMount] {
        &self.settings.mounts
    }

    /// Checks the launch against the host: each layer and the scratch image must be a regular
    /// file whose path has no comma or semicolon, each share must pass
    /// [`SandboxMount::validated`], and their host paths [`mount::validate_mount_host_paths`].
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for a missing image,
    /// and otherwise as those checks do.
    pub fn validated(&self) -> Result<()> {
        self.validated_with(&mount::HostQueries::native())
    }

    /// Implements [`validated`](Self::validated) with the given host queries.
    pub(crate) fn validated_with(&self, host: &mount::HostQueries<'_>) -> Result<()> {
        for layer in &self.layers {
            require_pure_file(layer.pure_path(), &format!("{} EROFS layer", layer.role()))?;
            reject_disk_path(layer.pure_path())?;
        }
        require_pure_file(&self.scratch, "ext4 scratch image")?;
        reject_disk_path(&self.scratch)?;
        for mount in &self.settings.mounts {
            mount.validated()?;
        }
        mount::validate_mount_host_paths_with(&self.settings.mounts, host)
    }

    /// Returns the layers in role order: distro, runtime, then custom.
    pub fn ordered_layers(&self) -> Vec<&SandboxLayer> {
        LAYER_ROLES
            .iter()
            .filter_map(|&role| self.layers.iter().rev().find(|layer| layer.role() == role))
            .collect()
    }

    /// Returns the OpenVMM arguments of the sandbox profile: the microVM machine, a read-only
    /// sandbox block device for each layer in role order, the scratch device, the workload
    /// identity, and the arguments that attach the shares.
    ///
    /// Fails as [`mount::mounts_openvmm_arguments`] does.
    pub fn openvmm_arguments(&self) -> Result<Vec<String>> {
        self.openvmm_arguments_with(&fs::cwd)
    }

    /// Implements [`openvmm_arguments`](Self::openvmm_arguments) with `cwd` as `Path.cwd()`.
    pub(crate) fn openvmm_arguments_with(
        &self,
        cwd: &dyn Fn() -> io::Result<PurePath>,
    ) -> Result<Vec<String>> {
        let mut arguments = vec!["--machine".to_owned(), "microvm".to_owned()];
        for layer in self.ordered_layers() {
            arguments.push("--microvm-sandbox-block".to_owned());
            arguments.push(format!(
                "{}:file:{},ro",
                layer.role(),
                layer.pure_path().as_str()
            ));
        }
        let (uid, gid) = self.settings.workload_identity;
        arguments.extend([
            "--microvm-sandbox-block".to_owned(),
            format!("scratch:file:{}", self.scratch.as_str()),
            "--microvm-workload-identity".to_owned(),
            format!("{uid}:{gid}"),
        ]);
        arguments.extend(mount::mounts_openvmm_arguments_with(
            &self.settings.mounts,
            cwd,
        )?);
        Ok(arguments)
    }

    /// Returns the kernel command line of the sandbox: `user_command_line`, stripped of Python
    /// whitespace, then `nvx_sandbox=1`, a `nvx_layer=` token for each layer in role order,
    /// `nvx_scratch=`, `nvx_entrypoint=`, `nvx_hostname=`, a `nvx_arg=` token for each
    /// argument, `nvx_memory_max=` and `nvx_pids_max=` when set, and the `nvx_share=` tokens of
    /// an aggregate.
    ///
    /// The user's command line may hold no NUL and no `nvx_*` or `virtfs_*` token, which the
    /// profile and OpenVMM own. The command line, the bootstrap tokens that OpenVMM appends for
    /// the shares, and a NUL must fit in [`SANDBOX_COMMAND_LINE_MAX_SIZE`] bytes. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
    pub fn kernel_command_line(&self, user_command_line: &str) -> Result<String> {
        if user_command_line.contains('\0') {
            return Err(refused("kernel command line contains an embedded NUL"));
        }
        for token in split(user_command_line) {
            let name = token.split('=').next().unwrap_or(token);
            if token.starts_with("nvx_") {
                return Err(refused(format!("{name} is owned by the sandbox profile")));
            }
            if token.starts_with("virtfs_") {
                return Err(refused(format!(
                    "{name} is owned by the sandbox --mount option"
                )));
            }
        }
        let settings = &self.settings;
        let mut tokens = vec![
            strip(user_command_line).to_owned(),
            "nvx_sandbox=1".to_owned(),
        ];
        for layer in self.ordered_layers() {
            tokens.push(format!(
                "nvx_layer={},0x{:x},{}",
                layer.role(),
                layer.role().mmio_base(),
                layer.uuid()
            ));
        }
        tokens.extend([
            format!("nvx_scratch=0x{SCRATCH_MMIO_BASE:x},ext4"),
            format!("nvx_entrypoint={}", settings.entrypoint),
            format!("nvx_hostname={}", settings.hostname),
        ]);
        tokens.extend(
            settings
                .args
                .iter()
                .map(|argument| format!("nvx_arg={argument}")),
        );
        if let Some(memory_max) = &settings.memory_max {
            tokens.push(format!("nvx_memory_max={memory_max}"));
        }
        if let Some(pids_max) = &settings.pids_max {
            tokens.push(format!("nvx_pids_max={}", pids_max.plus_one()));
        }
        if mount::mounts_use_aggregate(&settings.mounts) {
            let shares: Vec<_> = settings
                .mounts
                .iter()
                .map(|mount| (mount.guest_target(), mount.access(), mount.kind()))
                .collect();
            tokens.extend(mount::aggregate_share_tokens(&shares));
        }
        tokens.retain(|token| !token.is_empty());
        let command_line = tokens.join(" ");
        // OpenVMM appends the shares' bootstrap tokens after this command line, so they consume
        // the same x86 budget.
        let fragment = mount::mounts_command_line_fragment(&settings.mounts);
        if command_line.len() + fragment.len() + 1 > SANDBOX_COMMAND_LINE_MAX_SIZE {
            return Err(refused(
                "sandbox kernel command line exceeds its 1024-byte x86 budget; attach fewer \
                 --mount shares or use shorter targets and arguments",
            ));
        }
        Ok(command_line)
    }
}

/// Checks that a pure path is a regular file, as [`require_file`] does.
fn require_pure_file(path: &PurePath, description: &str) -> Result<()> {
    if fs::is_file(path.as_path()) {
        Ok(())
    } else {
        Err(Error::unavailable(format!(
            "{description} not found: {path}"
        )))
    }
}

#[cfg(test)]
mod golden;
#[cfg(test)]
mod tests;
