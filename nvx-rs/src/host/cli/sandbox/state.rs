//! The CLI's own record in a state directory, `config.json`, which `provision` writes in format 8
//! and `start` also reads in formats 1 and 7.
//!
//! - [`write_json`] writes JSON text and a line end, with each `\n` turned into the line separator
//!   of the host, into a temporary file of mode 0600 that then replaces the file.
//! - [`read_config`] reads a configuration as [`read_object`] reads a JSON file, whose `format` is
//!   1, 7, or 8, and [`Config::new`] holds the fields that `start` reads, as their types require
//!   them: a configuration without one, or with a value of another type, is malformed. Guest RAM
//!   outside the range of `--memory-mib`, which `nvx.py` and earlier releases of `nvx-rs`
//!   recorded, is refused with the advice to provision the sandbox again.
//! - [`serialize_launch`] writes a provisioned launch in format 8, with the export plan of its
//!   shares under `export`, `null` without shares, and its upper under `upper`, `scratch` or
//!   `ram`, with its members sorted by name, as [`pretty_serde`] writes them, and
//!   [`Config::launch`] reads it back. It also reads format 7, which earlier builds wrote, the
//!   same without `upper`, whose upper is on scratch, and format 1, which they wrote for a sandbox
//!   without shares, with `mount` `null` and no `export`. Formats 2 to 6 recorded the shares as
//!   they were requested; `start` no longer reads them, and such a sandbox is provisioned again.
//! - Every configuration also records the [`DistroBinding`] of the sandbox's distro layer, under
//!   `distro_binding` in its serde form, which [`Config::distro_binding`] reads back for `start`
//!   to check. `nvx.py` never wrote one, and `start` refuses a configuration without one. The
//!   binding is of the layer at the path that the configuration records, which a
//!   [`RecordedDistro`] holds.
//! - The configuration of an upper on scratch records the [`ScratchBinding`] of the sandbox's
//!   scratch image too, under `scratch_binding`, which [`Config::scratch_binding`] reads back for
//!   `start` to check and claim, of the image at the path that the configuration records under
//!   `scratch`, which a [`RecordedScratch`] holds. `start` refuses such a configuration without
//!   one as well. That of an upper in RAM has no scratch image, and records `null` under both.
//! - Every configuration also records the sandbox's [`SandboxNetwork`], under `network` in the
//!   serde form of the plan's network, or `null` for no network device, which
//!   [`Config::network`] reads back for `start` to render. It replaces the network options that
//!   `nvx.py` recorded, which no reader reads: `start` refuses a configuration without it.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::lifecycle::ProvisionConfig;
use super::{MAX_MEMORY_MIB, path_string};
use crate::ErrorCode;
use crate::host::cli::json::{Object, pretty_serde, read_object};
use crate::host::cli::output::LINE_SEPARATOR;
use crate::host::cli::{Exit, Result, io_error, io_message};
use crate::host::filesystem::ExportPlan;
use crate::host::paths;
use crate::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
use crate::host::sandbox::layer::{LayerRole, SandboxLayer};
use crate::host::sandbox::scratch::ScratchBinding;
use crate::host::sandbox::{SandboxLaunch, SandboxNetwork, SandboxSettings, SandboxUpper};
use crate::protocol::root::RootUpper;

/// The configuration that earlier builds wrote for a sandbox without shares, whose `mount` is
/// `null`, which `start` still reads.
pub(crate) const CONFIG_FORMAT: u64 = 1;
/// The configuration that earlier builds wrote, which records the export plan of the sandbox's
/// shares under `export`, in its serde form, or `null` for a sandbox without shares, and whose
/// upper is on scratch, which `start` still reads.
pub(crate) const EXPORT_CONFIG_FORMAT: u64 = 7;
/// The configuration that `provision` writes: that of [`EXPORT_CONFIG_FORMAT`], with the upper
/// under `upper`, `scratch` or `ram`.
pub(crate) const UPPER_CONFIG_FORMAT: u64 = 8;
/// Every configuration format that `start` reads.
pub(crate) const CONFIG_FORMATS: [u64; 3] =
    [CONFIG_FORMAT, EXPORT_CONFIG_FORMAT, UPPER_CONFIG_FORMAT];
/// What messages call a configuration.
const DESCRIPTION: &str = "sandbox configuration";

