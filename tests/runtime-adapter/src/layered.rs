//! The host program's VMs with a layered root: an image, on the `distro` block device, under an
//! upper, which the guest agent's worker assembles outside PID 1.
//!
//! The host stages the images of a root in the sandbox's store before it launches them, as a
//! [`Staged`] record: the binding of the image, which NVX's `DistroBinding::verify` makes, hashing
//! the image only when a manifest is beside it, and the binding of the ext4 scratch image of an
//! upper on scratch, which NVX's `ScratchBinding::bind` makes without reading the image. Staging is
//! where hashing belongs: a launch never hashes.
//!
//! The host plans the VM's block devices and the boot tokens of its root with NVX's typed plan of
//! a cold layered launch for an adapter's own guest, `nvx::host::sandbox::SandboxPlan::adapter`,
//! which names none of the settings of NVX's sandbox profile, such as a workload identity, since
//! the guest agent runs its executions itself. It launches the VM through NVX's supervisor, as it
//! launches VMs without a layered root, but without NVX's managed lifecycle; see
//! [`LayeredLaunch::openvmm_arguments`]. The boot tokens carry only what NVX's boot contract
//! defines: the image's format and the identity that the guest expects of it, and the upper.
//! Everything else that the root needs goes in one root configuration, a [`RootConfig`], which the
//! host sends once it attaches.
//!
//! The host records what it launched in the sandbox's store, beside NVX's runtime records, as a
//! [`LaunchRecord`], with the bindings that the store stages for the launch's images.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use nvx::host::artifacts::absolute;
use nvx::host::launch::DEFAULT_MEMORY_MIB;
use nvx::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
use nvx::host::sandbox::scratch::ScratchBinding;
use nvx::host::sandbox::{LayerRole, SandboxDistro, SandboxLayer, SandboxPlan, SandboxUpper};
use nvx::protocol::root::{ImageFormat, RootImage};
use nvx::uuid::Uuid;
use nvx::{Error, Hypervisor, Result};
use serde_json::{Value, json};

use crate::root::RootConfig;

/// The name of the record of a launch in the sandbox's store.
pub const RECORD_NAME: &str = "runtime-adapter.json";
/// The format of the record of a launch that this build writes.
pub const RECORD_FORMAT: u64 = 2;
/// The name of the record of the staged images in the sandbox's store.
pub const STAGED_NAME: &str = "runtime-adapter-staged.json";
/// The format of the record of the staged images that this build writes and reads.
pub const STAGED_FORMAT: u64 = 1;
/// The kernel parameter with which NVX's `/init` hands a VM to NVX's init agent, which the
/// adapter's initramfs replaces with the guest agent; see [`crate::initramfs`]. An adapter's plan
/// renders none of the parameters of NVX's sandbox profile, so the launch passes it as its own.
pub const SANDBOX_TOKEN: &str = "nvx_sandbox=1";
/// The size of a sector of a block device, in bytes.
const SECTOR_SIZE: u64 = 512;

/// A VM with a layered root: the plan of its image, its upper, and their boot tokens, the root
/// configuration that the host sends its guest agent, and the bindings that the sandbox's store
/// stages for its images.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayeredLaunch {
    /// The adapter's plan of the VM's block devices and of the boot tokens of its root.
    pub plan: SandboxPlan,
    /// The root configuration that the host sends the guest agent once it attaches.
    pub config: RootConfig,
    /// The bindings that the sandbox's store stages for the plan's images, as
    /// [`Staged::bindings`] finds them, which the launch records.
    pub bindings: Bindings,
}

