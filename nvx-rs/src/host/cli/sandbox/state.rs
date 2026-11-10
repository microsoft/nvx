//! The CLI's typed `config.json`, in format 9 only: one EROFS or GPT image by path,
//! a scratch upper by path or a RAM upper with its explicit cap, the image bindings,
//! and the workload's view, network and export.
//!
//! Provision checks the same [`VmConfig`] that start reconstructs. Older records are
//! refused, not migrated. Bindings still check the recorded caller-owned paths before
//! launch, and the caller keeps those paths stable while the VM uses them.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::lifecycle::ProvisionConfig;
use super::{MAX_MEMORY_MIB, path_string};
use crate::ErrorCode;
use crate::activation::{Journal, MachineIdMode, OverlayMode, View};
use crate::host::cli::json::{Object, pretty_serde, read_object};
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
use crate::host::sandbox::{SandboxLaunch, SandboxNetwork, SandboxSettings, SandboxUpper};
use crate::net::Ipv4Cidr;
use crate::protocol::root::{RAM_UPPER_CAPS, RootImage};
use crate::uuid::Uuid;

pub(crate) use crate::host::cli::{hex, random_bytes, temporary_path};

#[cfg(test)]
mod tests;

/// The typed image and upper configuration that provision writes and start reads.
pub(crate) const CONFIG_FORMAT: u64 = 9;
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

/// Returns the JSON text of the configuration that `provision` records for `launch` and the other
/// options, with `distro`, the launch's distro layer at the path that `provision` verified and
/// records, and its binding, and `scratch`, the launch's scratch image at the path that
/// `provision` records, and its binding, or `None` for an upper in RAM.
pub(crate) fn serialize_launch(
    launch: &SandboxLaunch,
    distro: &RecordedDistro,
    scratch: Option<&RecordedScratch>,
    config: &ProvisionConfig<'_>,
) -> Result<String> {
    serialize_launch_with_snapshot(launch, distro, scratch, config, None)
}

/// Records a restored sandbox's source alongside its own launch, without a snapshot index.
pub(crate) fn serialize_launch_with_snapshot(
    launch: &SandboxLaunch,
    distro: &RecordedDistro,
    scratch: Option<&RecordedScratch>,
    config: &ProvisionConfig<'_>,
    source_snapshot: Option<&SnapshotName>,
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
        network: launch.network().cloned(),
        cmdline: config.cmdline.to_owned(),
        distro_binding: distro.binding.clone(),
        scratch_binding: scratch.map(|scratch| scratch.binding.clone()),
        export: launch.export().cloned(),
        source_snapshot: source_snapshot.cloned(),
    };
    record
        .vm_config(&record.launch()?)?
        .check()
        .map_err(super::error_with_cause)?;
    pretty_serde(&record, DESCRIPTION)
}

/// Reads the configuration at `path`, which must be an object of one of the
/// [`CONFIG_FORMATS`], as [`read_object`] reads it, and fails as it does.
pub(crate) fn read_config(path: &Path) -> Result<Object> {
    read_object(path, DESCRIPTION, "format", &CONFIG_FORMATS)
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
    network: Option<SandboxNetwork>,
    /// The immutable named snapshot that supplies a restored sandbox's view, absent for a cold
    /// launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_snapshot: Option<SnapshotName>,
}

impl Config {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "SL3.8a locks a restored sandbox's source snapshot"
        )
    )]
    pub(crate) fn source_snapshot(&self) -> Option<&SnapshotName> {
        self.source_snapshot.as_ref()
    }

    /// Returns the fields of `object`, a configuration that [`read_config`] read, which fails as
    /// `sandbox configuration is malformed` without a field that it requires, or with a value of
    /// another type.
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

        serde_json::from_value(Value::Object(object)).map_err(|_| malformed())
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
        let launch = launch
            .with_export(self.export.clone())?
            .with_network(self.network.clone());
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
        let network =
            launch
                .network()
                .map(|network| -> Result<NetworkConfig> {
                    Ok(NetworkConfig {
                        guest_network: Some(Ipv4Cidr::parse(network.guest_network()).map_err(
                            |error| Exit::error(ErrorCode::Unsupported, error.to_string()),
                        )?),
                        policy: network.policy().clone(),
                    })
                })
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
