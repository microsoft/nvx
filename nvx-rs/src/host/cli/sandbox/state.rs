//! The CLI's typed `config.json`, in format 10 only: one EROFS or GPT image by path,
//! a scratch upper by path or a RAM upper with its explicit cap, the image bindings,
//! and the workload's view, network and export.
//!
//! Provision checks the same [`VmConfig`] that start reconstructs. Older records, those of format
//! 9 among them, are refused, not migrated, with the advice to deprovision the sandbox and
//! provision it again. Bindings still check the recorded caller-owned paths before launch, and the
//! caller keeps those paths stable while the VM uses them.
//!
//! The network is the [`NetworkRecord`] of the sandbox's network options, from which each start
//! builds the network's policy again, as provision built it.
//!
//! The configuration of a restored sandbox also records its [`Source`]: the name of its snapshot
//! under `source_snapshot`, the absolute path of the snapshot's store under `source_store`, and
//! the program and arguments of the workload that each restore runs under `workload`, which
//! [`Config::source`] reads back for `start` to restore the sandbox and for `deprovision` to
//! remove its view of the snapshot. That of a sandbox that boots cold records none of them.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::lifecycle::ProvisionConfig;
use super::{MAX_MEMORY_MIB, path_string, sandbox_network};
use crate::ErrorCode;
use crate::activation::{Journal, MachineIdMode, OverlayMode, View};
use crate::host::cli::json::{Object, pretty_serde, read_object};
use crate::host::cli::network::{NativeRules, NetworkOptions};
use crate::host::cli::output::LINE_SEPARATOR;
use crate::host::cli::snapshots::Name as SnapshotName;
use crate::host::cli::{Exit, Result, io_error, io_message};
use crate::host::config::{
    Boot, ConsoleAttachment, LayeredRoot, NetworkConfig, RootConfig, UpperConfig, VmConfig,
};
use crate::host::filesystem::ExportPlan;
use crate::host::paths;
use crate::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding, DistroFormat};
use crate::host::sandbox::layer::{LayerRole, SandboxLayer};
use crate::host::sandbox::scratch::ScratchBinding;
use crate::host::sandbox::{SandboxLaunch, SandboxSettings, SandboxUpper};
use crate::protocol::root::{RAM_UPPER_CAPS, RootImage};
use crate::uuid::Uuid;

pub(crate) use crate::host::cli::{hex, random_bytes, temporary_path};

#[cfg(test)]
mod tests;

/// The typed image and upper configuration that provision writes and start reads.
pub(crate) const CONFIG_FORMAT: u64 = 10;
/// Every configuration format that `start` reads.
pub(crate) const CONFIG_FORMATS: [u64; 1] = [CONFIG_FORMAT];
/// What messages call a configuration.
const DESCRIPTION: &str = "sandbox configuration";

/// Atomically replaces `path` with the JSON `text` and a line end, with the line separator of the
/// host for each `\n`, in a temporary file of mode 0600 that then replaces `path`.
pub(crate) fn write_json(path: &Path, text: &str) -> Result {
    let temporary = temporary_path(path)?;
    let text = format!("{text}\n").replace('\n', LINE_SEPARATOR);
    let written = (|| -> Result {
        let failed = |error: std::io::Error| io_error(ErrorCode::Io, &error, &temporary);
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file = options
            .open(&temporary)
            .and_then(|mut file| file.write_all(text.as_bytes()).map(|()| file))
            .map_err(failed)?;
        file.flush().map_err(failed)?;
        drop(file);
        set_private(&temporary)?;
        std::fs::rename(&temporary, path).map_err(|error| {
            Exit::error(
                ErrorCode::Io,
                format!("{temporary:?} -> {}", io_message(&error, path)),
            )
        })
    })();
    let _ = std::fs::remove_file(&temporary);
    written
}

/// Gives `path` mode 0600; on Windows, that only clears the read-only attribute.
fn set_private(path: &Path) -> Result {
    let error = |error: std::io::Error| io_error(ErrorCode::Io, &error, path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(error)
    }
    #[cfg(not(unix))]
    {
        let mut permissions = std::fs::metadata(path).map_err(error)?.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            std::fs::set_permissions(path, permissions).map_err(error)?;
        }
        Ok(())
    }
}

/// Returns `path` resolved, as [`paths::resolve`] resolves it, as text.
fn resolved(path: &Path) -> Result<String> {
    path_string(path)?;
    path_string(&paths::resolve(path).map_err(Exit::from_os_error)?)
}

