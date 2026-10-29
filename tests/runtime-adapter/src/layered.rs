//! The host program's VMs with a layered root: an image, on the `distro` block device, under an
//! upper, which the guest agent's worker assembles outside PID 1.
//!
//! The host stages the images of a root, with their bindings, in the sandbox's store before it
//! launches them; see [`crate::staged`]. Staging is where hashing belongs: a launch never hashes.
//!
//! The host plans the VM's block devices and the boot tokens of its root with NVX's typed plan of
//! a cold layered launch for an adapter's own guest, `nvx::host::sandbox::SandboxPlan::adapter`,
//! which names none of the settings of NVX's sandbox profile, such as a workload identity, since
//! the guest agent runs its executions itself. Before it spawns the VM, a launch checks the staged
//! bindings of its images from their metadata alone, which claims the scratch image for the
//! sandbox, and hands OpenVMM the files that the checks opened, through NVX's `PreparedLaunch`,
//! rather than their paths, which something else could take after the checks; see
//! [`LayeredLaunch::prepare`]. It launches the VM through NVX's supervisor, as it launches VMs
//! without a layered root, but without NVX's managed lifecycle; see
//! [`LayeredLaunch::openvmm_arguments`]. The boot tokens carry only what NVX's boot contract
//! defines: the image's format and the identity that the guest expects of it, and the upper.
//! Everything else that the root needs goes in one root configuration, a [`RootConfig`], which the
//! host sends once it attaches.
//!
//! A launch may also share host directories and files with the root, as
//! [`HostFsSharing`](crate::sharing::HostFsSharing): each at an absolute guest path in the root,
//! read-only or read-write, inside the host directories that the adapter authorizes, with host
//! paths hidden inside them, and paths inside hidden ones shown again, and never at the guest
//! targets that the root keeps for itself; see [`crate::sharing`]. NVX's `filesystem::plan` plans
//! their export, an `ExportPlan`, which the launch attaches itself, since an adapter's plan holds
//! none: OpenVMM exports the shares through its aggregate virtio-fs device, and appends the boot
//! tokens with which the guest mounts the export, out of the root. The root configuration
//! announces how many mappings the export's table holds, which the host sends once the guest is
//! ready; see [`LayeredLaunch::table`].
//!
//! A launch may give the VM a network: a `SandboxNetwork`, the guest's address and the policy that
//! OpenVMM enforces there, which [`read_network`] reads from a file in the serde form of a plan's
//! network, and which [`LayeredLaunch::with_network`] puts on the plan. The plan renders its
//! OpenVMM arguments through NVX's one lowering of a policy, `nvx::host::network::openvmm_arguments`,
//! as it renders those of NVX's sandbox profile. OpenVMM appends the guest's network settings to
//! the kernel command line itself, and NVX's `/init` configures the network device and its
//! addresses from them, and starts the announcer of the guest's listeners if OpenVMM publishes
//! them, before it hands the VM to the guest agent, which leaves all three to `/init`.
//!
//! The guest's executions follow the network. The root configuration says whether the guest's
//! listeners are published, as OpenVMM publishes them: to the host's private networks with
//! ingress allowed, or on host loopback with host-loopback access allowed. The executions enter
//! the root with that publication, which refuses `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while
//! listeners are published, since either could forge an announcement. The host program's `exec`
//! gives each execution the URL of a proxy-only network's proxy, as the guest sees it, in its
//! environment, with [`with_proxy`], from the plan that [`LaunchRecord::read_plan`] reads back.
//!
//! The host records what it launched in the sandbox's store, beside NVX's runtime records, as a
//! [`LaunchRecord`], with the bindings that the launch checked, and the plan, which keeps the
//! network.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;
use std::time::Instant;

