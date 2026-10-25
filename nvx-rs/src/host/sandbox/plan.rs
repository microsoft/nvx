//! The typed plan of a cold layered launch: its block devices, which image over which upper, its
//! launch settings, its live shares, and its network, from which come OpenVMM's arguments and the
//! kernel command line.
//!
//! A [`SandboxPlan`] holds:
//!
//! - one read-only `distro` block device, a [`SandboxDistro`]: a raw host image, an EROFS
//!   filesystem or a GPT disk, with the identity that the guest expects of it, if any, as its
//!   [`RootImage`] names them;
//! - an upper, a [`SandboxUpper`]: a whole-device ext4 scratch image on the `scratch` block
//!   device, or RAM, with no scratch block device;
//! - the launch settings of NVX's sandbox profile, [`SandboxSettings`]: the entrypoint and its
//!   arguments, the hostname, the workload identity, and the resource limits. An adapter's plan,
//!   [`SandboxPlan::adapter`], has none: the adapter's own guest runs its workloads;
//! - the export plan of the live shares, an [`ExportPlan`] that [`filesystem::plan`](crate::host::filesystem::plan) made for the
//!   container root, if the sandbox has shares: the children of OpenVMM's aggregate export, the
//!   ordered binds of the guest, and the identities that each start checks again. An adapter's
//!   plan has none: the adapter attaches its shares itself;
//! - optionally, a network, [`SandboxNetwork`]: the guest's address and the [`NetworkPolicy`]
//!   that OpenVMM enforces there, which one lowering, [`network::openvmm_arguments`], renders for
//!   every caller. A proxy-only policy reports the URL of its proxy as the guest sees it,
//!   [`proxy_url`](SandboxPlan::proxy_url).
//!
//! A plan keys its block devices by role, [`BlockRole`], and knows only the `distro` and
//! `scratch` roles. Every combination of format and upper plans and renders:
//! [`openvmm_arguments`](SandboxPlan::openvmm_arguments) attaches the block devices, with no
//! scratch block device for a RAM upper, and the network device, and
//! [`kernel_command_line`](SandboxPlan::kernel_command_line) describes the root with the boot
//! tokens of [`protocol::root`](crate::protocol::root), within the budget that OpenVMM leaves the
//! profile. Both render the shares as the managed lifecycle delivers them: OpenVMM exports them
//! at [`GUEST_EXPORT`](crate::host::filesystem::GUEST_EXPORT), where the guest mounts the export,
//! the kernel command line announces the number of their mappings with `nvx_maps=`, and the host
//! sends the plan's [`mapping_table`](SandboxPlan::mapping_table) in `MAPS` requests once the
//! guest agent is ready, which maps each in order beneath the container's root. The CLI's
//! one-shot lifecycle still renders them as boot tokens, from which the guest's shell binds them,
//! as [`mount::boot_shares`] does. An adapter's plan renders its root and devices alone, and lets
//! the adapter's own boot tokens through, as
//! [`kernel_command_line`](SandboxPlan::kernel_command_line) says. The plan's arguments name the
//! block devices by their paths; a
//! launch renders them through a [`PreparedLaunch`](super::PreparedLaunch), which names the files
//! that the checks of the images opened instead.
//!
//! A plan is persistable. Its serde form, of version [`PLAN_VERSION`], requires every field and
//! refuses unknown ones and other versions, and reading it checks the plan as
//! [`SandboxPlan::new`] or [`SandboxPlan::adapter`], [`SandboxPlan::with_export`], and
//! [`SandboxNetwork::new`] do:
//!
//! ```json
//! {"version": 3,
//!  "distro": {"path": "/images/distro.erofs", "format": "erofs",
//!             "identity": "11111111-1111-1111-1111-111111111111"},
//!  "upper": {"kind": "scratch", "path": "/sandboxes/one/scratch.ext4"},
//!  "settings": {"entrypoint": "/bin/sh", "args": [], "hostname": "nvx-sandbox",
//!               "workloadIdentity": {"uid": 65534, "gid": 65534},
//!               "memoryMax": "268435456", "pidsMax": null},
//!  "export": EXPORT,
//!  "network": NETWORK}
//! ```
//!
//! The `format` of the image is `erofs` or `gpt`, its `identity` a UUID in its canonical text or
//! `null`, and a RAM upper is `{"kind": "ram"}`. The limits are decimal strings or `null`, since a
//! memory limit may exceed the integers that a JSON number holds exactly. The `settings` of an
//! adapter's plan are `null`, as is its `EXPORT`. `EXPORT` is `null`
//! without shares, and otherwise the serde form of an [`ExportPlan`]:
//!
//! ```json
//! {"children": [{"root": "/home/user/work", "writable": true,
//!                "denied": ["/home/user/work/secrets"],
//!                "deniedIdentities": [{"device": 2049, "index": 1835013}],
//!                "allowed": [], "write": []}],
//!  "binds": [{"child": 0, "source": "", "target": "/workspace", "readOnly": false}]}
//! ```
//!
//! The `NETWORK` is `null` for no network device, or keeps the fields of a [`SandboxNetwork`] and
//! its [`NetworkPolicy`]:
//!
//! ```json
//! {"guestNetwork": "10.0.0.2/24", "egress": "allow",
//!  "allow": {"name": "egress.allow", "rules": []},
//!  "deny": {"name": "egress.deny",
//!           "rules": [{"to": [{"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}],
//!                      "ports": [{"protocol": "tcp", "port": 443, "endPort": null}]}]},
//!  "ingress": "deny", "hostLoopback": "allow",
//!  "hostLoopbackForwards": [{"protocol": "tcp", "hostPort": 18080, "guestPort": 8080}],
//!  "proxy": null}
//! ```
//!
//! `egress` and `ingress` are `allow` or `deny`, `hostLoopback` is `deny`, `outbound`, or
//! `allow`, a port's `protocol` is `tcp`, `udp`, `icmp`, or `any`, and a forward's `tcp` or `udp`.
//! The rules' `name`s name them in messages. A proxy-only network denies everything else, and
//! names the proxy's port on host loopback and the scheme of its URL, `http`, `https`, or `null`:
//!
//! ```json
//! {"guestNetwork": "10.0.0.2/24", "egress": "deny",
//!  "allow": {"name": "egress.allow", "rules": []},
//!  "deny": {"name": "egress.deny", "rules": []},
//!  "ingress": "deny", "hostLoopback": "deny", "hostLoopbackForwards": [],
//!  "proxy": {"port": 3128, "scheme": "http"}}
//! ```
//!
//! A plan of another version is not read: versions 1 and 2 held the shares as they were
//! requested, which the plan of each launch now resolves.