/// The distro layer that `provision` records, at the path that it records for it, with the
/// binding of the layer at that path, which `start` checks.
pub(crate) struct RecordedDistro {
    /// The image at its resolved path, in its actual format.
    image: Image,
    /// The binding of the layer at that path.
    binding: DistroBinding,
}

impl RecordedDistro {
    /// Returns the distro layer of `launch` at the path that `provision` records, the path given,
    /// resolved, with a binding of the layer there: `binding`, which explicit
    /// verification of the layer at the path given made, if it holds at the recorded path, and
    /// otherwise the binding that explicit verification of the layer at the recorded path makes,
    /// which must then hold there.
    ///
    /// The check of `binding` at the recorded path reads nothing, and passes unless the paths name
    /// different manifests: when the path given is a link to a layer of another name, `start`
    /// checks the manifest beside the link's target, so `provision` verifies the layer there too.
    pub(crate) fn new(launch: &SandboxLaunch, binding: DistroBinding) -> Result<Self> {
        let distro = launch.distro();
        let path = resolved(distro.path())?;
        let image = Image {
            path,
            format: distro.format(),
            identity: distro.uuid().to_owned(),
        };
        let layer = image.layer();
        let binding = match binding.check(&layer, DISTRO_POLICY_REVISION) {
            Ok(_) => binding,
            Err(_) => {
                let binding = DistroBinding::verify(&layer)?;
                // Verification counts a manifest path whose lookup fails as no manifest, which
                // the check refuses: provision records no binding that every start would refuse.
                binding.check(&layer, DISTRO_POLICY_REVISION)?;
                binding
            }
        };
        Ok(Self { image, binding })
    }
}

/// The scratch image that `provision` records, at the path that it records for it, with the
/// binding of the image at that path, which `start` checks and claims.
pub(crate) struct RecordedScratch {
    /// The image's path, resolved, as `provision` records it.
    path: String,
    /// The binding of the image at that path.
    binding: ScratchBinding,
}

impl RecordedScratch {
    /// Returns the scratch image of `launch` at the path that `provision` records, the path given,
    /// resolved, with a binding of the image there, which claims nothing, or `None` for a launch
    /// whose upper is in RAM, which has no scratch image.
    pub(crate) fn new(launch: &SandboxLaunch) -> Result<Option<Self>> {
        let Some(scratch) = launch.scratch() else {
            return Ok(None);
        };
        let path = resolved(scratch)?;
        let binding = ScratchBinding::bind(Path::new(&path))?;
        Ok(Some(Self { path, binding }))
    }
}

/// The one image of a provisioned root.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    format: DistroFormat,
    path: String,
    identity: String,
}

impl Image {
    fn layer(&self) -> SandboxLayer {
        match self.format {
            DistroFormat::Erofs => SandboxLayer::new(LayerRole::Distro, &self.path, &self.identity),
            DistroFormat::Gpt => SandboxLayer::gpt(LayerRole::Distro, &self.path, &self.identity),
        }
    }
}

/// The upper's kind and exactly the fields that it needs.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Upper {
    Scratch { path: String },
    Ram { max_memory_percent: u8 },
}

/// The named snapshot from which each start of a restored sandbox restores it, and the workload
/// that each restore runs: the only association between a snapshot and the sandboxes that it
/// supplies, which their own configurations record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Source {
    /// The snapshot's name.
    pub(crate) snapshot: SnapshotName,
    /// The absolute path of the snapshot's store.
    pub(crate) store: PathBuf,
    /// The program, an absolute guest path, and the arguments of the workload.
    pub(crate) workload: Vec<String>,
}

/// The network options of a sandbox that `provision` records, from which each `start` builds the
/// network again with [`sandbox_network`], as `provision` built it: `--net`, `--network-egress`,
/// `--network-ingress`, `--host-loopback`, `--network-proxy`, and the `--host-loopback-forward`
/// options, as given, and the egress rules in OpenVMM's form, as
/// [`NetworkOptions::egress_rules`] resolved them, from a policy file's compile if one was given,
/// which no start reads again. The record requires every field, an option that was not given as
/// `null`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NetworkRecord {
    /// `--net`: the guest's address and prefix.
    net: String,
    /// `--network-egress`, if given.
    #[serde(deserialize_with = "Option::deserialize")]
    egress: Option<String>,
    /// `--network-ingress`, if given.
    #[serde(deserialize_with = "Option::deserialize")]
    ingress: Option<String>,
    /// The rules that permit destinations.
    egress_allow: Vec<String>,
    /// The rules that block destinations.
    egress_deny: Vec<String>,
    /// `--host-loopback`, if given.
    #[serde(deserialize_with = "Option::deserialize")]
    host_loopback: Option<String>,
    /// The `--host-loopback-forward` options.
    host_loopback_forwards: Vec<String>,
    /// `--network-proxy`, if given.
    #[serde(deserialize_with = "Option::deserialize")]
    proxy: Option<String>,
}

