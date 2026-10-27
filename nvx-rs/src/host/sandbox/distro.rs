//! The binding of a sandbox's distro layer: what explicit verification of the layer found,
//! recorded so that a launch checks the layer from its metadata alone.
//!
//! Three separate checks guard the distro layer of a sandbox, and only explicit verification
//! reads it:
//!
//! - The entrypoint check,
//!   [`validate_sandbox_entrypoint`](super::options::validate_sandbox_entrypoint), refuses the
//!   entrypoints of systemd, which the sandbox security profile does not run. It reads nothing,
//!   and every launch applies it, whether or not the layer has a manifest.
//! - Explicit verification, [`DistroBinding::verify`], reads the layer's manifest,
//!   `LAYER.manifest.json` beside the layer, if there is one. The manifest must name the layer and
//!   its SHA-256 digest, which verification compares with a hash of the whole layer, and list the
//!   layer's packages, of which the distro policy refuses `systemd`. Verification runs where
//!   hashing belongs: where a layer is built, installed, or staged, in diagnostics, and in the
//!   preflight of `nvx-rs sandbox run` and `provision`. A layer without a manifest passes, and
//!   is not read.
//! - The staged check, [`DistroBinding::check`], opens the layer and its manifest, compares them
//!   with the binding from their metadata alone, and returns the layer, open, for the launch. It
//!   never reads, maps, or hashes either file. A missing or stale binding fails: no launch
//!   registers, verifies, or hashes a layer on its own.
//!
//! A [`DistroBinding`] comes from explicit verification, or from a staging caller that verified
//! the layer itself and imports the binding with [`DistroBinding::import`], which does not hash
//! the layer again. It ties the [`FileSeal`] of the layer to the seal of its manifest, or to the
//! manifest's absence, to the revision and result of the distro policy, and to the layer's format
//! and the identity that the guest expects of it. Callers record bindings, as
//! `nvx-rs sandbox provision` records one in the sandbox's configuration, so their serde form is
//! part of the callers' state.
//!
//! A seal detects accidental change, not deliberate tampering by the account that can write the
//! sealed files, as [`seal`] explains. A caller therefore stages layers and their manifests
//! read-only, in a directory that only its own account can write, never rewrites them in place,
//! and binds a new layer, or a replacement of one, anew.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::geometry::BlockGeometry;
use super::layer::SandboxLayer;
use super::mount::os_error;
use super::{block_geometry, refused};
use crate::error::{Error, Result};
use crate::host::artifacts::DISTRO_MANIFEST_SUFFIX;
use crate::host::network::policy::MAX_JSON_DEPTH;
use crate::host::network::policy::json::{self, ReadError};
use crate::host::seal::{self, FileKind, FileSeal};

/// The revision of NVX's distro policy, which refuses a distro layer whose manifest lists the
/// `systemd` package, since the sandbox security profile does not run systemd.
///
/// A change of the policy moves the revision, and [`DistroBinding::check`] refuses a binding of
/// another revision than the one that its caller accepts, so a layer is verified again under the
/// new policy before it launches.
pub const DISTRO_POLICY_REVISION: u32 = 1;

/// The version of the serde form of [`DistroBinding`] that this build writes and checks.
pub const BINDING_VERSION: u32 = 1;

/// How the operations of a binding reach files: the opens of [`seal`] and [`FileSeal::of`], or,
/// in tests, opens that forbid reading the content, and seals that change a file as it is read.
#[derive(Clone, Copy)]
pub(crate) struct Access<'a> {
    /// Opens a file.
    pub(crate) open: &'a dyn Fn(&Path) -> io::Result<File>,
    /// Seals an open file.
    pub(crate) seal: &'a dyn Fn(&File) -> io::Result<FileSeal>,
}

impl Access<'static> {
    /// The access of verification: [`seal::open_read`] and [`FileSeal::of`].
    pub(crate) const READ: Self = Self {
        open: &seal::open_read,
        seal: &FileSeal::of,
    };

    /// The access of the staged check and of imports: [`seal::open_guarded`] and
    /// [`FileSeal::of`].
    pub(crate) const GUARDED: Self = Self {
        open: &seal::open_guarded,
        seal: &FileSeal::of,
    };
}