mod form;

use std::fmt;
use std::net::Ipv4Addr;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::distro::{DistroBinding, DistroFormat};
use super::mount::{self, BootShares};
use super::{
    SANDBOX_COMMAND_LINE_MAX_SIZE, SandboxSettings, refused, reject_container_path,
    reject_disk_path, require_pure_file, validate_settings,
};
use crate::error::{ErrorCode, Result};
use crate::host::filesystem::{ExportPlan, ReservedTargets};
use crate::host::network::{self, NetworkPolicy};
use crate::host::python::builtins::{repr, split, strip};
use crate::host::python::path::{Flavor, PurePath};
use crate::protocol::HostMap;
use crate::protocol::root::{
    DISTRO_MMIO_BASE, ImageFormat, LAYER_TOKEN, RAM_UPPER_TOKEN, RootImage, RootLayout, RootUpper,
    SCRATCH_MMIO_BASE, SCRATCH_TOKEN,
};
use crate::uuid::Uuid;

/// The version of the serde form of [`SandboxPlan`] that this build writes and reads.
pub const PLAN_VERSION: u32 = 3;

/// How a launch delivers the shares of a plan to the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The managed lifecycle's: OpenVMM exports the shares at
    /// [`GUEST_EXPORT`](crate::host::filesystem::GUEST_EXPORT), the kernel command line announces
    /// the number of their mappings, and the host sends the mapping table once the guest agent is
    /// ready.
    MappingTable,
    /// The one-shot lifecycle's, which only the CLI launches: boot tokens, from which the guest's
    /// shell binds the shares.
    #[cfg(any(test, feature = "cli"))]
    BootTokens,
}

/// The role of a block device of a plan, which fixes its MMIO base and its access.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BlockRole {
    /// The read-only image.
    Distro,
    /// The writable ext4 upper.
    Scratch,
}