impl NetworkRecord {
    /// Returns the record of the network that `network` describes with `rules`, the egress rules
    /// that [`NetworkOptions::egress_rules`] resolved, or `None` without `--net`. The caller
    /// checks the network with [`sandbox_network`] first.
    pub(crate) fn new(network: &NetworkOptions, rules: &NativeRules) -> Option<Self> {
        Some(Self {
            net: network.net.clone()?,
            egress: network.network_egress.map(str::to_owned),
            ingress: network.network_ingress.map(str::to_owned),
            egress_allow: rules.allow.clone(),
            egress_deny: rules.deny.clone(),
            host_loopback: network.host_loopback.map(str::to_owned),
            host_loopback_forwards: network.host_loopback_forward.clone(),
            proxy: network.network_proxy.clone(),
        })
    }

    /// Returns the network that the record describes, as [`sandbox_network`] builds it from the
    /// options that it holds. A record whose options their parser or [`sandbox_network`] refuses
    /// fails as `sandbox configuration is malformed`, as the old plan's network did when read.
    fn config(&self) -> Result<NetworkConfig> {
        let mut network = NetworkOptions::default();
        let options = [
            ("--net", Some(&self.net)),
            ("--network-egress", self.egress.as_ref()),
            ("--network-ingress", self.ingress.as_ref()),
            ("--host-loopback", self.host_loopback.as_ref()),
            ("--network-proxy", self.proxy.as_ref()),
        ];
        let forwards = self
            .host_loopback_forwards
            .iter()
            .map(|forward| ("--host-loopback-forward", Some(forward)));
        for (name, value) in options.into_iter().chain(forwards) {
            if let Some(value) = value {
                network.take(name, value).map_err(|_| malformed())?;
            }
        }
        let rules = NativeRules {
            allow: self.egress_allow.clone(),
            deny: self.egress_deny.clone(),
        };
        sandbox_network(&network, &rules)
            .ok()
            .flatten()
            .ok_or_else(malformed)
    }
}

/// Returns the JSON text of the configuration that `provision` records for `launch` and the other
/// options, with `distro`, the launch's distro layer at the path that `provision` verified and
/// records, and its binding, and `scratch`, the launch's scratch image at the path that
/// `provision` records, and its binding, or `None` for an upper in RAM. The configuration of a
/// restored sandbox also records its `source`, without any index of the sandboxes that a snapshot
/// supplies; that of a sandbox that boots cold, whose `source` is `None`, records none.
pub(crate) fn serialize_launch(
    launch: &SandboxLaunch,
    distro: &RecordedDistro,
    scratch: Option<&RecordedScratch>,
    config: &ProvisionConfig<'_>,
    source: Option<&Source>,
) -> Result<String> {
    let settings = launch.settings();
    let (workload_uid, workload_gid) = settings.workload_identity;
    let record = Config {
        format: CONFIG_FORMAT,
        image: distro.image.clone(),
        upper: match scratch {
            Some(scratch) => Upper::Scratch {
                path: scratch.path.clone(),
            },
            None => Upper::Ram {
                max_memory_percent: config
                    .ram_upper_percent
                    .unwrap_or(UpperConfig::DEFAULT_MAX_MEMORY_PERCENT),
            },
        },
        hostname: settings.hostname.clone(),
        workload_uid,
        workload_gid,
        hypervisor: config.hypervisor.to_owned(),
        memory_mib: config.memory_mib,
        network: config.network.cloned(),
        cmdline: config.cmdline.to_owned(),
        distro_binding: distro.binding.clone(),
        scratch_binding: scratch.map(|scratch| scratch.binding.clone()),
        export: launch.export().cloned(),
        source_snapshot: source.map(|source| source.snapshot.clone()),
        source_store: source
            .map(|source| path_string(&source.store))
            .transpose()?,
        workload: source.map(|source| source.workload.clone()),
    };
    record
        .vm_config(&record.launch()?)?
        .check()
        .map_err(super::error_with_cause)?;
    pretty_serde(&record, DESCRIPTION)
}