impl LayeredLaunch {
    /// Returns the launch of the image at `distro`, in `format`, of which the guest expects the
    /// identity `identity`, over the ext4 scratch image at `scratch`, or over RAM without one,
    /// without bindings.
    ///
    /// The root configuration takes its defaults, with the runtime directory `runtime` instead, if
    /// any, as it is, so that the guest checks it, and with the size of each image, if it is a
    /// whole number of sectors, which the guest checks against its block device.
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
            ..RootConfig::default()
        };
        if let Some(runtime) = runtime {
            runtime.clone_into(&mut config.runtime);
        }
        Ok(Self {
            plan,
            config,
            bindings: Bindings::default(),
        })
    }

    /// Returns the VM's kernel command line: [`SANDBOX_TOKEN`], then the boot tokens of the
    /// plan's root, without parameters of the caller.
    ///
    /// Fails as `SandboxPlan::kernel_command_line` does.
    pub fn kernel_command_line(&self) -> Result<String> {
        self.plan.kernel_command_line(SANDBOX_TOKEN)
    }

    /// Returns OpenVMM's arguments for the VM: the plan's, which attach its block devices, then
    /// the VM in one process on `hypervisor`, NVX's default memory, the kernel at `kernel`, the
    /// initramfs at `initramfs`, the [kernel command line](Self::kernel_command_line), no virtio
    /// console, the control console listening on `endpoint` with the launch capability on
    /// standard input, and the outcome report at `report`.
    ///
    /// Unlike those of NVX's sandbox start, they name no workload identity, which the adapter's
    /// plan does not hold, and no managed lifecycle: OpenVMM only checks the arguments of the
    /// managed lifecycle and labels its outcome report with it, and only NVX's init agent, which
    /// the guest agent replaces, reads its boot token.
    ///
    /// Fails as `SandboxPlan::openvmm_arguments` and [`kernel_command_line`](Self::kernel_command_line)
    /// do.
    pub fn openvmm_arguments(
        &self,
        hypervisor: Hypervisor,
        kernel: &Path,
        initramfs: &Path,
        endpoint: &str,
        report: &Path,
    ) -> Result<Vec<OsString>> {
        let mut arguments = self
            .plan
            .openvmm_arguments()?
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>();
        let memory = format!("{DEFAULT_MEMORY_MIB}M");
        let command_line = self.kernel_command_line()?;
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
            OsStr::new(&command_line),
            OsStr::new("--virtio-console"),
            OsStr::new("none"),
            OsStr::new("--microvm-control-console"),
            OsStr::new(&control),
            OsStr::new("--microvm-control-auth-stdin"),
            OsStr::new("--microvm-report"),
            report.as_os_str(),
        ];
        arguments.extend(options.map(OsStr::to_os_string));
        Ok(arguments)
    }
}

/// Returns the adapter's plan of the image at `distro`, in `format`, of which the guest expects
/// the identity `identity`, over the ext4 scratch image at `scratch`, or over RAM without one,
/// checked against the host with `SandboxPlan::validated`.
///
/// OpenVMM runs in the sandbox's store, so the plan names the images by absolute paths, which it
/// normalizes as `pathlib` does.
fn plan(
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
fn layer(path: &Path, format: ImageFormat, identity: Uuid) -> Result<SandboxLayer> {
    let path = unicode(path)?;
    let identity = identity.to_string();
    Ok(match format {
        ImageFormat::Erofs => SandboxLayer::new(LayerRole::Distro, path, identity),
        ImageFormat::Gpt => SandboxLayer::gpt(LayerRole::Distro, path, identity),
    })
}

/// Returns `path` as text, which the plan takes.
fn unicode(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        Error::unsupported(format!("the path {} is not Unicode text", path.display()))
    })
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

/// A file that the host program staged: its path, absolute and normalized as a plan names it, and
/// its binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedFile<B> {
    /// The file's path.
    pub path: PathBuf,
    /// The file's binding.
    pub binding: B,
}

/// The images that the host program staged in a sandbox's store for its launches: the image, with
/// the binding that its explicit verification made, and the ext4 scratch image of an upper on
/// scratch, if any, with its binding.
///
/// [`stage`](Self::stage) stages them, and [`write`](Self::write) records them in the store, in
/// [`STAGED_NAME`], in place of the images staged earlier. A launch [reads](Self::read) the record,
/// and takes the [bindings](Self::bindings) of its own images from it. The record is JSON in format
/// [`STAGED_FORMAT`]:
///
/// ```json
/// {"format": 1,
///  "distro": {"path": "/images/distro.erofs", "binding": DISTRO_BINDING},
///  "scratch": {"path": "/sandboxes/one/scratch.ext4", "binding": SCRATCH_BINDING}}
/// ```
///
/// Each `path` is the absolute path of a file, as a plan names it, and each binding the serde form
/// of NVX's `DistroBinding` of the image or `ScratchBinding` of the scratch image. `scratch` is
/// `null` when no scratch image was staged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    /// The image, and the binding that its explicit verification made.
    pub distro: StagedFile<DistroBinding>,
    /// The scratch image and its binding, if one was staged.
    pub scratch: Option<StagedFile<ScratchBinding>>,
}