use nvx::host::artifacts::absolute;
use nvx::host::filesystem::ExportPlan;
use nvx::host::launch::DEFAULT_MEMORY_MIB;
use nvx::host::network::{Access, HostLoopback};
use nvx::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
use nvx::host::sandbox::scratch::{CLAIM_RELEASE_WAIT, ScratchBinding};
use nvx::host::sandbox::{
    LayerRole, PreparedLaunch, SandboxDistro, SandboxLayer, SandboxNetwork, SandboxPlan,
    SandboxUpper,
};
use nvx::protocol::HostMap;
use nvx::protocol::root::{ImageFormat, RootImage};
use nvx::uuid::Uuid;
use nvx::{Error, Hypervisor, Result};
use serde_json::{Value, json};

use crate::exec::Start;
use crate::root::{Listeners, RootConfig, encode_maps};
use crate::staged::invalid;

/// The name of the record of a launch in the sandbox's store.
pub const RECORD_NAME: &str = "runtime-adapter.json";
/// The format of the record of a launch that this build writes.
pub const RECORD_FORMAT: u64 = 1;
/// The kernel parameter with which NVX's `/init` hands a VM to NVX's init agent, which the
/// adapter's initramfs replaces with the guest agent; see [`crate::initramfs`]. An adapter's plan
/// renders none of the parameters of NVX's sandbox profile, so the launch passes it as its own.
pub const SANDBOX_TOKEN: &str = "nvx_sandbox=1";
/// The environment variables that hold the URL of a network's proxy for an execution, as
/// [`with_proxy`] sets them: those of HTTP clients, in lowercase and in uppercase.
pub const PROXY_VARIABLES: [&str; 4] = ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"];
/// The size of a sector of a block device, in bytes.
const SECTOR_SIZE: u64 = 512;

/// A VM with a layered root: the plan of its image, its upper, and their boot tokens, the root
/// configuration that the host sends its guest agent, the bindings that the sandbox's store
/// stages for its images, which the launch checks, and the export of the host paths that it
/// shares with the root, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayeredLaunch {
    /// The adapter's plan of the VM's block devices, of its network, if it has one, and of the boot
    /// tokens of its root.
    pub plan: SandboxPlan,
    /// The root configuration that the host sends the guest agent once it attaches.
    pub config: RootConfig,
    /// The bindings that the sandbox's store stages for the plan's images, as
    /// [`Staged::bindings`](crate::staged::Staged::bindings) finds them, which
    /// [`prepare`](Self::prepare) checks, and the launch records.
    pub bindings: Bindings,
    /// The export of the host paths that the launch shares with the root, which
    /// [`share`](Self::share) sets, or `None` if it shares none.
    export: Option<ExportPlan>,
}

impl LayeredLaunch {
    /// Returns the launch of the image at `distro`, in `format`, of which the guest expects the
    /// identity `identity`, over the ext4 scratch image at `scratch`, or over RAM without one,
    /// without bindings, without shares, and without a network, which
    /// [`with_network`](Self::with_network) puts on the plan.
    ///
    /// The root configuration takes its defaults, with the runtime directory `runtime` instead, if
    /// any, as it is, so that the guest checks it, with the size of each image, if it is a whole
    /// number of sectors, which the guest checks against its block device, and with the guest's
    /// listeners unpublished, as they are without a network.
    ///
    /// Fails with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) if a path is not
    /// Unicode, and otherwise as `SandboxPlan::validated` does, such as for an image that is not a
    /// regular file.
    pub fn new(
        distro: &Path,
        format: ImageFormat,
        identity: Uuid,
        scratch: Option<&Path>,
        runtime: Option<&str>,
    ) -> Result<Self> {
        let plan = plan(distro, format, identity, scratch)?;
        let mut config = RootConfig {
            distro_sectors: sectors(plan.distro().path())?,
            scratch_sectors: plan
                .upper()
                .scratch_path()
                .map(sectors)
                .transpose()?
                .flatten(),
            listeners: Some(listeners(None)),
            ..RootConfig::default()
        };
        if let Some(runtime) = runtime {
            runtime.clone_into(&mut config.runtime);
        }
        Ok(Self {
            plan,
            config,
            bindings: Bindings::default(),
            export: None,
        })
    }