/// The format of a staged distro layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DistroFormat {
    /// An EROFS filesystem image, whose filesystem UUID the guest checks before it mounts it.
    Erofs,
    /// A GPT disk image, whose disk GUID the guest checks before it uses the disk.
    Gpt,
}

impl DistroFormat {
    /// Returns the name of the format: `erofs` or `gpt`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Erofs => "erofs",
            Self::Gpt => "gpt",
        }
    }
}

impl fmt::Display for DistroFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The result of a distro policy for a layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyResult {
    /// The policy accepts the layer.
    Accepted,
    /// The policy refuses the layer, which no check passes.
    Rejected,
}

/// The verdict of a distro policy for a layer: the policy's revision and its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyVerdict {
    /// The revision of the policy that judged the layer, such as [`DISTRO_POLICY_REVISION`].
    pub revision: u32,
    /// What the policy found.
    pub result: PolicyResult,
}

impl PolicyVerdict {
    /// The verdict of explicit verification for a layer that it accepts: NVX's distro policy, at
    /// [`DISTRO_POLICY_REVISION`], accepts the layer.
    pub const ACCEPTED: Self = Self {
        revision: DISTRO_POLICY_REVISION,
        result: PolicyResult::Accepted,
    };
}

/// The binding of a distro layer: the seals of the layer and of its manifest, or the manifest's
/// absence, the policy verdict, and the layer's format and expected identity.
///
/// [`verify`](Self::verify) and [`import`](Self::import) make a binding, and
/// [`check`](Self::check) checks the layer against it before a launch. Its serde form, which
/// callers record, holds every field, each required:
///
/// ```json
/// {"version": 1, "format": "erofs", "identity": "11111111-1111-1111-1111-111111111111",
///  "image": SEAL, "manifest": SEAL or null, "policy": {"revision": 1, "result": "accepted"}}
/// ```
///
/// where `format` is `erofs` or `gpt`, and each `SEAL` is the serde form of a [`FileSeal`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DistroBinding {
    /// [`BINDING_VERSION`].
    version: u32,
    /// The layer's format.
    format: DistroFormat,
    /// The identity that the guest expects of the layer: the UUID of its EROFS filesystem, or the
    /// GUID of its GPT disk.
    identity: String,
    /// The seal of the layer.
    image: FileSeal,
    /// The seal of the layer's manifest, or `None` for a layer without one. The form requires it,
    /// as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    manifest: Option<FileSeal>,
    /// The policy verdict.
    policy: PolicyVerdict,
}

/// A distro layer that [`DistroBinding::check`] found unchanged, held open.
///
/// The file is the object that the check compared with the binding, open for reading, whatever
/// later takes its path. On Windows, it denies other opens for writing or deleting the layer, so
/// that nothing modifies, renames, or replaces the layer while it is held; OpenVMM's read-only
/// open of the layer shares it. Dropping it closes the file, and a VMM that inherited it from a
/// [`PreparedLaunch`](super::PreparedLaunch) keeps its copy until the prepared launch is released
/// or the VMM exits.
///
/// A [`PreparedLaunch`](super::PreparedLaunch) takes it, with the path at which the check opened
/// the layer and the format and identity that the binding names, and hands the layer to the VMM.
#[derive(Debug)]
pub struct CheckedDistro {
    /// The checked layer.
    file: File,
    /// The path at which the check opened the layer.
    path: PathBuf,
    /// The layer's format, as the binding names it.
    format: DistroFormat,
    /// The identity that the guest expects of the layer, as the binding names it.
    identity: String,
}

impl CheckedDistro {
    /// Returns the checked layer.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Returns the path at which the check opened the layer.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the layer's format and the identity that the guest expects of it, as the binding
    /// names them.
    pub(crate) fn bound(&self) -> (DistroFormat, &str) {
        (self.format, &self.identity)
    }