impl Staged {
    /// Stages the image at `distro`, in `format`, of which the guest expects the identity
    /// `identity`, and the ext4 scratch image at `scratch`, if any.
    ///
    /// It plans the images as a launch over the scratch image, or over RAM without one, would, and
    /// checks the plan against the host, as `SandboxPlan::validated` does. It then verifies the
    /// image explicitly with NVX's `DistroBinding::verify`, which hashes the image only when a
    /// manifest is beside it, checks the binding from metadata, as a launch checks it, so that it
    /// stages no binding that every launch would refuse, and binds the scratch image with NVX's
    /// `ScratchBinding::bind`, which neither reads nor claims it.
    ///
    /// Fails as those do: with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) for a path
    /// that is not Unicode and for a manifest that does not match the image, with
    /// [`ErrorKind::Unavailable`](nvx::ErrorKind::Unavailable) for an image that is not a regular
    /// file, and with [`ErrorKind::Io`](nvx::ErrorKind::Io) if an image cannot be read or sealed.
    pub fn stage(
        distro: &Path,
        format: ImageFormat,
        identity: Uuid,
        scratch: Option<&Path>,
    ) -> Result<Self> {
        let plan = plan(distro, format, identity, scratch)?;
        let path = plan.distro().path();
        let layer = layer(path, format, identity)?;
        let binding = DistroBinding::verify(&layer)?;
        binding.check(&layer, DISTRO_POLICY_REVISION)?;
        let scratch = match plan.upper().scratch_path() {
            Some(path) => Some(StagedFile {
                path: path.to_owned(),
                binding: ScratchBinding::bind(path)?,
            }),
            None => None,
        };
        Ok(Self {
            distro: StagedFile {
                path: path.to_owned(),
                binding,
            },
            scratch,
        })
    }

    /// Returns the bindings that this record stages for the images of `plan`: the image's, if the
    /// record stages the image at the path of the plan's, and the scratch image's, if the plan's
    /// upper is on a scratch image that the record stages at its path. The paths compare as text,
    /// as a plan normalizes them.
    pub fn bindings(&self, plan: &SandboxPlan) -> Bindings {
        let staged_at = |staged: &Path, planned: &Path| staged.as_os_str() == planned.as_os_str();
        Bindings {
            distro: staged_at(&self.distro.path, plan.distro().path())
                .then(|| self.distro.binding.clone()),
            scratch: self
                .scratch
                .as_ref()
                .zip(plan.upper().scratch_path())
                .filter(|(scratch, planned)| staged_at(&scratch.path, planned))
                .map(|(scratch, _)| scratch.binding.clone()),
        }
    }

    /// Returns the record as JSON.
    ///
    /// Fails with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) for a path that is not
    /// Unicode, and with [`ErrorKind::Other`](nvx::ErrorKind::Other) if a binding has no JSON
    /// form.
    pub fn to_json(&self) -> Result<Value> {
        let distro = binding_json(serde_json::to_value(&self.distro.binding))?;
        let scratch = match &self.scratch {
            Some(scratch) => file_json(
                &scratch.path,
                binding_json(serde_json::to_value(&scratch.binding))?,
            )?,
            None => Value::Null,
        };
        Ok(json!({
            "format": STAGED_FORMAT,
            "distro": file_json(&self.distro.path, distro)?,
            "scratch": scratch,
        }))
    }

    /// Returns the record whose JSON form, as [`to_json`](Self::to_json) writes it, is `value`.
    ///
    /// Fails with [`ErrorKind::Unavailable`](nvx::ErrorKind::Unavailable) for a record of another
    /// format, an object that lacks a member or has another, a path that is not a string, and a
    /// binding that does not decode, as NVX's bindings decode their serde forms.
    pub fn from_json(value: &Value) -> Result<Self> {
        let [format, distro, scratch] = members(value, ["format", "distro", "scratch"])
            .ok_or_else(|| {
                invalid("is not an object of a format, an image, and a scratch image")
            })?;
        if format.as_u64() != Some(STAGED_FORMAT) {
            return Err(invalid(&format!(
                "has the format {format}, which this build does not read"
            )));
        }
        let (path, binding) = file(distro, "image")?;
        let distro = StagedFile {
            path,
            binding: serde_json::from_value(binding.clone()).map_err(|error| {
                invalid("holds a binding of the image that does not decode").with_source(error)
            })?,
        };
        let scratch = match scratch {
            Value::Null => None,
            scratch => {
                let (path, binding) = file(scratch, "scratch image")?;
                Some(StagedFile {
                    path,
                    binding: serde_json::from_value(binding.clone()).map_err(|error| {
                        invalid("holds a binding of the scratch image that does not decode")
                            .with_source(error)
                    })?,
                })
            }
        };
        Ok(Self { distro, scratch })
    }

    /// Writes the record in `directory`, the sandbox's store, in place of an earlier one.
    ///
    /// Fails as [`to_json`](Self::to_json) does, and with
    /// [`ErrorKind::Io`](nvx::ErrorKind::Io) if the record cannot be written.
    pub fn write(&self, directory: &Path) -> Result<()> {
        write_record(&directory.join(STAGED_NAME), &self.to_json()?)
    }