    /// Shares the host paths of `export`, which
    /// [`HostFsSharing::plan`](crate::sharing::HostFsSharing::plan) plans, with the root: the
    /// launch attaches the export, and the root configuration announces how many mappings its
    /// table holds, which the host sends once the guest is ready; see [`table`](Self::table).
    ///
    /// Fails with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) for an export whose
    /// table the host cannot send, as `encode_maps` refuses it: one of more mappings than the
    /// guest takes, or with a source or a target that the guest refuses.
    pub fn share(&mut self, export: ExportPlan) -> Result<()> {
        let table = export.guest_table();
        encode_maps(&table).map_err(|error| Error::unsupported(error.to_string()))?;
        self.config.mapping_count = u32::try_from(table.len())
            .map_err(|_| Error::unsupported(format!("the export maps {} paths", table.len())))?;
        self.export = Some(export);
        Ok(())
    }

    /// Returns the export of the host paths that the launch shares with the root, which
    /// [`share`](Self::share) attached, or `None` if it shares none.
    pub fn export(&self) -> Option<&ExportPlan> {
        self.export.as_ref()
    }

    /// Returns the mapping table of the launch's export, in the order in which the guest applies
    /// it, which `ExportPlan::guest_table` gives: each mapping before those inside its target. A
    /// launch that shares nothing has an empty table.
    pub fn table(&self) -> Vec<HostMap> {
        self.export
            .as_ref()
            .map_or_else(Vec::new, ExportPlan::guest_table)
    }

    /// Returns the launch with `network` on its plan, with `SandboxPlan::with_network`, or without
    /// a network, and with a root configuration that says whether the guest's listeners are
    /// published through it: as OpenVMM publishes them, with ingress allowed, to the host's
    /// private networks, or with host-loopback access allowed, on host loopback.
    #[must_use]
    pub fn with_network(mut self, network: Option<SandboxNetwork>) -> Self {
        self.config.listeners = Some(listeners(network.as_ref()));
        self.plan = self.plan.with_network(network);
        self
    }

    /// Returns the VM's kernel command line: [`SANDBOX_TOKEN`], then the boot tokens of the
    /// plan's root, without parameters of the caller.
    ///
    /// Fails as `SandboxPlan::kernel_command_line` does.
    pub fn kernel_command_line(&self) -> Result<String> {
        self.plan.kernel_command_line(SANDBOX_TOKEN)
    }

    /// Checks the [bindings](Self::bindings) of the plan's images from their metadata alone, and
    /// the [export](Self::export) of the launch's shares, if any, and prepares the launch of the
    /// files that the checks opened, as NVX's `PreparedLaunch`.
    ///
    /// NVX's `ExportPlan::verify` checks that the host still keeps the export's read-only mappings
    /// read-only, and that its directories and files, and the mapped and hidden paths that the
    /// guest could move, still name what they named when the export was planned. NVX's
    /// `DistroBinding::check` checks the image against its binding, under the distro policy
    /// that this build accepts, and `ScratchBinding::check` checks the scratch image of an upper
    /// on scratch against its own, and claims it for this sandbox, waiting `CLAIM_RELEASE_WAIT`
    /// for a claim that should be free. Neither reads the images. The prepared launch renders the
    /// plan's OpenVMM arguments, which name the checked files rather than their paths, and the
    /// [kernel command line](Self::kernel_command_line).
    ///
    /// The caller hands OpenVMM the checked files, with
    /// [`openvmm_arguments`](Self::openvmm_arguments) and `PreparedLaunch::inherited` as the
    /// allowlist of its `VmmCommand`, and holds the prepared launch until the guest has assembled
    /// the root, or until it has torn down a launch that failed. On Linux, OpenVMM opens the
    /// checked files through the descriptors that it inherits, whatever has taken their paths
    /// since the checks; on Windows, it opens them by their final paths, which the checked files
    /// keep anything else from taking until they are closed.
    ///
    /// Fails with [`ErrorKind::Unavailable`](nvx::ErrorKind::Unavailable) for an image or a scratch
    /// image that the store does not stage, a binding that no longer holds, as for an image that
    /// changed or was replaced after it was staged, and a missing image; with
    /// [`ErrorKind::TimedOut`](nvx::ErrorKind::TimedOut), and the code
    /// [`ErrorCode::ScratchClaimed`](nvx::ErrorCode::ScratchClaimed), for a scratch image that
    /// another launch still claims; and otherwise as `ExportPlan::verify`, the checks, and
    /// `PreparedLaunch::new` fail.
    pub fn prepare(&self) -> Result<PreparedLaunch> {
        if let Some(export) = &self.export {
            export.verify()?;
        }
        let distro = self.plan.distro();
        let binding = self
            .bindings
            .distro
            .as_ref()
            .ok_or_else(|| unstaged("image", distro.path()))?;
        let RootImage { format, identity } = distro.image();
        let identity = identity
            .ok_or_else(|| Error::unsupported("the plan names no identity of its image"))?;
        let checked = binding.check(
            &layer(distro.path(), format, identity)?,
            DISTRO_POLICY_REVISION,
        )?;
        let scratch = match self.plan.upper().scratch_path() {
            Some(path) => {
                let binding = self
                    .bindings
                    .scratch
                    .as_ref()
                    .ok_or_else(|| unstaged("scratch image", path))?;
                Some(binding.check(path, Instant::now() + CLAIM_RELEASE_WAIT)?)
            }
            None => None,
        };
        PreparedLaunch::new(self.plan.clone(), checked, scratch, SANDBOX_TOKEN)
    }

