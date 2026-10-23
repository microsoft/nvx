//! The sandbox launch profile: a microVM that runs one workload in a container over a read-only
//! distro image and a writable upper, with live virtio-fs shares.
//!
//! This is the profile of `nvx.py sandbox`, ported from the launch profile that
//! `scripts/nvx_tools/sandbox.py` held until microsoft/nvx#359, with its limits, option
//! semantics, OpenVMM arguments, kernel command line, and messages:
//!
//! - [`plan`]: the typed, persistable [`SandboxPlan`] of a cold layered launch: one read-only
//!   `distro` block device, an EROFS filesystem or a GPT disk with the identity that the guest
//!   expects of it, over an upper on an ext4 scratch image or in RAM, with the launch settings, the
//!   export plan of its live shares, and optionally a network, a [`SandboxNetwork`] whose typed
//!   policy may be proxy-only. It renders the OpenVMM arguments,
//!   [`openvmm_arguments`](SandboxPlan::openvmm_arguments), and the kernel command line,
//!   [`kernel_command_line`](SandboxPlan::kernel_command_line), whose boot tokens
//!   [`protocol::root`](crate::protocol::root) defines. An adapter that runs its own guest plans
//!   the same root with [`SandboxPlan::adapter`], without the profile's workload settings.
//! - [`SandboxLayer`]: the distro layer of a sandbox, the one [`LayerRole`] that a sandbox takes:
//!   an EROFS filesystem, as `nvx.py sandbox` names it with `ROLE,PATH,EROFS_UUID`, or a GPT disk.
//! - [`SandboxMount`]: a requested live share of a host directory or regular file at a guest
//!   target, with its access mode, owner, and access-policy paths; [`mount`] holds the rules of
//!   guest targets and the checks of host paths, plans the shares of a sandbox through the planner
//!   of every NVX caller, [`host::filesystem`](crate::host::filesystem), and renders the plan as
//!   the guest's shell reads it at boot.
//! - [`SandboxLaunch`]: the launch of `nvx.py sandbox`, its layers over its scratch image with
//!   its settings and the export plan of its shares, which plans its distro layer, in the layer's
//!   format, over the scratch image.
//! - [`options`]: the checks that `nvx.py sandbox` and `nvx.py run` apply to their options: the
//!   attribution of access-policy paths to shares, the systemd entrypoint check and the preflight
//!   that verifies the distro layer, the targets of `nvx.py run` shares, and the managed exec
//!   environment.
//! - [`distro`]: the binding of the distro layer, which explicit verification makes from the
//!   layer and its manifest, and which a launch checks from metadata alone.
//! - [`scratch`]: the binding of the scratch image, its identity, kind, and length, which a
//!   launch checks from metadata alone, and the claim of the image by one sandbox at a time.
//! - [`PreparedLaunch`]: the launch of a plan with the files that the checks of its block devices
//!   returned, which hands the VMM those objects rather than paths that something could replace
//!   meanwhile: the OpenVMM arguments that name them, and the descriptors that the VMM inherits;
//!   [`prepared`] explains the handoff on each host.
//!
//! Values are checked as Python checked them: whitespace is what `str.isspace` accepts,
//! integers are what `int` reads ([`Int`]), UUIDs are what `uuid.UUID` reads, host paths are
//! normalized as `pathlib` normalizes them, with backslashes on Windows, and messages quote
//! values with Python's `repr`.
//!
//! # Errors
//!
//! A request that the profile refuses fails with
//! [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), and a missing image with
//! [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable). A host query that fails
//! is [`ErrorKind::Io`](crate::ErrorKind::Io), with Python's form of the system's error, such as
//! `[Errno 13] Permission denied: '/path'`.

pub mod distro;
pub mod layer;
pub mod mount;
pub mod options;
pub mod plan;
pub mod prepared;
pub mod scratch;

use std::path::Path;

use self::distro::{DistroBinding, DistroFormat};
pub use self::layer::{LAYER_ROLES, LayerRole, SandboxLayer};
pub use self::mount::{MountAccess, MountKind, MountOwner, MountPolicy, PolicyKind, SandboxMount};
pub use self::plan::{
    BlockRole, PLAN_VERSION, SandboxDistro, SandboxNetwork, SandboxPlan, SandboxUpper,
};
pub use self::prepared::PreparedLaunch;
use crate::error::{Error, Result};
use crate::host::filesystem::ExportPlan;
pub use crate::host::python::builtins::{Int, IntError};
use crate::host::python::builtins::{is_space, repr};
use crate::host::python::fs;
use crate::host::python::path::{Flavor, PurePath};
use crate::net::is_valid_hostname;
use crate::protocol::root::RootImage;
use crate::uuid::Uuid;