/// Returns the name of a temporary file next to `path`: `.NAME.UUID.tmp`, with a fresh UUID4 in
/// hexadecimal.
pub(crate) fn temporary_path(path: &Path) -> Result<PathBuf> {
    let Some(name) = path.file_name() else {
        return Err(Exit::error(
            ErrorCode::Other,
            format!("{} has an empty name", path.display()),
        ));
    };
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".{}.tmp", uuid4_hex()?));
    Ok(path.with_file_name(temporary))
}

/// Returns 32 lowercase hexadecimal digits of 122 random bits, with the version and variant bits
/// of a version-4 UUID.
pub(crate) fn uuid4_hex() -> Result<String> {
    let mut bytes = random_bytes::<16>()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(hex(&bytes))
}

/// Returns `N` random bytes from the operating system.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|error| {
        Exit::error(ErrorCode::Io, format!("cannot read random bytes: {error}"))
    })?;
    Ok(bytes)
}

/// Returns `bytes` in lowercase hexadecimal.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

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
    /// The layer's path, resolved, as `provision` records it.
    path: String,
    /// The layer at that path.
    layer: SandboxLayer,
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
        let layer = SandboxLayer::new(distro.role(), &path, distro.uuid());
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
        Ok(Self {
            path,
            layer,
            binding,
        })
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

/// A read-only layer of a configuration, as `provision` records it.
#[derive(Serialize)]
struct RecordedLayer<'a> {
    /// The layer's role: `distro`.
    role: &'a str,
    /// The layer's image, at the path that `provision` verified.
    path: &'a str,
    /// The identity that the guest expects of the image.
    uuid: &'a str,
}

/// A read-only layer of a configuration, as `start` reads it.
#[derive(Deserialize)]
struct ConfigLayer {
    /// The layer's role, which only `distro` may be.
    role: String,
    /// The layer's image.
    path: String,
    /// The identity that the guest expects of the image.
    uuid: String,
}

/// The configuration that `provision` records, in format 8.
#[derive(Serialize)]
struct Record<'a> {
    /// [`UPPER_CONFIG_FORMAT`].
    format: u64,
    /// The distro layer, the one layer that a launch holds.
    layers: [RecordedLayer<'a>; 1],
    /// Where the overlay keeps its upper: `scratch` or `ram`.
    upper: &'a str,
    /// The scratch image, at the path that `provision` bound, or `None` for an upper in RAM.
    scratch: Option<&'a str>,
    /// The workload's hostname.
    hostname: &'a str,
    /// The workload's UID.
    workload_uid: u32,
    /// The workload's GID.
    workload_gid: u32,
    /// The workload's memory limit in bytes, if any.
    memory_max: Option<u64>,
    /// The workload's process limit, if any.
    pids_max: Option<u32>,
    /// `--hypervisor`, resolved.
    hypervisor: &'a str,
    /// `--memory-mib`.
    memory_mib: u64,
    /// The sandbox's network, or `None` for no network device.
    network: Option<&'a SandboxNetwork>,
    /// `--cmdline`.
    cmdline: &'a str,
    /// The binding of the distro layer.
    distro_binding: &'a DistroBinding,
    /// The binding of the scratch image, or `None` for an upper in RAM.
    scratch_binding: Option<&'a ScratchBinding>,
    /// The export plan of the shares, or `None` without shares.
    export: Option<&'a ExportPlan>,
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
    let layer = &distro.layer;
    let settings = launch.settings();
    let (workload_uid, workload_gid) = settings.workload_identity;
    let record = Record {
        format: UPPER_CONFIG_FORMAT,
        layers: [RecordedLayer {
            role: layer.role().as_str(),
            path: &distro.path,
            uuid: layer.uuid(),
        }],
        upper: launch.upper().kind().as_str(),
        scratch: scratch.map(|scratch| scratch.path.as_str()),
        hostname: &settings.hostname,
        workload_uid,
        workload_gid,
        memory_max: settings.memory_max,
        pids_max: settings.pids_max,
        hypervisor: config.hypervisor,
        memory_mib: config.memory_mib,
        network: launch.network(),
        cmdline: config.cmdline,
        distro_binding: &distro.binding,
        scratch_binding: scratch.map(|scratch| &scratch.binding),
        export: launch.export(),
    };
    pretty_serde(&record, DESCRIPTION)
}