    /// Returns OpenVMM's arguments for the VM of `prepared`, the launch that
    /// [`prepare`](Self::prepare) prepared: its arguments, which attach the checked files as the
    /// plan's block devices, and the plan's network device, with the policy that NVX's lowering
    /// renders, if the plan has a network that lets anything through, those of the
    /// [export](Self::export) of the launch's shares, if any, which `ExportPlan::openvmm_arguments`
    /// renders, then the VM in one process on `hypervisor`, NVX's default memory, the kernel at
    /// `kernel`, the initramfs at `initramfs`, its kernel command line, which is the
    /// [launch's](Self::kernel_command_line), no virtio console, the control console listening on
    /// `endpoint` with the launch capability on standard input, and the outcome report at `report`.
    /// OpenVMM appends the boot tokens with which the guest mounts the export to the kernel command
    /// line.
    ///
    /// Unlike those of NVX's sandbox start, they name no workload identity, which the adapter's
    /// plan does not hold, and no managed lifecycle: OpenVMM only checks the arguments of the
    /// managed lifecycle and labels its outcome report with it, and only NVX's init agent, which
    /// the guest agent replaces, reads its boot token.
    ///
    /// The arguments name the checked files of this launch alone: never persist them.
    pub fn openvmm_arguments(
        &self,
        prepared: &PreparedLaunch,
        hypervisor: Hypervisor,
        kernel: &Path,
        initramfs: &Path,
        endpoint: &str,
        report: &Path,
    ) -> Vec<OsString> {
        debug_assert_eq!(prepared.plan(), &self.plan, "the launch prepared its plan");
        let export = self
            .export
            .as_ref()
            .map(ExportPlan::openvmm_arguments)
            .unwrap_or_default();
        let mut arguments = prepared
            .openvmm_arguments()
            .iter()
            .chain(&export)
            .map(OsString::from)
            .collect::<Vec<_>>();
        let memory = format!("{DEFAULT_MEMORY_MIB}M");
        let command_line = prepared.kernel_command_line();
        let control = format!("listen={endpoint}");
        let options = [
            OsStr::new("--single-process"),
            OsStr::new("--hypervisor"),
            OsStr::new(hypervisor.as_str()),
            OsStr::new("--memory"),
            OsStr::new(&memory),
            OsStr::new("--kernel"),
            kernel.as_os_str(),
            OsStr::new("--initrd"),
            initramfs.as_os_str(),
            OsStr::new("--cmdline"),
            OsStr::new(command_line),
            OsStr::new("--virtio-console"),
            OsStr::new("none"),
            OsStr::new("--microvm-control-console"),
            OsStr::new(&control),
            OsStr::new("--microvm-control-auth-stdin"),
            OsStr::new("--microvm-report"),
            report.as_os_str(),
        ];
        arguments.extend(options.map(OsStr::to_os_string));
        arguments
    }
}