/// The largest value the guest cgroup v2 `memory.max` parser can store.
const MEMORY_MAX_LIMIT: u64 = u64::MAX;
/// The guest adds one for its init process; Linux rejects `pids.max` at or above 4,194,305.
const PIDS_MAX_LIMIT: u32 = 4_194_303;

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
/// a comma or semicolon. The message shows `path` as `shown`.
fn reject_disk_path(path: &PurePath, shown: &str) -> Result<()> {
    if path.as_str().contains([',', ';']) {
        return Err(refused(format!(
            "disk paths containing commas or semicolons are unsupported: {shown}"
        )));
    }
    Ok(())
}

/// The extensions of the files that OpenVMM does not attach as the raw image of a sandbox block
/// device: it opens a `.vhd`, `.vhdx`, or `.vmgs` file in its container format, refuses to write
/// an `.iso` file, and refuses all four for the block devices of a snapshot.
const CONTAINER_EXTENSIONS: [&str; 4] = ["vhd", "vhdx", "vmgs", "iso"];

/// Refuses the image at `path`, which the message shows as `shown`, if OpenVMM would not attach it
/// as a raw image: if the name of `path` has an extension of [`CONTAINER_EXTENSIONS`]. The
/// extension is the text after the name's last dot, unless that dot starts the name, and compares
/// case-sensitively, as OpenVMM reads and compares it.
fn reject_container_path(path: &PurePath, shown: &str) -> Result<()> {
    let extension = match path.name().rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension,
        _ => "",
    };
    if CONTAINER_EXTENSIONS.contains(&extension) {
        return Err(refused(format!(
            "disk paths ending in .{extension} are unsupported, since OpenVMM would not attach \
             them as raw images: {shown}"
        )));
    }
    Ok(())
}

/// The launch settings of a [`SandboxPlan`] or a [`SandboxLaunch`], besides its block devices and
/// its shares, with the defaults of `nvx.py sandbox`.
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
        }
    }
}