    /// Returns the block geometry that OpenVMM presents for the layer as the read-only `distro`
    /// block device, read through the checked file, as OpenVMM reads it through its own open of
    /// the layer: the layer's length, which must be a nonzero multiple of 512 bytes; logical
    /// blocks of 512 bytes; and physical blocks of the I/O block size of the layer's filesystem,
    /// its `st_blksize`, on Linux, of 4096 bytes on Windows, and of the logical block size on
    /// other hosts, which must be a power of two of at least 512 bytes.
    ///
    /// It reads the file's metadata alone. A restore of a template presents its `distro` at the
    /// geometry that the template recorded, the capacity of every clone's image, over an image
    /// that may be shorter, so the image of a clone must be no longer than the template's
    /// `distro`, with the same logical and physical block sizes, as
    /// [`ClonePlan`](crate::host::template::ClonePlan) checks before the clone's launch.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a length or a
    /// physical block size that OpenVMM refuses, and with [`ErrorKind::Io`](crate::ErrorKind::Io)
    /// if the metadata cannot be read.
    pub fn geometry(&self) -> Result<BlockGeometry> {
        let description = match self.format {
            DistroFormat::Erofs => "distro EROFS layer",
            DistroFormat::Gpt => "distro GPT image",
        };
        block_geometry(&self.file, &self.path, description)
    }

    /// Returns the checked layer, for a prepared launch to move.
    #[cfg(target_os = "linux")]
    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    /// Returns the checked layer, which the caller then holds: an owned descriptor on Linux, and
    /// an owned handle on Windows, which it converts into an `OwnedFd` or an `OwnedHandle`.
    pub fn into_file(self) -> File {
        self.file
    }
}

impl DistroBinding {
    /// Verifies the distro layer `layer` explicitly, and returns its binding.
    ///
    /// Without a manifest beside the layer, the layer passes without being read: the binding
    /// records the manifest's absence. A manifest is beside the layer as `Path.exists` finds it,
    /// for `nvx.py`'s outcomes, so a path whose lookup fails counts as none; the check of the
    /// binding then refuses that path, as [`check`](Self::check) says. With a manifest, it must
    /// be a JSON object of format 1 that names the layer and its SHA-256 digest, with a list of
    /// packages that each have a name, of which none is `systemd`. Verification reads the
    /// manifest and hashes the whole layer through the files that it opened, and fails if the
    /// metadata of either changed while it read them. The binding holds the seals that the files
    /// had, and [`PolicyVerdict::ACCEPTED`].
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a path without a
    /// name and a manifest that it refuses, which a digest that does not match fails before the
    /// packages do; with
    /// [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for a layer that is not a regular
    /// file; and with [`ErrorKind::Io`](crate::ErrorKind::Io) if the layer cannot be read or sealed,
    /// or changed while it was read.
    pub fn verify(layer: &SandboxLayer) -> Result<Self> {
        verify_with(layer, Access::READ)
    }

    /// Returns the binding of the distro layer `layer`, staged and judged by its caller, which
    /// verified the layer itself, without reading the layer or its manifest.
    ///
    /// The binding holds the seals of the layer and of its manifest, if one sits beside it, and
    /// `verdict`. A caller that imports a binding vouches for the layer and its manifest as
    /// [`verify`](Self::verify) would; it must therefore be authorized to stage layers, which
    /// NVX leaves to it. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a path without a name, with
    /// [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) if the layer or its manifest is
    /// not a regular file, and with [`ErrorKind::Io`](crate::ErrorKind::Io) if either cannot be
    /// opened or sealed, as for a manifest path whose lookup fails for another reason than a
    /// missing file, which the import does not take for an absent manifest.
    pub fn import(layer: &SandboxLayer, verdict: PolicyVerdict) -> Result<Self> {
        import_with(layer, verdict, Access::GUARDED)
    }

    /// Checks the distro layer `layer` against this binding from metadata alone, and returns the
    /// layer, open, as the check found it.
    ///
    /// The binding must be of [`BINDING_VERSION`], hold an accepted verdict of
    /// `policy_revision`, the revision of the distro policy that the caller accepts, such as
    /// [`DISTRO_POLICY_REVISION`], and name the format and identity of `layer`. The check then
    /// opens the layer and its manifest, and compares their seals with the binding's: a
    /// replacement, a change of kind, length, or time, and a manifest that appeared, disappeared,
    /// or changed each fail. A binding of a layer without a manifest holds only while nothing is
    /// at the manifest's path: a path whose lookup fails for another reason than a missing file
    /// fails the check too. It never reads, maps, or hashes the layer or the manifest, and never
    /// verifies them again.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for a stale binding,
    /// one that names another layer, and a missing layer; with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a verdict that rejects the
    /// layer and a path without a name; and with [`ErrorKind::Io`](crate::ErrorKind::Io) if a file
    /// cannot be opened or sealed.
    pub fn check(&self, layer: &SandboxLayer, policy_revision: u32) -> Result<CheckedDistro> {
        self.check_with(layer, policy_revision, Access::GUARDED)
    }