/// Reads the network of a launch from the file at `path`: one `SandboxNetwork`, in the serde form
/// of a plan's network, as NVX's plan persists it, which [`LayeredLaunch::with_network`] puts on
/// the adapter's plan. Without a network, a launch has no network device.
///
/// The form requires every member and refuses any other, and reading it checks the network as
/// `SandboxNetwork::new` does: the guest's address must leave a gateway, and the policy must lower
/// to OpenVMM's arguments, as [`LayeredLaunch::openvmm_arguments`] renders them. So a launch that
/// reads its network first refuses one that the plan refuses before it launches anything.
///
/// Fails with [`ErrorKind::Io`](nvx::ErrorKind::Io) if the file cannot be read, and with
/// [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported), whose cause says why, if the file does
/// not hold a network in that form, or the plan refuses it.
pub fn read_network(path: &Path) -> Result<SandboxNetwork> {
    let bytes = fs::read(path).map_err(|error| {
        Error::io(format!("cannot read the network {}", path.display())).with_source(error)
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        Error::unsupported(format!("the network {} is refused", path.display())).with_source(error)
    })
}

/// Returns whether the guest's listeners are published through `network`, or without one, as
/// OpenVMM publishes them: with ingress allowed, to the host's private networks, or with
/// host-loopback access allowed, on host loopback. OpenVMM tells NVX's `/init` so on the kernel
/// command line, and `/init` then starts the announcer of the guest's listeners.
fn listeners(network: Option<&SandboxNetwork>) -> Listeners {
    let published = network.is_some_and(|network| {
        let policy = network.policy();
        policy.ingress == Access::Allow || policy.host_loopback == HostLoopback::Allow
    });
    if published {
        Listeners::Published
    } else {
        Listeners::Unpublished
    }
}

/// Returns `start` with the URL of the proxy of `plan`'s network, as the guest sees it,
/// `SandboxPlan::proxy_url`, in each of the [`PROXY_VARIABLES`], after the start's own entries:
/// NVX gives an execution no proxy settings, and an execution's environment is exactly its
/// start's. A start of a plan without a proxy, or that names any of those variables, whose caller
/// chose its own proxy settings, is returned as it is.
pub fn with_proxy(mut start: Start, plan: &SandboxPlan) -> Start {
    let Some(url) = plan.proxy_url() else {
        return start;
    };
    let named = start.environment.iter().any(|entry| {
        let name = entry.split(|byte| *byte == b'=').next().unwrap_or_default();
        PROXY_VARIABLES
            .iter()
            .any(|variable| name == variable.as_bytes())
    });
    if !named {
        let entries = PROXY_VARIABLES.map(|variable| format!("{variable}={url}").into_bytes());
        start.environment.extend(entries);
    }
    start
}

/// Returns the adapter's plan of the image at `distro`, in `format`, of which the guest expects
/// the identity `identity`, over the ext4 scratch image at `scratch`, or over RAM without one,
/// checked against the host with `SandboxPlan::validated`.
///
/// OpenVMM runs in the sandbox's store, so the plan names the images by absolute paths, which it
/// normalizes as `pathlib` does.
pub(crate) fn plan(
    distro: &Path,
    format: ImageFormat,
    identity: Uuid,
    scratch: Option<&Path>,
) -> Result<SandboxPlan> {
    let distro = absolute(distro)?;
    let scratch = scratch.map(absolute).transpose()?;
    let upper = match &scratch {
        Some(scratch) => SandboxUpper::scratch(unicode(scratch)?),
        None => SandboxUpper::RAM,
    };
    let image = RootImage {
        format,
        identity: Some(identity),
    };
    let plan = SandboxPlan::adapter(SandboxDistro::new(unicode(&distro)?, image), upper);
    plan.validated()?;
    Ok(plan)
}

