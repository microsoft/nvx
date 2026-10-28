//! The sandbox launch profile: a microVM that runs one workload in a container over a read-only
//! distro image and a writable upper, with live virtio-fs shares.
//!
//! This is the profile of `nvx-rs sandbox`: its documented limits, option semantics, OpenVMM
//! arguments, and guest kernel tokens, with native Rust input validation and diagnostics:
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
//!   an EROFS filesystem, as `nvx-rs sandbox` names it with `ROLE,PATH,EROFS_UUID`, or a GPT disk.
//! - [`SandboxMount`]: a requested live share of a host directory or regular file at a guest
//!   target, with its access mode, owner, and access-policy paths; [`mount`] holds the rules of
//!   guest targets and the checks of host paths, plans the shares of a sandbox through the planner
//!   of every NVX caller, [`host::filesystem`](crate::host::filesystem), and renders the plan as
//!   the guest's shell reads it at boot.
//! - [`SandboxLaunch`]: the launch of `nvx-rs sandbox`, its layers over its upper, an ext4
//!   scratch image or RAM, with its settings and the export plan of its shares, which plans its
//!   distro layer, in the layer's format, over that upper.
//! - [`options`]: the checks that `nvx-rs sandbox` and `nvx-rs run` apply to their options: the
//!   attribution of access-policy paths to shares, the systemd entrypoint check and the preflight
//!   that verifies the distro layer, the targets of `nvx-rs run` shares, and the managed exec
//!   environment.
//! - [`distro`]: the binding of the distro layer, which explicit verification makes from the
//!   layer and its manifest, and which a launch checks from metadata alone.
//! - [`scratch`]: the binding of the scratch image, its identity, kind, and length, which a
//!   launch checks from metadata alone, and the claim of the image by one sandbox at a time.
//! - The block geometry that OpenVMM presents for each checked file, its length and its logical
//!   and physical block sizes, which [`CheckedDistro::geometry`](distro::CheckedDistro::geometry)
//!   and [`CheckedScratch::geometry`](scratch::CheckedScratch::geometry) read through the file,
//!   as OpenVMM reads it.
//! - [`PreparedLaunch`]: the launch of a plan with the files that the checks of its block devices
//!   returned, which hands the VMM those objects rather than paths that something could replace
//!   meanwhile: the OpenVMM arguments that name them, and the descriptors that the VMM inherits;
//!   [`prepared`] explains the handoff on each host.
//!
//! Workload IDs and resource limits are ASCII decimal integers in their documented ranges.
//! Layer UUIDs use their canonical lowercase hyphenated text, and resource limits use native
//! unsigned Rust integers. Host paths use the host's native flavor.
//!
//! # Errors
//!
//! A request that the profile refuses fails with
//! [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), and a missing image with
//! [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable). A host query that fails
//! is [`ErrorKind::Io`](crate::ErrorKind::Io), with the native I/O error and, where available,
//! the path quoted with Rust's debug formatting.

pub mod distro;
pub(crate) mod geometry;
pub mod layer;
pub mod mount;
pub mod options;
pub mod plan;
pub mod prepared;
pub mod scratch;

use std::fs::File;
use std::path::Path;

use self::distro::{DistroBinding, DistroFormat};
use self::geometry::BlockGeometry;
pub use self::layer::{LAYER_ROLES, LayerRole, SandboxLayer};
use self::mount::os_error;
pub use self::mount::{MountAccess, MountKind, MountOwner, MountPolicy, PolicyKind, SandboxMount};
pub use self::plan::{
    BlockRole, PLAN_VERSION, SandboxDistro, SandboxNetwork, SandboxPlan, SandboxUpper,
};
pub use self::prepared::PreparedLaunch;
use crate::error::{Error, ErrorCode, Result};
use crate::host::artifacts::require_existing;
use crate::host::filesystem::ExportPlan;
use crate::net::is_valid_hostname;
use crate::protocol::cmdline;
use crate::protocol::root::RootImage;
use crate::uuid::Uuid;

/// The guest adds one for its init process; Linux rejects `pids.max` at or above 4,194,305.
pub(crate) const PIDS_MAX_LIMIT: u32 = 4_194_303;

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