/// Reads the configuration at `path`, which must be an object of one of the
/// [`CONFIG_FORMATS`], as [`read_object`] reads it, and fails as it does.
pub(crate) fn read_config(path: &Path) -> Result<Object> {
    read_object(path, DESCRIPTION, "format", &CONFIG_FORMATS)
}

/// Deserializes a member that may be `null` as present, so that only a missing member reads as
/// `None`.
fn present<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// The fields of a configuration of format 1, 7, or 8 that `start` reads.
///
/// Every field is required, with its type, but the bindings, the network, the shares, and the
/// upper, whose absence [`Config`]'s methods report: `nvx.py` wrote no bindings and no typed
/// network, only formats 7 and 8 record `export`, and only format 8 `upper`, without which the
/// upper is on scratch. Other fields, such as the network options of `nvx.py`, are ignored.
#[derive(Deserialize)]
pub(crate) struct Config {
    /// The format, which [`read_config`] checked.
    format: u64,
    /// The read-only layers.
    layers: Vec<ConfigLayer>,
    /// Where the overlay keeps its upper, `scratch` or `ram`, as format 8 records it.
    upper: Option<String>,
    /// The scratch image, or `None` for an upper in RAM.
    scratch: Option<String>,
    /// The workload's hostname.
    hostname: String,
    /// The workload's UID.
    workload_uid: u32,
    /// The workload's GID.
    workload_gid: u32,
    /// The workload's memory limit in bytes, or `None` for none.
    memory_max: Option<u64>,
    /// The workload's process limit, or `None` for none.
    pids_max: Option<u32>,
    /// The hypervisor that OpenVMM uses.
    hypervisor: String,
    /// The guest RAM in MiB, from 1 to [`MAX_MEMORY_MIB`].
    memory_mib: u64,
    /// The user's kernel command line.
    cmdline: String,
    /// The export plan of the shares, `null` for none, as formats 7 and 8 record it, if recorded.
    #[serde(default, deserialize_with = "present")]
    export: Option<Value>,
    /// The shares as formats 1 to 6 recorded them, `null` for none, if recorded.
    #[serde(default, deserialize_with = "present")]
    mount: Option<Value>,
    /// The binding of the distro layer, if recorded.
    #[serde(default, deserialize_with = "present")]
    distro_binding: Option<Value>,
    /// The binding of the scratch image, `null` for an upper in RAM, if recorded.
    #[serde(default, deserialize_with = "present")]
    scratch_binding: Option<Value>,
    /// The sandbox's network, `null` for none, if recorded.
    #[serde(default, deserialize_with = "present")]
    network: Option<Value>,
}

impl Config {
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

    /// Returns the guest RAM in MiB.
    pub(crate) fn memory_mib(&self) -> u64 {
        self.memory_mib
    }

    /// Returns the user's kernel command line.
    pub(crate) fn cmdline(&self) -> &str {
        &self.cmdline
    }

    /// Reads the launch that the configuration records and checks it against the host.
    ///
    /// A layer whose role is not `distro`, an export plan that is not of its serde form, or an
    /// upper that [`upper`](Self::upper) refuses fails as `sandbox configuration is malformed`. A
    /// configuration whose format is not the one that its shares need, format 7 or 8 for one that
    /// records `export` and format 1 for one that does not, fails as `sandbox configuration format
    /// does not match its mounts`, and so does one with a share under `mount`, which only formats
    /// that `start` no longer reads recorded. The launch must pass the checks of
    /// [`SandboxLaunch::new`], then the export plan those of [`SandboxLaunch::with_export`], and
    /// the launch those of [`SandboxLaunch::validated`], which checks again the host objects that
    /// the plan recorded.
    pub(crate) fn launch(&self) -> Result<SandboxLaunch> {
        let layers = self
            .layers
            .iter()
            .map(|layer| {
                let role = LayerRole::from_name(&layer.role).ok_or_else(malformed)?;
                Ok(SandboxLayer::new(role, &layer.path, layer.uuid.as_str()))
            })
            .collect::<Result<Vec<_>>>()?;
        let plan = match &self.export {
            Some(Value::Null) | None => None,
            Some(export) => Some(
                serde_json::from_value::<ExportPlan>(export.clone()).map_err(|_| malformed())?,
            ),
        };
        let launch = SandboxLaunch::new(
            layers,
            self.upper()?,
            SandboxSettings {
                hostname: self.hostname.clone(),
                workload_identity: (self.workload_uid, self.workload_gid),
                memory_max: self.memory_max,
                pids_max: self.pids_max,
                ..SandboxSettings::default()
            },
        )?;
        let (expected, mounted) = match &self.export {
            Some(_) if self.upper.is_some() => (UPPER_CONFIG_FORMAT, false),
            Some(_) => (EXPORT_CONFIG_FORMAT, false),
            None => (
                CONFIG_FORMAT,
                self.mount.as_ref().is_some_and(|mount| !mount.is_null()),
            ),
        };
        if mounted || self.format != expected {
            return Err(Exit::error(
                ErrorCode::FormatUnsupported,
                "sandbox configuration format does not match its mounts",
            ));
        }
        let launch = launch.with_export(plan)?;
        launch.validated()?;
        Ok(launch)
    }