/// Returns the distro layer of the image at `path`, in `format`, of which the guest expects the
/// identity `identity`, which NVX's bindings of the image name.
pub(crate) fn layer(path: &Path, format: ImageFormat, identity: Uuid) -> Result<SandboxLayer> {
    let path = unicode(path)?;
    let identity = identity.to_string();
    Ok(match format {
        ImageFormat::Erofs => SandboxLayer::new(LayerRole::Distro, path, identity),
        ImageFormat::Gpt => SandboxLayer::gpt(LayerRole::Distro, path, identity),
    })
}

/// Returns `path` as text, which the plan takes.
pub(crate) fn unicode(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        Error::unsupported(format!("the path {} is not Unicode text", path.display()))
    })
}

/// Returns the error of a launch whose `what`, its image or its scratch image, at `path`, the
/// sandbox's store does not stage.
pub(crate) fn unstaged(what: &str, path: &Path) -> Error {
    Error::unavailable(format!(
        "the sandbox's store stages no {what} at {}",
        path.display()
    ))
}

/// Returns the size of the file at `path` in sectors, if it is a whole number of them.
fn sectors(path: &Path) -> Result<Option<u64>> {
    let length = fs::metadata(path)
        .map_err(|error| {
            Error::unavailable(format!("cannot read the size of {}", path.display()))
                .with_source(error)
        })?
        .len();
    Ok(length
        .is_multiple_of(SECTOR_SIZE)
        .then_some(length / SECTOR_SIZE))
}

/// The bindings of a launch's images that the sandbox's store stages, as
/// [`Staged::bindings`](crate::staged::Staged::bindings) finds them, which
/// [`LayeredLaunch::prepare`] checks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bindings {
    /// The binding of the image, if the store stages the launch's image, which every launch needs.
    pub distro: Option<DistroBinding>,
    /// The binding of the scratch image, if the launch's upper is on a scratch image that the
    /// store stages, which a launch over a scratch image needs.
    pub scratch: Option<ScratchBinding>,
}

impl Bindings {
    /// Returns the bindings as JSON: an object of the serde forms of the image's binding and of the
    /// scratch image's, each `null` without one.
    ///
    /// Fails with [`ErrorKind::Other`](nvx::ErrorKind::Other) if a binding has no JSON form.
    pub fn to_json(&self) -> Result<Value> {
        let distro = self.distro.as_ref().map(serde_json::to_value);
        let scratch = self.scratch.as_ref().map(serde_json::to_value);
        Ok(json!({
            "distro": distro.map(binding_json).transpose()?,
            "scratch": scratch.map(binding_json).transpose()?,
        }))
    }
}

/// Returns the JSON form that `form` holds for a binding.
///
/// Fails with [`ErrorKind::Other`](nvx::ErrorKind::Other) if the binding has none.
pub(crate) fn binding_json(form: serde_json::Result<Value>) -> Result<Value> {
    form.map_err(|error| Error::other("a binding has no JSON form").with_source(error))
}

/// Writes `record` as JSON, with a newline, at `path`, in place of an earlier record.
///
/// Fails with [`ErrorKind::Io`](nvx::ErrorKind::Io) if the record cannot be written.
pub(crate) fn write_record(path: &Path, record: &Value) -> Result<()> {
    let mut text = record.to_string();
    text.push('\n');
    fs::write(path, text)
        .map_err(|error| Error::io(format!("cannot write {}", path.display())).with_source(error))
}