/// Checks the launch settings of a plan, in this order: the entrypoint, the arguments, the
/// hostname, a non-root identity, positive limits, limits that the guest's cgroups can apply, and
/// at most [`u64::MAX`] bytes of memory and 4,194,303 processes.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
fn validate_settings(settings: &SandboxSettings) -> Result<()> {
    let SandboxSettings {
        entrypoint,
        args,
        hostname,
        workload_identity: (uid, gid),
        memory_max,
        pids_max,
    } = settings;
    let has_invalid_chars =
        |text: &str| text.contains('"') || text.bytes().any(|byte| byte == 0xa0);
    if !entrypoint.starts_with('/')
        || entrypoint.chars().any(is_space)
        || has_invalid_chars(entrypoint)
    {
        return Err(refused(
            "sandbox entrypoint must be an absolute path without spaces or quotes",
        ));
    }
    if args.iter().any(|argument| {
        argument.is_empty() || argument.chars().any(is_space) || has_invalid_chars(argument)
    }) {
        return Err(refused(
            "sandbox arguments must be nonempty and contain no spaces or quotes",
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
    if memory_max
        .as_ref()
        .is_some_and(|value| value.to_u64().is_none())
    {
        return Err(refused(format!(
            "--memory-max must be at most {MEMORY_MAX_LIMIT}"
        )));
    }
    if pids_max
        .as_ref()
        .is_some_and(|value| value.to_u32().is_none_or(|value| value > PIDS_MAX_LIMIT))
    {
        return Err(refused(format!(
            "--pids-max must be at most {PIDS_MAX_LIMIT}"
        )));
    }
    Ok(())
}

/// The launch of a sandbox of `nvx.py sandbox`: its read-only layers, its ext4 scratch image, its
/// settings, the export plan of its shares, and its network, which it plans as its distro layer,
/// in the layer's format, over the scratch image.
///
/// Its layers are keyed by the role of each `--layer` value, of which a launch takes only the
/// distro layer. [`plan`](Self::plan) returns its [`SandboxPlan`], which renders its OpenVMM
/// arguments and kernel command line, and [`bound_plan`](Self::bound_plan) the plan with the image
/// that the binding of its distro layer records. A launch has no network device until
/// [`with_network`](Self::with_network) gives it one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxLaunch {
    /// The read-only layers, in the order given: the distro layer alone.
    layers: Vec<SandboxLayer>,
    /// The plan: the distro layer, in its format and with its identity, over the scratch image,
    /// with the settings and the export plan.
    plan: SandboxPlan,
}

impl SandboxLaunch {
    /// Returns the launch of `layers` over the ext4 image at `scratch`, which is normalized as
    /// `pathlib.Path` normalizes it, without shares, which [`with_export`](Self::with_export)
    /// adds.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), checking in this
    /// order: no duplicate roles, exactly one layer, the distro layer, a UUID of the distro layer
    /// in its canonical text, as `--layer` writes it, and the settings, as [`SandboxPlan::new`]
    /// checks them.
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
        let distro = match &layers[..] {
            [layer] if layer.role() == LayerRole::Distro => layer,
            _ => {
                return Err(refused(
                    "a sandbox requires exactly one read-only distro layer",
                ));
            }
        };
        let Some(uuid) = Uuid::parse(distro.uuid()) else {
            return Err(refused(format!(
                "{} layer UUID is invalid: {}",
                distro.role(),
                repr(distro.uuid())
            )));
        };
        let image = match distro.format() {
            DistroFormat::Erofs => RootImage::erofs(Some(uuid)),
            DistroFormat::Gpt => RootImage::gpt(Some(uuid)),
        };
        let plan = SandboxPlan::new(
            SandboxDistro::at(distro.pure_path().clone(), image),
            SandboxUpper::scratch_in(flavor, scratch),
            settings,
        )?;
        Ok(Self { layers, plan })
    }

    /// Returns the layers, in the order given.
    pub fn layers(&self) -> &[SandboxLayer] {
        &self.layers
    }

    /// Returns the distro layer, the one layer that a launch holds.
    pub fn distro(&self) -> &SandboxLayer {
        &self.layers[0]
    }

    /// Returns the scratch image.
    pub fn scratch(&self) -> &Path {
        self.plan
            .upper()
            .scratch_path()
            .expect("a launch keeps its upper on its scratch image")
    }

    /// Returns the settings besides the layers and the scratch image.
    pub fn settings(&self) -> &SandboxSettings {
        self.plan
            .settings()
            .expect("a launch plans the sandbox profile, with its settings")
    }

    /// Returns the export plan of the shares, if any.
    pub fn export(&self) -> Option<&ExportPlan> {
        self.plan.export()
    }

    /// Returns the launch with `export`, the export plan of its live shares, if any, such as
    /// [`mount::plan_mounts`] makes.
    ///
    /// Fails as [`SandboxPlan::with_export`] checks the export plan.
    pub fn with_export(self, export: Option<ExportPlan>) -> Result<Self> {
        let Self { layers, plan } = self;
        Ok(Self {
            layers,
            plan: plan.with_export(export)?,
        })
    }

    /// Returns the launch with `network`, or with no network device for `None`.
    #[must_use]
    pub fn with_network(self, network: Option<SandboxNetwork>) -> Self {
        Self {
            plan: self.plan.with_network(network),
            ..self
        }
    }

    /// Returns the network, or `None` for no network device.
    pub fn network(&self) -> Option<&SandboxNetwork> {
        self.plan.network()
    }

    /// Returns the plan of the launch: its distro layer, in the format and with the identity of
    /// its layer, as a `--layer` value names an EROFS layer and its UUID, over its scratch image,
    /// with its settings, the export plan of its shares, and its network.
    pub fn plan(&self) -> &SandboxPlan {
        &self.plan
    }

    /// Returns the plan of the launch with the image that `binding`, the binding of its distro
    /// layer, records: the staged format, and the identity that the guest expects of the layer.
    ///
    /// A binding that [`DistroBinding::check`] accepts for the distro layer names the format and
    /// identity that [`plan`](Self::plan) takes from the layer's `--layer` value, so both plans
    /// render alike; callers that hold the binding plan from it. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a binding whose identity is
    /// not a UUID in its canonical text.
    pub fn bound_plan(&self, binding: &DistroBinding) -> Result<SandboxPlan> {
        Ok(self.plan.with_image(plan::bound_image(binding)?))
    }

    /// Checks the launch against the host, as [`SandboxPlan::validated`] checks its plan: the
    /// distro layer and the scratch image must be regular files whose paths have no comma or
    /// semicolon, and whose names, and those of the files to which their paths resolve, do not end
    /// in an extension with which OpenVMM would not attach them as raw images, and the export
    /// plan's directories and files must still not overlap, and it must pass
    /// [`ExportPlan::verify`].
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for a missing image,
    /// and otherwise as those checks do.
    pub fn validated(&self) -> Result<()> {
        self.plan.validated()
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
mod specification;
#[cfg(test)]
mod tests;