impl BlockRole {
    /// Returns the role's name, which OpenVMM's `--microvm-sandbox-block` takes: `distro` or
    /// `scratch`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Distro => "distro",
            Self::Scratch => "scratch",
        }
    }

    /// Returns the MMIO base of the role's block device: [`DISTRO_MMIO_BASE`] or
    /// [`SCRATCH_MMIO_BASE`].
    pub const fn mmio_base(self) -> u64 {
        match self {
            Self::Distro => DISTRO_MMIO_BASE,
            Self::Scratch => SCRATCH_MMIO_BASE,
        }
    }

    /// Returns whether the role's block device is read-only, as the distro image is.
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::Distro)
    }
}

impl fmt::Display for BlockRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The read-only `distro` block device of a plan: a raw host image, and its format and the
/// identity that the guest expects of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxDistro {
    /// The image, as `pathlib` normalizes it.
    path: PurePath,
    /// The image's format and expected identity.
    image: RootImage,
}

impl SandboxDistro {
    /// Returns the image at `path`, normalized as `pathlib.Path` normalizes it, in the format and
    /// with the expected identity of `image`.
    ///
    /// The image is raw: OpenVMM attaches the file's bytes as they are. An image in a container
    /// format, such as a VHD, is staged as a raw file first, under a name whose extension does not
    /// make OpenVMM read a container, which [`SandboxPlan::validated`] checks.
    pub fn new(path: &str, image: RootImage) -> Self {
        Self::new_in(Flavor::NATIVE, path, image)
    }

    /// Implements [`new`](Self::new) for a path of `flavor`.
    pub(crate) fn new_in(flavor: Flavor, path: &str, image: RootImage) -> Self {
        Self::at(PurePath::new(flavor, path), image)
    }

    /// Returns the image at `path`, which is normalized already.
    pub(crate) fn at(path: PurePath, image: RootImage) -> Self {
        Self { path, image }
    }

    /// Returns the image's path.
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Returns the image's path, as `pathlib` normalizes it.
    pub(crate) fn pure_path(&self) -> &PurePath {
        &self.path
    }

    /// Returns the image's format and expected identity.
    pub fn image(&self) -> RootImage {
        self.image
    }

    /// Returns how messages name the image: `distro EROFS layer` or `distro GPT image`.
    fn description(&self) -> &'static str {
        match self.image.format {
            ImageFormat::Erofs => "distro EROFS layer",
            ImageFormat::Gpt => "distro GPT image",
        }
    }
}

/// Where a plan keeps the upper of its overlay: in an ext4 scratch image on the `scratch` block
/// device, or in RAM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxUpper {
    /// The scratch image, as `pathlib` normalizes it, or `None` for a RAM upper.
    scratch: Option<PurePath>,
}

impl SandboxUpper {
    /// An upper in RAM, with no scratch block device.
    pub const RAM: Self = Self { scratch: None };

    /// Returns an upper on the preformatted ext4 scratch image at `path`, normalized as
    /// `pathlib.Path` normalizes it, whose whole device the guest mounts.
    pub fn scratch(path: &str) -> Self {
        Self::scratch_in(Flavor::NATIVE, path)
    }

    /// Implements [`scratch`](Self::scratch) for a path of `flavor`.
    pub(crate) fn scratch_in(flavor: Flavor, path: &str) -> Self {
        Self::scratch_at(PurePath::new(flavor, path))
    }

    /// Returns an upper on the scratch image at `path`, which is normalized already.
    pub(crate) fn scratch_at(path: PurePath) -> Self {
        Self {
            scratch: Some(path),
        }
    }

    /// Returns where the upper is, as the boot tokens name it.
    pub fn kind(&self) -> RootUpper {
        match self.scratch {
            Some(_) => RootUpper::Scratch,
            None => RootUpper::Ram,
        }
    }

    /// Returns the scratch image, or `None` for a RAM upper.
    pub fn scratch_path(&self) -> Option<&Path> {
        self.scratch.as_ref().map(PurePath::as_path)
    }

    /// Returns the scratch image, as `pathlib` normalizes it, or `None` for a RAM upper.
    pub(crate) fn pure_scratch_path(&self) -> Option<&PurePath> {
        self.scratch.as_ref()
    }
}