/// What the host program launched in a sandbox, which it records in the sandbox's store, in
/// [`RECORD_NAME`], before it spawns the VMM. The record is JSON in format [`RECORD_FORMAT`]:
///
/// ```json
/// {"format": 1, "mode": "layered", "kernelCommandLine": "nvx_sandbox=1 nvx_layer=...",
///  "plan": PLAN, "bindings": {"distro": DISTRO_BINDING, "scratch": SCRATCH_BINDING},
///  "rootConfiguration": "version=1\nruntime=/run/runtime-adapter\n..."}
/// ```
///
/// The `mode` is `layered`, or `blockless` for a VM without a layered root, whose `plan`,
/// `bindings`, and `rootConfiguration` are `null`, or `clone` for a clone's record, which
/// [`CloneRecord`](crate::clone::CloneRecord) writes, with the members of a layered root's, but a
/// `rootConfiguration` of `null`, and the template that the clone leases. The `plan` is the
/// persisted form of the VM's `SandboxPlan`, which [`read_plan`](Self::read_plan) reads back,
/// whose `network` keeps the VM's network, in the form that [`read_network`] reads, or `null`
/// without one. The `bindings` are those that the launch checked, which the store stages for the
/// plan's images, as [`Staged::bindings`](crate::staged::Staged::bindings) finds them, in the serde
/// forms of NVX's `DistroBinding` and `ScratchBinding`; the scratch image's is `null` for an upper
/// in RAM, which has no scratch image. The `rootConfiguration` is the encoding of the root
/// configuration that the host sends, as it sends it, whether or not the guest accepts it.
#[derive(Clone, Copy, Debug)]
pub struct LaunchRecord<'a> {
    /// The VM's kernel command line.
    pub kernel_command_line: &'a str,
    /// The VM's layered root, if it has one.
    pub layered: Option<&'a LayeredLaunch>,
}

impl LaunchRecord<'_> {
    /// Returns the record as JSON.
    ///
    /// Fails with [`ErrorKind::Other`](nvx::ErrorKind::Other) if the plan or a binding has no
    /// JSON form.
    pub fn to_json(&self) -> Result<Value> {
        let (mode, plan, bindings, config) = match self.layered {
            Some(layered) => {
                let plan = serde_json::to_value(&layered.plan).map_err(|error| {
                    Error::other("the plan of the layered root has no JSON form").with_source(error)
                })?;
                let config = String::from_utf8_lossy(&layered.config.encode()).into_owned();
                (
                    "layered",
                    plan,
                    layered.bindings.to_json()?,
                    Value::String(config),
                )
            }
            None => ("blockless", Value::Null, Value::Null, Value::Null),
        };
        Ok(json!({
            "format": RECORD_FORMAT,
            "mode": mode,
            "kernelCommandLine": self.kernel_command_line,
            "plan": plan,
            "bindings": bindings,
            "rootConfiguration": config,
        }))
    }

    /// Writes the record in `directory`, the sandbox's store, in place of an earlier one.
    ///
    /// Fails as [`to_json`](Self::to_json) does, and with
    /// [`ErrorKind::Io`](nvx::ErrorKind::Io) if the record cannot be written.
    pub fn write(&self, directory: &Path) -> Result<()> {
        write_record(&directory.join(RECORD_NAME), &self.to_json()?)
    }

    /// Reads the plan of the launch that `directory`, the sandbox's store, records: the plan of
    /// the VM's layered root, with its network, or `None` for a VM without one.
    ///
    /// Fails with [`ErrorKind::Io`](nvx::ErrorKind::Io) if the record cannot be read, as when the
    /// store holds none, and with [`ErrorKind::Unavailable`](nvx::ErrorKind::Unavailable) if it
    /// is not JSON, is not in format [`RECORD_FORMAT`], or has no plan in a plan's serde form.
    pub fn read_plan(directory: &Path) -> Result<Option<SandboxPlan>> {
        let path = directory.join(RECORD_NAME);
        let text = fs::read_to_string(&path).map_err(|error| {
            Error::io(format!("cannot read {}", path.display())).with_source(error)
        })?;
        let unreadable = || {
            Error::unavailable(format!(
                "cannot read the plan of the launch record {}",
                path.display()
            ))
        };
        let value = serde_json::from_str::<Value>(&text)
            .map_err(|error| unreadable().with_source(error))?;
        if value.get("format").and_then(Value::as_u64) != Some(RECORD_FORMAT) {
            let problem = format!("is not in format {RECORD_FORMAT}");
            return Err(unreadable().with_source(invalid(&problem)));
        }
        match value.get("plan") {
            None => Err(unreadable().with_source(invalid("has no plan"))),
            Some(Value::Null) => Ok(None),
            Some(plan) => serde_json::from_value(plan.clone())
                .map(Some)
                .map_err(|error| unreadable().with_source(error)),
        }
    }
}