/// Reads the configuration at `path`, which must be an object of one of the
/// [`CONFIG_FORMATS`], as [`read_object`] reads it, and fails as it does; a configuration of
/// another format, such as an older one, also says to deprovision the sandbox and provision it
/// again.
pub(crate) fn read_config(path: &Path) -> Result<Object> {
    read_object(path, DESCRIPTION, "format", &CONFIG_FORMATS).map_err(|error| {
        match (error.code(), error.message()) {
            (Some(ErrorCode::FormatUnsupported), Some(message)) => Exit::error(
                ErrorCode::FormatUnsupported,
                format!("{message}; deprovision the sandbox and provision it again"),
            ),
            _ => error,
        }
    })
}

/// The sole configuration form, shared by provision's writer and start's reader.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// The format, which [`read_config`] checked.
    format: u64,
    image: Image,
    upper: Upper,
    /// The workload's hostname.
    hostname: String,
    /// The workload's UID.
    workload_uid: u32,
    /// The workload's GID.
    workload_gid: u32,
    /// The hypervisor that OpenVMM uses.
    hypervisor: String,
    /// The guest RAM in MiB, from 1 to [`MAX_MEMORY_MIB`].
    memory_mib: u64,
    /// The user's kernel command line.
    cmdline: String,
    #[serde(deserialize_with = "Option::deserialize")]
    export: Option<ExportPlan>,
    distro_binding: DistroBinding,
    #[serde(deserialize_with = "Option::deserialize")]
    scratch_binding: Option<ScratchBinding>,
    #[serde(deserialize_with = "Option::deserialize")]
    network: Option<NetworkRecord>,
    /// The immutable named snapshot that supplies a restored sandbox's view, absent for a cold
    /// launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_snapshot: Option<SnapshotName>,
    /// The store of the snapshot of a restored sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_store: Option<String>,
    /// The workload that each restore of a restored sandbox runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workload: Option<Vec<String>>,
}

impl Config {
    /// Returns the [`Source`] of a restored sandbox, or `None` for one that boots cold.
    ///
    /// A configuration that records only part of a source, a store that is not absolute, or an
    /// empty workload fails as `sandbox configuration is malformed`.
    pub(crate) fn source(&self) -> Result<Option<Source>> {
        match (&self.source_snapshot, &self.source_store, &self.workload) {
            (None, None, None) => Ok(None),
            (Some(snapshot), Some(store), Some(workload))
                if Path::new(store).is_absolute() && !workload.is_empty() =>
            {
                Ok(Some(Source {
                    snapshot: snapshot.clone(),
                    store: PathBuf::from(store),
                    workload: workload.clone(),
                }))
            }
            _ => Err(malformed()),
        }
    }

    /// Returns the fields of `object`, a configuration that [`read_config`] read, which fails as
    /// `sandbox configuration is malformed` without a field that it requires, with a value of
    /// another type, or with a network that its record cannot build again.
    ///
    /// Guest RAM that is a number, but not an integer from 1 to [`MAX_MEMORY_MIB`], the range of
    /// `--memory-mib`, fails as `sandbox configuration records N MiB of guest RAM`, which names
    /// that range and adds `deprovision the sandbox and provision it again`. `nvx.py` and earlier
    /// releases of `nvx-rs` recorded any integer that `--memory-mib` gave, such as 0, -1, or 2^44,
    /// which OpenVMM refuses at every start. `provision` now refuses it, so such a sandbox is
    /// provisioned again rather than migrated.
    pub(crate) fn new(object: Object) -> Result<Self> {
        for (name, description) in [
            ("distro_binding", "distro binding"),
            ("scratch_binding", "scratch binding"),
            ("network", "network policy"),
            ("export", "export plan"),
        ] {
            if !object.contains_key(name) {
                return Err(Exit::error(
                    ErrorCode::FormatUnsupported,
                    format!(
                        "sandbox configuration has no {description}; deprovision the sandbox \
                         and provision it again"
                    ),
                ));
            }
        }
        if let Some(Value::Number(memory)) = object.get("memory_mib")
            && !memory
                .as_u64()
                .is_some_and(|memory| (1..=MAX_MEMORY_MIB).contains(&memory))
        {
            return Err(Exit::error(
                ErrorCode::FormatUnsupported,
                format!(
                    "sandbox configuration records {memory} MiB of guest RAM, not an integer \
                     between 1 and {MAX_MEMORY_MIB}; deprovision the sandbox and provision it \
                     again"
                ),
            ));
        }

        let config: Self =
            serde_json::from_value(Value::Object(object)).map_err(|_| malformed())?;
        // The network is checked as it is read, as the old plan's network was.
        if let Some(network) = &config.network {
            network.config()?;
        }
        Ok(config)
    }