    /// Returns the layer's format.
    pub fn format(&self) -> DistroFormat {
        self.format
    }

    /// Returns the identity that the guest expects of the layer: the UUID of its EROFS
    /// filesystem, or the GUID of its GPT disk.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Returns the seal of the layer.
    pub fn image(&self) -> &FileSeal {
        &self.image
    }

    /// Returns the seal of the layer's manifest, or `None` for a layer without one.
    pub fn manifest(&self) -> Option<&FileSeal> {
        self.manifest.as_ref()
    }

    /// Returns the policy verdict.
    pub fn policy(&self) -> PolicyVerdict {
        self.policy
    }

    /// Returns the binding's version, which a [check](Self::check) requires to be
    /// [`BINDING_VERSION`].
    pub(crate) fn version(&self) -> u32 {
        self.version
    }

    /// Returns the binding of `layer`, with the seals of its image and manifest and `policy`.
    fn new(
        layer: &SandboxLayer,
        image: FileSeal,
        manifest: Option<FileSeal>,
        policy: PolicyVerdict,
    ) -> Self {
        Self {
            version: BINDING_VERSION,
            format: layer.format(),
            identity: layer.uuid().to_owned(),
            image,
            manifest,
            policy,
        }
    }

    /// Implements [`check`](Self::check), reaching files through `access`.
    pub(crate) fn check_with(
        &self,
        layer: &SandboxLayer,
        policy_revision: u32,
        access: Access<'_>,
    ) -> Result<CheckedDistro> {
        let path = layer.path();
        let shown = path.display();
        let stale =
            |detail: String| Error::unavailable(format!("the distro binding of {shown} {detail}"));
        if self.version != BINDING_VERSION {
            return Err(stale(format!(
                "has version {}, which this build does not check",
                self.version
            )));
        }
        if self.format != layer.format() {
            return Err(stale(format!(
                "names the {} layer with identity {}, not the {} layer with identity {}",
                self.format,
                self.identity,
                layer.format(),
                layer.uuid()
            )));
        }
        if self.identity != layer.uuid() {
            return Err(stale(format!(
                "names the {} layer with identity {}, not {}",
                self.format,
                self.identity,
                layer.uuid()
            )));
        }
        if self.policy.revision != policy_revision {
            return Err(stale(format!(
                "holds a verdict of distro policy revision {}, not {policy_revision}",
                self.policy.revision
            )));
        }
        if self.policy.result != PolicyResult::Accepted {
            return Err(refused(format!(
                "the distro binding of {shown} holds a verdict that rejects the layer"
            )));
        }
        let manifest = manifest_path(path)?;
        let manifest_shown = manifest.display();
        let file = (access.open)(path)
            .map_err(|error| open_error(&error, path, &layer_description(layer)))?;
        let image = (access.seal)(&file).map_err(|error| os_error(&error, Some(path)))?;
        if let Some(field) = self.image.difference(&image) {
            return Err(Error::unavailable(format!(
                "{shown} changed after its distro binding was made: its {field} differs"
            )));
        }
        match &self.manifest {
            Some(expected) => {
                let opened = match (access.open)(&manifest) {
                    Ok(opened) => opened,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Err(Error::unavailable(format!(
                            "{manifest_shown} disappeared after the distro binding of {shown} \
                             was made"
                        )));
                    }
                    Err(error) => return Err(os_error(&error, Some(&manifest))),
                };
                let sealed =
                    (access.seal)(&opened).map_err(|error| os_error(&error, Some(&manifest)))?;
                if let Some(field) = expected.difference(&sealed) {
                    return Err(Error::unavailable(format!(
                        "{manifest_shown} changed after the distro binding of {shown} was made: \
                         its {field} differs"
                    )));
                }
            }
            None => match (access.open)(&manifest) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                // A manifest whose lookup fails otherwise may be present: the check fails.
                Err(error) => return Err(os_error(&error, Some(&manifest))),
                Ok(_) => {
                    return Err(Error::unavailable(format!(
                        "{manifest_shown} appeared after the distro binding of {shown} was made"
                    )));
                }
            },
        }
        Ok(CheckedDistro {
            file,
            path: path.to_path_buf(),
            format: self.format,
            identity: self.identity.clone(),
        })
    }
}