    /// Returns the upper that the configuration records. Format 8 names it under `upper`: for
    /// `scratch`, the scratch image under `scratch`, and for `ram`, RAM, with `scratch` `null`.
    /// Formats 1 and 7 name none, and keep the upper on the scratch image under `scratch`. Any
    /// other combination fails as `sandbox configuration is malformed`.
    fn upper(&self) -> Result<SandboxUpper> {
        let (scratch, ram) = (RootUpper::Scratch.as_str(), RootUpper::Ram.as_str());
        match (self.format, self.upper.as_deref(), self.scratch.as_deref()) {
            (UPPER_CONFIG_FORMAT, Some(upper), Some(path)) if upper == scratch => {
                Ok(SandboxUpper::scratch(path))
            }
            (UPPER_CONFIG_FORMAT, Some(upper), None) if upper == ram => Ok(SandboxUpper::RAM),
            (CONFIG_FORMAT | EXPORT_CONFIG_FORMAT, None, Some(path)) => {
                Ok(SandboxUpper::scratch(path))
            }
            _ => Err(malformed()),
        }
    }

    /// Reads the binding of the distro layer that the configuration records, which `start` checks.
    ///
    /// A configuration without one, as `nvx.py` and earlier releases of `nvx-rs` wrote, fails as
    /// `sandbox configuration has no distro binding`: the sandbox is provisioned again rather than
    /// migrated. A binding that is not of the binding's serde form fails as `sandbox configuration
    /// is malformed`.
    pub(crate) fn distro_binding(&self) -> Result<DistroBinding> {
        recorded(self.distro_binding.as_ref(), "distro binding")
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
        if self.upper.as_deref() == Some(RootUpper::Ram.as_str()) {
            return match self.scratch_binding {
                Some(Value::Null) => Ok(None),
                _ => Err(malformed()),
            };
        }
        recorded(self.scratch_binding.as_ref(), "scratch binding").map(Some)
    }

    /// Reads the network that the configuration records, in its typed form, which `start`
    /// renders, or `None` for no network device.
    ///
    /// A configuration without one, as `nvx.py` and earlier releases of `nvx-rs` wrote with their
    /// network options, fails as `sandbox configuration has no network policy`: the sandbox is
    /// provisioned again rather than migrated. A network that is not of the network's serde form,
    /// or whose policy [`SandboxNetwork::new`] refuses, fails as `sandbox configuration is
    /// malformed`.
    pub(crate) fn network(&self) -> Result<Option<SandboxNetwork>> {
        recorded(self.network.as_ref(), "network policy")
    }
}

/// Reads `value`, the serde form of a record of a configuration that messages call
/// `description`, such as `distro binding`, as [`Config::distro_binding`] reads that of the
/// distro layer.
fn recorded<T: DeserializeOwned>(value: Option<&Value>, description: &str) -> Result<T> {
    let Some(value) = value else {
        return Err(Exit::error(
            ErrorCode::FormatUnsupported,
            format!(
                "sandbox configuration has no {description}; deprovision the sandbox and \
                 provision it again"
            ),
        ));
    };
    serde_json::from_value(value.clone()).map_err(|_| malformed())
}

/// Returns the error of a configuration that is not of the form that this build reads.
fn malformed() -> Exit {
    Exit::error(ErrorCode::Io, "sandbox configuration is malformed")
}