/// The network of a plan: the guest's static IPv4 address on its network, and the policy that
/// OpenVMM enforces there, which [`network::openvmm_arguments`] lowers.
///
/// Its serde form is the `network` of the persisted plan of the [module](self).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "form::NetworkForm", try_from = "form::NetworkForm")]
pub struct SandboxNetwork {
    /// The guest's address and prefix, such as `10.0.0.2/24`.
    guest_network: String,
    /// The first address of the guest's network, where the guest reaches its gateway.
    gateway: Ipv4Addr,
    /// The policy.
    policy: NetworkPolicy,
}

impl SandboxNetwork {
    /// Returns the network of a guest at `guest_network`, an IPv4 address and prefix such as
    /// `10.0.0.2/24`, under `policy`.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a guest network
    /// that OpenVMM refuses, one for which [`crate::net::guest_gateway`] finds no gateway, and
    /// then as [`network::openvmm_arguments`] fails for a policy that it cannot lower.
    pub fn new(guest_network: &str, policy: NetworkPolicy) -> Result<Self> {
        let gateway = guest_gateway(guest_network)?;
        network::openvmm_arguments(Some(&policy), guest_network)?;
        Ok(Self {
            guest_network: guest_network.to_owned(),
            gateway,
            policy,
        })
    }

    /// Returns the guest's address and prefix.
    pub fn guest_network(&self) -> &str {
        &self.guest_network
    }

    /// Returns the address at which the guest reaches its gateway: the first address of its
    /// network.
    pub fn gateway(&self) -> Ipv4Addr {
        self.gateway
    }

    /// Returns the policy.
    pub fn policy(&self) -> &NetworkPolicy {
        &self.policy
    }

    /// Returns the URL of the policy's proxy as the guest sees it, at its gateway:
    /// `SCHEME://GATEWAY:PORT` with a scheme and `GATEWAY:PORT` without one, or `None` without a
    /// proxy. How workloads learn it is the caller's choice.
    pub fn proxy_url(&self) -> Option<String> {
        self.policy.proxy.map(|proxy| proxy.url(self.gateway))
    }

    /// Returns the OpenVMM arguments that attach the network device and enforce the policy, none
    /// for a policy that lets nothing through, as [`network::openvmm_arguments`] renders them.
    pub fn openvmm_arguments(&self) -> Result<Vec<String>> {
        network::openvmm_arguments(Some(&self.policy), &self.guest_network)
    }
}

/// Returns the gateway of `guest_network`, a static guest address that OpenVMM accepts, as
/// [`crate::net::guest_gateway`] finds it.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for any other value, as
/// `guest network '10.0.0.1/24' must be an IPv4 address with a /1 to /30 prefix that is not its
/// network's network, broadcast, or gateway (first) address`.
pub(crate) fn guest_gateway(guest_network: &str) -> Result<Ipv4Addr> {
    crate::net::guest_gateway(guest_network).ok_or_else(|| {
        refused(format!(
            "guest network {} must be an IPv4 address with a /1 to /30 prefix that is not its \
             network's network, broadcast, or gateway (first) address",
            repr(guest_network)
        ))
    })
}

/// The plan of a cold layered launch: everything that its OpenVMM arguments and kernel command
/// line depend on.
///
/// A plan launches either NVX's sandbox profile, whose guest runs one workload with the plan's
/// [`SandboxSettings`], [`new`](Self::new), or an adapter's own guest, which runs its workloads
/// itself, [`adapter`](Self::adapter).
///
/// Its serde form is the persisted plan of the [module](self).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "form::PlanForm", try_from = "form::PlanForm")]
pub struct SandboxPlan {
    /// The image.
    distro: SandboxDistro,
    /// The upper.
    upper: SandboxUpper,
    /// The launch settings of the sandbox profile, or `None` for an adapter's plan.
    settings: Option<SandboxSettings>,
    /// The export plan of the live shares, if any, which an adapter's plan never has.
    export: Option<ExportPlan>,
    /// The network, or `None` for no network device.
    network: Option<SandboxNetwork>,
}

impl SandboxPlan {
    /// Returns the plan of `distro` over `upper`, with `settings`, without live shares, and with
    /// no network device; [`with_export`](Self::with_export) and
    /// [`with_network`](Self::with_network) add them.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), checking the settings
    /// in this order: the entrypoint, the arguments, the hostname, a non-root identity, positive
    /// limits, limits that the guest's cgroups can apply, and at most [`u64::MAX`] bytes of memory
    /// and 4,194,303 processes.
    pub fn new(
        distro: SandboxDistro,
        upper: SandboxUpper,
        settings: SandboxSettings,
    ) -> Result<Self> {
        validate_settings(&settings)?;
        Ok(Self {
            distro,
            upper,
            settings: Some(settings),
            export: None,
            network: None,
        })
    }