    /// Returns the hypervisor that OpenVMM uses.
    pub(crate) fn hypervisor(&self) -> &str {
        &self.hypervisor
    }

    /// Returns the user's kernel command line.
    pub(crate) fn cmdline(&self) -> &str {
        &self.cmdline
    }

    /// Reconstructs and validates the recorded image, upper, network and export.
    pub(crate) fn launch(&self) -> Result<SandboxLaunch> {
        let upper = match &self.upper {
            Upper::Scratch { path } => SandboxUpper::scratch(path),
            Upper::Ram { max_memory_percent } if RAM_UPPER_CAPS.contains(max_memory_percent) => {
                SandboxUpper::RAM
            }
            _ => return Err(malformed()),
        };
        let launch = SandboxLaunch::new(
            vec![self.image.layer()],
            upper,
            SandboxSettings {
                hostname: self.hostname.clone(),
                workload_identity: (self.workload_uid, self.workload_gid),
                ..SandboxSettings::default()
            },
        )?;
        let launch = launch.with_export(self.export.clone())?;
        launch.validated()?;
        Ok(launch)
    }

    /// The binding that start checks against the recorded image.
    pub(crate) fn distro_binding(&self) -> &DistroBinding {
        &self.distro_binding
    }

    /// Reads the binding of the scratch image that the configuration records, which `start`
    /// checks and claims, or `None` for an upper in RAM, whose configuration records `null`.
    ///
    /// A configuration of an upper on scratch without one, as earlier releases of `nvx-rs` wrote,
    /// fails as `sandbox configuration has no scratch binding`, and one that is not of the
    /// binding's serde form as `sandbox configuration is malformed`, as
    /// [`distro_binding`](Self::distro_binding) fails, and so does a configuration of an upper in
    /// RAM that records anything but `null`.
    pub(crate) fn scratch_binding(&self) -> Result<Option<ScratchBinding>> {
        match (&self.upper, &self.scratch_binding) {
            (Upper::Scratch { .. }, Some(binding)) => Ok(Some(binding.clone())),
            (Upper::Ram { .. }, None) => Ok(None),
            _ => Err(malformed()),
        }
    }

    /// The shared projection checked at provision and consumed unchanged at start.
    pub(crate) fn vm_config(&self, launch: &SandboxLaunch) -> Result<VmConfig> {
        let identity = Some(Uuid::parse(&self.image.identity).ok_or_else(malformed)?);
        let format = match self.image.format {
            DistroFormat::Erofs => RootImage::erofs(identity),
            DistroFormat::Gpt => RootImage::gpt(identity),
        };
        let upper = match &self.upper {
            Upper::Scratch { path } => UpperConfig::Scratch {
                path: PathBuf::from(path),
                journal: Journal::Allowed,
            },
            Upper::Ram { max_memory_percent } => UpperConfig::Ram {
                max_memory_percent: *max_memory_percent,
            },
        };
        let (uid, gid) = launch.settings().workload_identity;
        let mut command_line = format!("nvx_sandbox=1 nvx_mode=managed nvx_identity={uid}:{gid}");
        if !self.cmdline.is_empty() {
            command_line.push(' ');
            command_line.push_str(&self.cmdline);
        }
        let network = self
            .network
            .as_ref()
            .map(NetworkRecord::config)
            .transpose()?;
        Ok(VmConfig {
            processors: 1,
            memory_mib: self.memory_mib,
            root: RootConfig::Layered(LayeredRoot {
                image: PathBuf::from(&self.image.path),
                format,
                gpt_first_partition_bound: None,
                upper,
                overlay: OverlayMode::TrustedLowers,
            }),
            view: View {
                hostname: self.hostname.as_bytes().to_vec(),
                // As the sandbox agent did, nvx-init gives the workload a machine ID of the VM's
                // own.
                machine_id: MachineIdMode::Generated,
            },
            network,
            filesystem: launch.export().cloned(),
            console: ConsoleAttachment::Disconnected,
            boot: Some(Boot {
                command_line,
                memory_capacity_mib: None,
                capture: None,
            }),
        })
    }
}

/// Returns the error of a configuration that is not of the form that this build reads.
fn malformed() -> Exit {
    Exit::error(ErrorCode::Io, "sandbox configuration is malformed")
}