    /// Reads the record of the images staged in `directory`, the sandbox's store, or returns
    /// `None` if the store has none.
    ///
    /// Fails with [`ErrorKind::Io`](nvx::ErrorKind::Io) if the record cannot be read, and with
    /// [`ErrorKind::Unavailable`](nvx::ErrorKind::Unavailable) if it is not JSON, or
    /// [`from_json`](Self::from_json) refuses it.
    pub fn read(directory: &Path) -> Result<Option<Self>> {
        let path = directory.join(STAGED_NAME);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::io(format!("cannot read {}", path.display())).with_source(error));
            }
        };
        let unreadable = || {
            Error::unavailable(format!(
                "cannot read the record of the staged images {}",
                path.display()
            ))
        };
        let value = serde_json::from_str(&text).map_err(|error| unreadable().with_source(error))?;
        Self::from_json(&value)
            .map(Some)
            .map_err(|error| unreadable().with_source(error))
    }
}

/// The bindings of a launch's images that the sandbox's store stages, as [`Staged::bindings`]
/// finds them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bindings {
    /// The binding of the image, if the store stages the launch's image.
    pub distro: Option<DistroBinding>,
    /// The binding of the scratch image, if the launch's upper is on a scratch image that the
    /// store stages.
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
fn binding_json(form: serde_json::Result<Value>) -> Result<Value> {
    form.map_err(|error| Error::other("a binding has no JSON form").with_source(error))
}

/// Returns the JSON form of a staged file at `path`, whose binding's JSON form is `binding`.
///
/// Fails with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) for a path that is not
/// Unicode.
fn file_json(path: &Path, binding: Value) -> Result<Value> {
    Ok(json!({"path": unicode(path)?, "binding": binding}))
}

/// Returns the path and the JSON form of the binding of the staged file whose JSON form is
/// `value`, an object of a `path`, a string, and a `binding`, which the record calls `what`.
fn file<'a>(value: &'a Value, what: &str) -> Result<(PathBuf, &'a Value)> {
    let [path, binding] = members(value, ["path", "binding"])
        .ok_or_else(|| invalid(&format!("does not name its {what} by a path and a binding")))?;
    let path = path
        .as_str()
        .ok_or_else(|| invalid(&format!("names its {what} by a path that is not a string")))?;
    Ok((PathBuf::from(path), binding))
}

/// Returns the members `names` of `value`, in their order, if it is an object of those members and
/// no other.
fn members<'a, const N: usize>(value: &'a Value, names: [&str; N]) -> Option<[&'a Value; N]> {
    let object = value.as_object().filter(|object| object.len() == N)?;
    names
        .iter()
        .map(|name| object.get(*name))
        .collect::<Option<Vec<_>>>()?
        .try_into()
        .ok()
}

/// Returns the error of a record of staged images that is invalid, which `problem` says.
fn invalid(problem: &str) -> Error {
    Error::unavailable(format!("the record {problem}"))
}

/// Writes `record` as JSON, with a newline, at `path`, in place of an earlier record.
///
/// Fails with [`ErrorKind::Io`](nvx::ErrorKind::Io) if the record cannot be written.
fn write_record(path: &Path, record: &Value) -> Result<()> {
    let mut text = record.to_string();
    text.push('\n');
    fs::write(path, text)
        .map_err(|error| Error::io(format!("cannot write {}", path.display())).with_source(error))
}

/// What the host program launched in a sandbox, which it records in the sandbox's store, in
/// [`RECORD_NAME`], before it spawns the VMM. The record is JSON in format [`RECORD_FORMAT`]:
///
/// ```json
/// {"format": 2, "mode": "layered", "kernelCommandLine": "nvx_sandbox=1 nvx_layer=...",
///  "plan": PLAN, "bindings": {"distro": DISTRO_BINDING, "scratch": SCRATCH_BINDING},
///  "rootConfiguration": "version=1\nruntime=/run/runtime-adapter\n..."}
/// ```
///
/// The `mode` is `layered`, or `blockless` for a VM without a layered root, whose `plan`,
/// `bindings`, and `rootConfiguration` are `null`. The `plan` is the persisted form of the VM's
/// `SandboxPlan`. The `bindings` are those that the store stages for the plan's images, as
/// [`Staged::bindings`] finds them, in the serde forms of NVX's `DistroBinding` and
/// `ScratchBinding`, each `null` unless the store stages it, as for an upper in RAM, which has no
/// scratch image. The `rootConfiguration` is the encoding of the root configuration that the host
/// sends, as it sends it, whether or not the guest accepts it.
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
}