/// Implements [`DistroBinding::verify`], reaching files through `access`.
pub(crate) fn verify_with(layer: &SandboxLayer, access: Access<'_>) -> Result<DistroBinding> {
    let path = layer.path();
    let manifest = manifest_path(path)?;
    if !manifest.exists() {
        let image = seal_regular(path, access, &layer_description(layer))?;
        return Ok(DistroBinding::new(
            layer,
            image,
            None,
            PolicyVerdict::ACCEPTED,
        ));
    }
    let invalid = |detail: String| refused(format!("invalid sandbox distro manifest: {detail}"));
    let (bytes, manifest_seal) =
        read_sealed(&manifest, access).map_err(|failure| match failure {
            Failure::Io(error) => invalid(format!("{manifest:?}: {error}")),
            Failure::Changed(field) => changed_while_verified(&manifest, field),
        })?;
    // The JSON reader of egress policy files reads the manifest alike whatever the features of
    // `serde_json`, which another crate of a build may change: its `arbitrary_precision` would
    // otherwise read an object that holds a number as that number, such as a `format` of
    // `{"$serde_json::private::Number": "1"}` as 1.
    let text =
        std::str::from_utf8(&bytes).map_err(|error| invalid(format!("not UTF-8: {error}")))?;
    let document = json::read_document(text, "manifest").map_err(|error| {
        invalid(match error {
            ReadError::Syntax(error) => error.to_string(),
            ReadError::TooDeep { line, column } => format!(
                "arrays and objects nest more than {MAX_JSON_DEPTH} deep at line {line} column \
                 {column}"
            ),
            ReadError::Repeated { object, field } => {
                format!("{object} has duplicate field {field:?}")
            }
        })
    })?;
    if !document.is_object() {
        return Err(invalid("expected a JSON object".to_owned()));
    }
    let mismatch = || refused("sandbox distro manifest does not match its artifact");
    if document.get("format").and_then(Value::as_u64) != Some(1) {
        return Err(mismatch());
    }
    if document.get("artifact").and_then(Value::as_str)
        != path.file_name().and_then(|name| name.to_str())
    {
        return Err(mismatch());
    }
    let (digest, image) = hash_sealed(path, access, &layer_description(layer))?;
    if document.get("artifact_sha256").and_then(Value::as_str) != Some(digest.as_str()) {
        return Err(mismatch());
    }
    let invalid_packages = || refused("sandbox distro manifest has invalid package metadata");
    let Some(packages) = document.get("packages").and_then(Value::as_array) else {
        return Err(invalid_packages());
    };
    let mut systemd = false;
    for package in packages {
        let Some(name) = package.get("name").and_then(Value::as_str) else {
            return Err(invalid_packages());
        };
        systemd |= name == "systemd";
    }
    if systemd {
        return Err(refused(
            "systemd images are unsupported by the sandbox security profile",
        ));
    }
    Ok(DistroBinding::new(
        layer,
        image,
        Some(manifest_seal),
        PolicyVerdict::ACCEPTED,
    ))
}

/// Implements [`DistroBinding::import`], reaching files through `access`.
pub(crate) fn import_with(
    layer: &SandboxLayer,
    verdict: PolicyVerdict,
    access: Access<'_>,
) -> Result<DistroBinding> {
    let path = layer.path();
    let manifest = manifest_path(path)?;
    let image = seal_regular(path, access, &layer_description(layer))?;
    let manifest = seal_if_present(&manifest, access, "sandbox distro manifest")?;
    Ok(DistroBinding::new(layer, image, manifest, verdict))
}