    /// Returns the plan of `distro` over `upper` for an adapter's own guest, with no network
    /// device, which [`with_network`](Self::with_network) adds.
    ///
    /// An adapter's plan renders the root and its devices alone. Its guest chooses the identity,
    /// the program, and the limits of its workloads, so the plan names none of them to the VMM or
    /// the guest, and the adapter attaches its live shares itself, with the arguments that
    /// [`ExportPlan::openvmm_arguments`] renders, so the plan holds no export plan. Its kernel
    /// command line takes the adapter's own `nvx_*` tokens beside those of the root, as
    /// [`kernel_command_line`](Self::kernel_command_line) says.
    pub fn adapter(distro: SandboxDistro, upper: SandboxUpper) -> Self {
        Self {
            distro,
            upper,
            settings: None,
            export: None,
            network: None,
        }
    }

    /// Returns the plan with `export`, the export plan of its live shares, if any, which
    /// [`filesystem::plan`](crate::host::filesystem::plan) made for the container root.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for an export plan
    /// that [`ExportPlan::check`] refuses for the container root,
    /// [`ReservedTargets::CONTAINER_ROOT`], and for any export plan of an adapter's plan.
    pub fn with_export(self, export: Option<ExportPlan>) -> Result<Self> {
        if let Some(export) = &export {
            if self.settings.is_none() {
                return Err(refused(
                    "an adapter's plan holds no export plan: the adapter attaches its shares itself",
                ));
            }
            export.check(ReservedTargets::CONTAINER_ROOT)?;
        }
        Ok(Self { export, ..self })
    }

    /// Returns the plan with `network`, or with no network device for `None`.
    #[must_use]
    pub fn with_network(mut self, network: Option<SandboxNetwork>) -> Self {
        self.network = network;
        self
    }

    /// Returns the image.
    pub fn distro(&self) -> &SandboxDistro {
        &self.distro
    }

    /// Returns the upper.
    pub fn upper(&self) -> &SandboxUpper {
        &self.upper
    }

    /// Returns the launch settings of the sandbox profile, or `None` for an adapter's plan.
    pub fn settings(&self) -> Option<&SandboxSettings> {
        self.settings.as_ref()
    }

    /// Returns the export plan of the live shares, if any.
    pub fn export(&self) -> Option<&ExportPlan> {
        self.export.as_ref()
    }

    /// Returns the network, or `None` for no network device.
    pub fn network(&self) -> Option<&SandboxNetwork> {
        self.network.as_ref()
    }

    /// Returns the URL of the network's proxy as the guest sees it, as
    /// [`SandboxNetwork::proxy_url`] does, or `None` without one.
    pub fn proxy_url(&self) -> Option<String> {
        self.network.as_ref().and_then(SandboxNetwork::proxy_url)
    }

    /// Returns the layout of the root that the plan boots: its image and its upper.
    pub fn layout(&self) -> RootLayout {
        RootLayout {
            image: self.distro.image,
            upper: self.upper.kind(),
        }
    }

    /// Returns the block devices of the plan in role order: the image, then the scratch image of
    /// an upper on scratch.
    pub fn blocks(&self) -> Vec<(BlockRole, &Path)> {
        self.pure_blocks()
            .into_iter()
            .map(|(role, path)| (role, path.as_path()))
            .collect()
    }

    /// Returns the block devices of the plan in role order, with their pure paths.
    fn pure_blocks(&self) -> Vec<(BlockRole, &PurePath)> {
        let mut blocks = vec![(BlockRole::Distro, &self.distro.path)];
        if let Some(scratch) = &self.upper.scratch {
            blocks.push((BlockRole::Scratch, scratch));
        }
        blocks
    }

    /// Returns the plan with another image, over the same upper and with the same settings and
    /// shares.
    pub(crate) fn with_image(&self, image: RootImage) -> Self {
        Self {
            distro: SandboxDistro::at(self.distro.path.clone(), image),
            ..self.clone()
        }
    }