/// Parses a `--workload-user` value, `UID:GID`, with both IDs between 1 and 4294967294.
///
/// 4294967295 is refused: as `(uid_t)-1` it reads as "leave unchanged" to `setresuid`/`setresgid`,
/// so accepting it would leave a one-shot sandbox workload running as UID 0 and GID 0.
///
/// Each ID contains only ASCII decimal digits, without a sign or whitespace. Fails with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn parse_workload_identity(value: &str) -> Result<(u32, u32)> {
    let fields: Vec<&str> = value.split(':').collect();
    let [uid, gid] = fields[..] else {
        return Err(refused("--workload-user must be UID:GID"));
    };
    if [uid, gid]
        .iter()
        .any(|field| field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(refused("--workload-user must contain numeric UID and GID"));
    }
    let id = |value: &str, name: &str| {
        parse_decimal::<u32>(value)
            .filter(|&id| id != 0 && id != u32::MAX)
            .ok_or_else(|| {
                refused(format!(
                    "--workload-user {name} must be between 1 and 4294967294"
                ))
            })
    };
    Ok((id(uid, "UID")?, id(gid, "GID")?))
}

/// Parses nonempty ASCII decimal digits into a native integer, refusing signs and whitespace.
pub(crate) fn parse_decimal<T: std::str::FromStr>(value: &str) -> Option<T> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Returns whether a value cannot stay one unquoted kernel token: whitespace, a control
/// character, a quote, or a UTF-8 byte that Linux treats as whitespace.
pub(crate) fn has_kernel_separator(value: &str) -> bool {
    value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
        || value
            .bytes()
            .any(|byte| byte == b'"' || cmdline::is_space(byte))
}

/// Checks that `path` is a regular file, following links, as `require_file` of `nvx.py` does.
///
/// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) and the message
/// `{description} not found: {path}`, with `path` spelled with its components alone, as
/// [`Path::components`] reads them.
pub fn require_file(path: &Path, description: &str) -> Result<()> {
    require_existing(path, description, ErrorCode::Unavailable)
}