/// Returns how messages name the image of `layer`: `distro EROFS layer` or `distro GPT image`.
fn layer_description(layer: &SandboxLayer) -> String {
    match layer.format() {
        DistroFormat::Erofs => format!("{} EROFS layer", layer.role()),
        DistroFormat::Gpt => format!("{} GPT image", layer.role()),
    }
}

/// Returns the path of the manifest of the layer at `path`: `LAYER.manifest.json` beside it.
///
/// Fails for a layer path without a file name.
fn manifest_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            refused(format!(
                "sandbox distro layer path has no file name: {path:?}"
            ))
        })?;
    Ok(path.with_file_name(format!("{name}{DISTRO_MANIFEST_SUFFIX}")))
}

/// Returns the error of a file, which messages call `description`, that cannot be opened:
/// unavailable if it is missing, as `require_file` refuses a missing file.
fn open_error(error: &io::Error, path: &Path, description: &str) -> Error {
    if error.kind() == io::ErrorKind::NotFound {
        Error::unavailable(format!("{description} not found: {}", path.display()))
    } else {
        os_error(error, Some(path))
    }
}

/// Opens and seals the regular file at `path`, which messages call `description`, through
/// `access`, without reading it.
fn seal_regular(path: &Path, access: Access<'_>, description: &str) -> Result<FileSeal> {
    seal_if_present(path, access, description)?
        .ok_or_else(|| Error::unavailable(format!("{description} not found: {}", path.display())))
}

/// Opens and seals the regular file at `path`, which messages call `description`, through
/// `access`, without reading it, or returns `None` if nothing is at `path`. A lookup that fails
/// for another reason fails, rather than count as no file.
fn seal_if_present(path: &Path, access: Access<'_>, description: &str) -> Result<Option<FileSeal>> {
    let file = match (access.open)(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(os_error(&error, Some(path))),
    };
    let seal = (access.seal)(&file).map_err(|error| os_error(&error, Some(path)))?;
    require_regular(&seal, path, description)?;
    Ok(Some(seal))
}

/// Refuses a seal of a file that is not a regular file, as `require_file` refuses its path.
fn require_regular(seal: &FileSeal, path: &Path, description: &str) -> Result<()> {
    if seal.kind() == FileKind::Regular {
        Ok(())
    } else {
        Err(Error::unavailable(format!(
            "{description} not found: {}",
            path.display()
        )))
    }
}

/// Returns the error of a file whose `field` changed while verification read it.
fn changed_while_verified(path: &Path, field: &str) -> Error {
    Error::io(format!(
        "{} changed while it was verified: its {field} differs",
        path.display()
    ))
}

/// Why reading a sealed file failed.
enum Failure {
    /// The host refused an operation.
    Io(io::Error),
    /// The file's metadata changed while it was read, first the field named.
    Changed(&'static str),
}

/// Opens the file at `path`, and returns its content and the seal that it had while it was read,
/// both through `access`.
fn read_sealed(
    path: &Path,
    access: Access<'_>,
) -> std::result::Result<(Vec<u8>, FileSeal), Failure> {
    let mut file = (access.open)(path).map_err(Failure::Io)?;
    let before = (access.seal)(&file).map_err(Failure::Io)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(Failure::Io)?;
    let after = (access.seal)(&file).map_err(Failure::Io)?;
    match before.difference(&after) {
        Some(field) => Err(Failure::Changed(field)),
        None => Ok((bytes, before)),
    }
}

/// Opens the regular file at `path`, which messages call `description`, and returns the
/// lowercase hexadecimal SHA-256 digest of its content and the seal that it had while it was
/// hashed, both through `access`.
fn hash_sealed(path: &Path, access: Access<'_>, description: &str) -> Result<(String, FileSeal)> {
    let io_error = |error: io::Error| os_error(&error, Some(path));
    let mut file = (access.open)(path).map_err(io_error)?;
    let before = (access.seal)(&file).map_err(io_error)?;
    require_regular(&before, path, description)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(error)),
        };
        hasher.update(&buffer[..read]);
    }
    let after = (access.seal)(&file).map_err(io_error)?;
    if let Some(field) = before.difference(&after) {
        return Err(changed_while_verified(path, field));
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((digest, before))
}

#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests;