    /// Checks the plan against the host: each image of its block devices must be a regular file
    /// whose path has no comma or semicolon, and whose name, and that of the file to which its
    /// path resolves, as `provision` records it, do not end in `.vhd`, `.vhdx`, `.vmgs`, or
    /// `.iso`, with which OpenVMM would not attach it as a raw image; and the export plan's
    /// directories and files must still not overlap, by path or through a hard link or bind
    /// mount, and the plan must pass [`ExportPlan::verify`], which checks again the host objects
    /// that it recorded.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for a missing image, as
    /// `distro EROFS layer not found: PATH`, `distro GPT image not found: PATH`, or `ext4 scratch
    /// image not found: PATH`, and otherwise as those checks do.
    pub fn validated(&self) -> Result<()> {
        self.validated_with(&mount::HostQueries::native())
    }

    /// Implements [`validated`](Self::validated) with the given host queries.
    pub(crate) fn validated_with(&self, host: &mount::HostQueries<'_>) -> Result<()> {
        for (role, path) in self.pure_blocks() {
            let description = match role {
                BlockRole::Distro => self.distro.description(),
                BlockRole::Scratch => "ext4 scratch image",
            };
            require_pure_file(path, description, ErrorCode::Unavailable)?;
            reject_disk_path(path, path.as_str())?;
            reject_container_path(path, path.as_str())?;
            // OpenVMM reads the extension of the path that it is given, which for a provisioned
            // sandbox is the resolved one: a link must not resolve to a container's name either.
            let resolved = (host.resolve)(path)
                .map_err(|error| mount::os_error(&error, Some(path.as_str())))?;
            if resolved.name() != path.name() {
                let shown = format!("{} resolves to {}", path.as_str(), resolved.as_str());
                reject_container_path(&resolved, &shown)?;
            }
        }
        match &self.export {
            Some(export) => {
                mount::check_distinct_export_roots(export, host)?;
                export.verify()
            }
            None => Ok(()),
        }
    }

    /// Returns the shares as `delivery` delivers them, which [`mount::boot_shares`] renders as
    /// boot tokens; nothing without shares.
    fn shares(&self, delivery: Delivery) -> Result<BootShares> {
        let Some(export) = &self.export else {
            return Ok(BootShares::default());
        };
        Ok(match delivery {
            Delivery::MappingTable => BootShares {
                arguments: export.openvmm_arguments(),
                tokens: vec![export.kernel_token()],
                fragment: mount::export_command_line_fragment(export),
            },
            #[cfg(any(test, feature = "cli"))]
            Delivery::BootTokens => mount::boot_shares(export)?,
        })
    }

    /// Returns the OpenVMM arguments of the plan as the managed lifecycle launches it: the
    /// microVM machine, a sandbox block device for each block device of the plan in role order,
    /// `ROLE:file:PATH` with `,ro` for the read-only image, the workload identity of the sandbox
    /// profile, the arguments that export the shares at
    /// [`GUEST_EXPORT`](crate::host::filesystem::GUEST_EXPORT), which
    /// [`ExportPlan::openvmm_arguments`] renders, and from which the guest agent maps them as
    /// [`mapping_table`](Self::mapping_table) orders, and those of the network,
    /// [`SandboxNetwork::openvmm_arguments`]. An adapter's plan names no workload identity and
    /// exports no shares.
    ///
    /// Fails as [`SandboxNetwork::openvmm_arguments`] does.
    pub fn openvmm_arguments(&self) -> Result<Vec<String>> {
        self.render_arguments(&|_, path| path.as_str().to_owned(), Delivery::MappingTable)
    }

    /// Returns the OpenVMM arguments of the plan as the one-shot lifecycle launches it: as
    /// [`openvmm_arguments`](Self::openvmm_arguments) does, but with the arguments that attach
    /// the shares as the guest's shell binds them at boot.
    ///
    /// Fails as [`mount::boot_shares`] and [`SandboxNetwork::openvmm_arguments`] do.
    #[cfg(any(test, feature = "cli"))]
    pub(crate) fn one_shot_openvmm_arguments(&self) -> Result<Vec<String>> {
        self.render_arguments(&|_, path| path.as_str().to_owned(), Delivery::BootTokens)
    }