/// Refuses a disk path that OpenVMM's `--microvm-sandbox-block` value could not hold: one with
/// a comma, semicolon, or improperly nested brackets. The message shows `path` as `shown`.
pub(crate) fn reject_disk_path(path: &Path, shown: &str) -> Result<()> {
    let depth = path
        .to_string_lossy()
        .bytes()
        .try_fold(0_usize, |depth, byte| match byte {
            b'[' => Some(depth + 1),
            b']' => depth.checked_sub(1),
            b',' | b';' => None,
            _ => Some(depth),
        });
    if depth != Some(0) {
        return Err(refused(format!(
            "disk paths containing commas, semicolons, or unbalanced brackets are unsupported: \
             {shown}"
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
pub(crate) fn reject_container_path(path: &Path, shown: &str) -> Result<()> {
    let name = path.file_name().map(|name| name.to_string_lossy());
    let extension = match name.as_deref().unwrap_or_default().rsplit_once('.') {
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

/// The logical block size that OpenVMM presents for a sandbox block device, in bytes, of which the
/// device's length is a nonzero multiple.
const LOGICAL_BLOCK_SIZE: u32 = 512;

/// The physical block size that OpenVMM presents for a sandbox block device on Windows, in bytes.
#[cfg(windows)]
const WINDOWS_PHYSICAL_BLOCK_SIZE: u32 = 4096;

/// Returns the block geometry that OpenVMM presents for a sandbox block device whose file `file`
/// holds open, which messages call `description` at `path`, read through `file` as OpenVMM reads
/// it through its own open of the file, as
/// [`CheckedDistro::geometry`](distro::CheckedDistro::geometry) describes.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a length or a physical
/// block size that OpenVMM refuses, and with [`ErrorKind::Io`](crate::ErrorKind::Io) if the file's
/// metadata cannot be read.
pub(crate) fn block_geometry(file: &File, path: &Path, description: &str) -> Result<BlockGeometry> {
    let shown = path.display();
    let metadata = file
        .metadata()
        .map_err(|error| os_error(&error, Some(path)))?;
    let length = metadata.len();
    if length == 0 || !length.is_multiple_of(u64::from(LOGICAL_BLOCK_SIZE)) {
        return Err(refused(format!(
            "the {description} {shown} of {length} bytes cannot be attached: OpenVMM attaches \
             block devices of nonzero multiples of {LOGICAL_BLOCK_SIZE} bytes"
        )));
    }
    #[cfg(target_os = "linux")]
    let (reported, physical) = {
        use std::os::unix::fs::MetadataExt;

        let size = metadata.blksize();
        (size, u32::try_from(size).ok())
    };
    #[cfg(windows)]
    let (reported, physical) = (
        u64::from(WINDOWS_PHYSICAL_BLOCK_SIZE),
        Some(WINDOWS_PHYSICAL_BLOCK_SIZE),
    );
    #[cfg(not(any(target_os = "linux", windows)))]
    let (reported, physical) = (u64::from(LOGICAL_BLOCK_SIZE), Some(LOGICAL_BLOCK_SIZE));
    match physical {
        Some(physical) if physical >= LOGICAL_BLOCK_SIZE && physical.is_power_of_two() => {
            Ok(BlockGeometry {
                length,
                logical_block_size: LOGICAL_BLOCK_SIZE,
                physical_block_size: physical,
            })
        }
        _ => Err(refused(format!(
            "the {description} {shown} cannot be attached: its filesystem's block size of \
             {reported} bytes is not a physical block size that OpenVMM presents, a power of two \
             from {LOGICAL_BLOCK_SIZE} bytes to 2 GiB"
        ))),
    }
}

/// The launch settings of a [`SandboxPlan`] or a [`SandboxLaunch`], besides its block devices and
/// its shares, with the defaults of `nvx-rs sandbox`.
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
    pub memory_max: Option<u64>,
    /// The most processes that the workload may run, if positive; the guest's own init counts
    /// too, so its `pids.max` is one more.
    pub pids_max: Option<u32>,
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
    if !entrypoint.starts_with('/') || has_kernel_separator(entrypoint) {
        return Err(refused(
            "sandbox entrypoint must be an absolute path without whitespace, control characters, or quotes",
        ));
    }
    if args
        .iter()
        .any(|argument| argument.is_empty() || has_kernel_separator(argument))
    {
        return Err(refused(
            "sandbox arguments must be nonempty and contain no whitespace, control characters, or quotes",
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
    for (name, value) in [
        ("memory-max", *memory_max),
        ("pids-max", pids_max.map(u64::from)),
    ] {
        if value == Some(0) {
            return Err(refused(format!("--{name} must be positive")));
        }
    }
    if pids_max
        .as_ref()
        .is_some_and(|&value| value > PIDS_MAX_LIMIT)
    {
        return Err(refused(format!(
            "--pids-max must be at most {PIDS_MAX_LIMIT}"
        )));
    }
    Ok(())
}

/// The launch of a sandbox of `nvx-rs sandbox`: its read-only layers, its upper, an ext4 scratch
/// image or RAM, its settings, the export plan of its shares, and its network, which it plans as
/// its distro layer, in the layer's format, over its upper.
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
    /// The plan: the distro layer, in its format and with its identity, over the upper, with the
    /// settings and the export plan.
    plan: SandboxPlan,
}

impl SandboxLaunch {
    /// Returns the launch of `layers` over `upper`, an ext4 scratch image or RAM, without shares,
    /// which [`with_export`](Self::with_export) adds.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), checking in this
    /// order: no duplicate roles, exactly one layer, the distro layer, a UUID of the distro layer
    /// in its canonical text, as `--layer` writes it, and the settings, as [`SandboxPlan::new`]
    /// checks them.
    pub fn new(
        layers: Vec<SandboxLayer>,
        upper: SandboxUpper,
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
                "{} layer UUID is invalid: {:?}",
                distro.role(),
                distro.uuid()
            )));
        };
        let image = match distro.format() {
            DistroFormat::Erofs => RootImage::erofs(Some(uuid)),
            DistroFormat::Gpt => RootImage::gpt(Some(uuid)),
        };
        let plan = SandboxPlan::new(
            SandboxDistro::at(distro.path().to_path_buf(), image),
            upper,
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

    /// Returns the upper: an ext4 scratch image, or RAM.
    pub fn upper(&self) -> &SandboxUpper {
        self.plan.upper()
    }

    /// Returns the scratch image, or `None` for an upper in RAM.
    pub fn scratch(&self) -> Option<&Path> {
        self.plan.upper().scratch_path()
    }

    /// Returns the settings besides the layers and the upper.
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
    /// its layer, as a `--layer` value names an EROFS layer and its UUID, over its upper, with its
    /// settings, the export plan of its shares, and its network.
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
    /// distro layer and the scratch image of an upper on scratch must be regular files whose paths
    /// have no comma or semicolon, and whose names, and those of the files to which their paths
    /// resolve, do not end in an extension with which OpenVMM would not attach them as raw images,
    /// and the export plan's directories and files must still not overlap, and it must pass
    /// [`ExportPlan::verify`].
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for a missing image,
    /// and otherwise as those checks do.
    pub fn validated(&self) -> Result<()> {
        self.plan.validated()
    }
}

#[cfg(test)]
mod specification;
#[cfg(test)]
mod tests;