    /// Renders the OpenVMM arguments of the plan, with `block` naming the file of each block
    /// device, given its role and its path in the plan, and with the shares as `delivery` delivers
    /// them.
    pub(crate) fn render_arguments(
        &self,
        block: &dyn Fn(BlockRole, &PurePath) -> String,
        delivery: Delivery,
    ) -> Result<Vec<String>> {
        let mut arguments = vec!["--machine".to_owned(), "microvm".to_owned()];
        for (role, path) in self.pure_blocks() {
            let access = if role.is_read_only() { ",ro" } else { "" };
            arguments.push("--microvm-sandbox-block".to_owned());
            arguments.push(format!("{role}:file:{}{access}", block(role, path)));
        }
        if let Some(settings) = &self.settings {
            let (uid, gid) = settings.workload_identity;
            arguments.extend([
                "--microvm-workload-identity".to_owned(),
                format!("{uid}:{gid}"),
            ]);
        }
        arguments.extend(self.shares(delivery)?.arguments);
        if let Some(network) = &self.network {
            arguments.extend(network.openvmm_arguments()?);
        }
        Ok(arguments)
    }

    /// Returns the plan's mapping table, the binds of its export plan in their order, which
    /// [`ExportPlan::guest_table`] renders: what the managed lifecycle sends in `MAPS` requests
    /// once the guest agent is ready, and before the launch is usable. A plan without shares has
    /// none, and its launch maps nothing.
    pub fn mapping_table(&self) -> Vec<HostMap> {
        self.export
            .as_ref()
            .map_or_else(Vec::new, ExportPlan::guest_table)
    }

    /// Returns the kernel command line of the plan as the managed lifecycle boots it:
    /// `user_command_line`, stripped of Python whitespace, then `nvx_sandbox=1`, the boot tokens
    /// of the root's image and upper, `nvx_entrypoint=`, `nvx_hostname=`, a `nvx_arg=` token for
    /// each argument, `nvx_memory_max=` and `nvx_pids_max=` when set, and with shares the
    /// `nvx_maps=` token that announces the number of mappings of
    /// [`mapping_table`](Self::mapping_table).
    ///
    /// The user's command line may hold no NUL and no `nvx_*` or `virtfs_*` token, which the
    /// profile and OpenVMM own. The command line, the bootstrap tokens that OpenVMM appends for
    /// the shares, and a NUL must fit in [`SANDBOX_COMMAND_LINE_MAX_SIZE`] bytes. The guest must
    /// then read the plan's root from the command line: one that [`RootLayout::parse`] refuses
    /// fails too, as when the user's command line adds a virtio-mmio device at the scratch base
    /// beside a RAM upper. Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
    ///
    /// For an adapter's plan, the command line is `user_command_line`, stripped, then the boot
    /// tokens of the root alone. The adapter's command line may hold its own `nvx_*` tokens, but
    /// no NUL, none of the root's tokens, `nvx_layer=`, `nvx_scratch=`, and `nvx_overlay_upper=`,
    /// and no `virtfs_*` token, which OpenVMM appends for the shares that the adapter attaches.
    /// The command line and a NUL must fit in [`SANDBOX_COMMAND_LINE_MAX_SIZE`] bytes, which
    /// leaves the bootstrap tokens of the adapter's shares to the VMM's own check of the whole
    /// command line; the guest must read the plan's root from it, as for the sandbox profile.
    pub fn kernel_command_line(&self, user_command_line: &str) -> Result<String> {
        self.command_line(user_command_line, Delivery::MappingTable)
    }

    /// Returns the kernel command line of the plan as the one-shot lifecycle boots it: as
    /// [`kernel_command_line`](Self::kernel_command_line) does, but with the `nvx_share=` tokens
    /// of an aggregate, which [`mount::boot_shares`] renders, instead of `nvx_maps=`.
    ///
    /// Fails as [`kernel_command_line`](Self::kernel_command_line) and [`mount::boot_shares`]
    /// do.
    #[cfg(any(test, feature = "cli"))]
    pub(crate) fn one_shot_kernel_command_line(&self, user_command_line: &str) -> Result<String> {
        self.command_line(user_command_line, Delivery::BootTokens)
    }

    /// Returns the kernel command line of the plan, with the shares as `delivery` delivers them.
    pub(crate) fn command_line(
        &self,
        user_command_line: &str,
        delivery: Delivery,
    ) -> Result<String> {
        if user_command_line.contains('\0') {
            return Err(refused("kernel command line contains an embedded NUL"));
        }
        let Some(settings) = &self.settings else {
            return self.adapter_command_line(user_command_line);
        };
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
        let mut tokens = vec![
            strip(user_command_line).to_owned(),
            "nvx_sandbox=1".to_owned(),
        ];
        tokens.extend(self.layout().tokens());
        tokens.extend([
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
        let BootShares {
            tokens: shares,
            fragment,
            ..
        } = self.shares(delivery)?;
        tokens.extend(shares);
        tokens.retain(|token| !token.is_empty());
        let command_line = tokens.join(" ");
        // OpenVMM appends the shares' bootstrap tokens after this command line, so they consume
        // the same x86 budget.
        if command_line.len() + fragment.len() + 1 > SANDBOX_COMMAND_LINE_MAX_SIZE {
            // Only boot tokens put the shares' targets on the command line.
            let advice = match delivery {
                Delivery::MappingTable => "use shorter arguments and kernel parameters",
                #[cfg(any(test, feature = "cli"))]
                Delivery::BootTokens => {
                    "attach fewer --mount shares or use shorter targets and arguments"
                }
            };
            return Err(refused(format!(
                "sandbox kernel command line exceeds its 1024-byte x86 budget; {advice}"
            )));
        }
        let layout = RootLayout::parse(command_line.as_bytes()).map_err(|error| {
            refused(format!(
                "sandbox kernel command line conflicts with the boot tokens of its root: {error}"
            ))
        })?;
        debug_assert_eq!(layout, Some(self.layout()));
        Ok(command_line)
    }

    /// Implements [`kernel_command_line`](Self::kernel_command_line) for an adapter's plan.
    fn adapter_command_line(&self, user_command_line: &str) -> Result<String> {
        for token in split(user_command_line) {
            let name = token.split('=').next().unwrap_or(token);
            if is_root_parameter(name) {
                return Err(refused(format!("{name} is owned by the plan's root")));
            }
            if token.starts_with("virtfs_") {
                return Err(refused(format!("{name} is owned by OpenVMM's shares")));
            }
        }
        let mut tokens = vec![strip(user_command_line).to_owned()];
        tokens.extend(self.layout().tokens());
        tokens.retain(|token| !token.is_empty());
        let command_line = tokens.join(" ");
        if command_line.len() + 1 > SANDBOX_COMMAND_LINE_MAX_SIZE {
            return Err(refused(
                "the kernel command line of an adapter's plan exceeds its 1024-byte x86 budget",
            ));
        }
        let layout = RootLayout::parse(command_line.as_bytes()).map_err(|error| {
            refused(format!(
                "the kernel command line of an adapter's plan conflicts with the boot tokens of \
                 its root: {error}"
            ))
        })?;
        debug_assert_eq!(layout, Some(self.layout()));
        Ok(command_line)
    }
}

/// Returns whether `name` is that of a kernel parameter of the boot tokens of a plan's root, which
/// [`RootLayout::tokens`] renders: `nvx_layer`, `nvx_scratch`, or `nvx_overlay_upper`.
fn is_root_parameter(name: &str) -> bool {
    [LAYER_TOKEN, SCRATCH_TOKEN, RAM_UPPER_TOKEN]
        .iter()
        .any(|token| token.split('=').next() == Some(name))
}

/// Returns the image that a distro binding records: the staged format, and the identity that the
/// guest expects of the layer.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for an identity that is
/// not a UUID in its canonical text.
pub(crate) fn bound_image(binding: &DistroBinding) -> Result<RootImage> {
    image_of(binding.format(), binding.identity())
}

/// Returns the image of a layer of `format` whose binding names `identity`, as [`bound_image`]
/// does.
pub(crate) fn image_of(format: DistroFormat, identity: &str) -> Result<RootImage> {
    let identity = Uuid::parse(identity).ok_or_else(|| {
        refused(format!(
            "the distro binding names the identity {}, which is not a UUID in its canonical text",
            repr(identity)
        ))
    })?;
    Ok(match format {
        DistroFormat::Erofs => RootImage::erofs(Some(identity)),
        DistroFormat::Gpt => RootImage::gpt(Some(identity)),
    })
}

#[cfg(test)]
mod tests;
