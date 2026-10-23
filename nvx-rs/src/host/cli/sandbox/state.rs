//! The CLI's own record in a state directory, `config.json`, which `provision` writes in format 7
//! and `start` also reads in format 1, and the JSON reading and writing of the former
//! `sandbox_lifecycle.py`.
//!
//! - [`write_json`] writes `json.dumps(value, indent=2, sort_keys=True) + "\n"` as
//!   `Path.write_text` does, with the `\n`s turned into `os.linesep`, into a temporary file of
//!   mode 0600 that then replaces the file.
//! - [`read_json`] reads a file as `json.loads(path.read_text(encoding="utf-8"))` does, and
//!   accepts only an object whose version field equals, as `==` compares, one of the given
//!   versions.
//! - [`serialize_launch`] writes a provisioned launch in format 7, with the export plan of its
//!   shares under `export`, `null` without shares, and [`deserialize_launch`] reads it back with
//!   the conversions, defaults, and checks of `_deserialize_launch`. It also reads format 1, which
//!   earlier builds wrote for a sandbox without shares, with `mount` `null` and no `export`.
//!   Formats 2 to 6 recorded the shares as they were requested; `start` no longer reads them, and
//!   such a sandbox is provisioned again.
//! - Every configuration also records the [`DistroBinding`] of the sandbox's distro layer, under
//!   `distro_binding` in its serde form, which [`deserialize_binding`] reads back for `start` to
//!   check. `nvx.py` never wrote one, and `start` refuses a configuration without one. The
//!   binding is of the layer at the path that the configuration records, which a
//!   [`RecordedDistro`] holds.
//! - It records the [`ScratchBinding`] of the sandbox's scratch image too, under
//!   `scratch_binding`, which [`deserialize_scratch_binding`] reads back for `start` to check and
//!   claim, of the image at the path that the configuration records, which a [`RecordedScratch`]
//!   holds. `start` refuses a configuration without one as well.
//! - Every configuration also records the sandbox's [`SandboxNetwork`], under `network` in the
//!   serde form of the plan's network, or `null` for no network device, which
//!   [`deserialize_network`] reads back for `start` to render. It replaces the network options
//!   that `nvx.py` recorded, which no reader reads: `start` refuses a configuration without it.

use std::fs::OpenOptions;
use std::io::Write;

use super::lifecycle::ProvisionConfig;
use crate::ErrorCode;
use crate::host::cli::output::LINE_SEPARATOR;
use crate::host::cli::{Exit, Result};
use crate::host::filesystem::ExportPlan;
use crate::host::python::builtins::{Int, repr};
use crate::host::python::dumps::{self, Value, dict};
use crate::host::python::fs::{self, os_error_message};
use crate::host::python::json::{self, Document, Loads, Node, NodeId, ReadError};
use crate::host::python::path::PurePath;
use crate::host::python::text::decode_utf8;
use crate::host::python::value::{self, IntError, equals_int, py_int, py_str};
use crate::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
use crate::host::sandbox::layer::{LayerRole, SandboxLayer};
use crate::host::sandbox::scratch::ScratchBinding;
use crate::host::sandbox::{SandboxLaunch, SandboxNetwork, SandboxSettings};

/// The configuration that earlier builds wrote for a sandbox without shares, whose `mount` is
/// `null`, which `start` still reads.
pub(crate) const CONFIG_FORMAT: u64 = 1;
/// The configuration that `provision` writes, which records the export plan of the sandbox's
/// shares under `export`, in its serde form, or `null` for a sandbox without shares.
pub(crate) const EXPORT_CONFIG_FORMAT: u64 = 7;
/// Every configuration format that `start` reads.
pub(crate) const CONFIG_FORMATS: [u64; 2] = [CONFIG_FORMAT, EXPORT_CONFIG_FORMAT];
/// The member of a configuration that records the binding of the sandbox's distro layer.
pub(crate) const DISTRO_BINDING: &str = "distro_binding";
/// The member of a configuration that records the binding of the sandbox's scratch image.
pub(crate) const SCRATCH_BINDING: &str = "scratch_binding";
/// How deeply a recorded binding may nest its values, which its serde form nests three deep.
const MAX_BINDING_DEPTH: usize = 8;
/// The member of a configuration that records the export plan of its shares.
pub(crate) const EXPORT: &str = "export";
/// How deeply a recorded export plan may nest its values, which its serde form nests five deep.
const MAX_EXPORT_DEPTH: usize = 8;
/// The member of a configuration that records the sandbox's network, or `null` for none.
pub(crate) const NETWORK: &str = "network";
/// How deeply a recorded network may nest its values, which its serde form nests eight deep, to
/// the exceptions of a rule's destination.
const MAX_NETWORK_DEPTH: usize = 10;

/// Returns the name of a temporary file next to `path`: `.NAME.UUID.tmp`, with a fresh UUID4 in
/// hexadecimal.
pub(crate) fn temporary_path(path: &PurePath) -> Result<PurePath> {
    let name = format!(".{}.{}.tmp", path.name(), uuid4_hex()?);
    path.with_name(&name)
        .ok_or_else(|| Exit::error(ErrorCode::Other, format!("{path} has an empty name")))
}

/// Returns `uuid.uuid4().hex`: 32 lowercase hexadecimal digits of 122 random bits, with the
/// version and variant bits of a version-4 UUID.
pub(crate) fn uuid4_hex() -> Result<String> {
    let mut bytes = random_bytes::<16>()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(hex(&bytes))
}

/// Returns `N` random bytes from the operating system, as `secrets` and `uuid` take them.
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

/// Returns `str(error)` for an `OSError` that names two files, as `os.replace` raises it.
fn os_error_with_two_paths(error: &std::io::Error, source: &str, destination: &str) -> String {
    let message = os_error_message(error, Some(source));
    match error.raw_os_error() {
        Some(_) => format!("{message} -> {}", repr(destination)),
        None => message,
    }
}

/// Atomically replaces `path` with `value` as `_write_json` writes it: the JSON text of
/// `json.dumps(value, indent=2, sort_keys=True)` and a newline, with `os.linesep` for each
/// newline, in a temporary file of mode 0600 that then replaces `path`.
pub(crate) fn write_json(path: &PurePath, value: &Value) -> Result {
    let temporary = temporary_path(path)?;
    let text = format!("{}\n", dumps::dumps(value)).replace('\n', LINE_SEPARATOR);
    let written = (|| -> Result {
        let failed = |error: std::io::Error| {
            Exit::error(
                ErrorCode::Io,
                os_error_message(&error, Some(temporary.as_str())),
            )
        };
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file = options
            .open(temporary.as_path())
            .and_then(|mut file| file.write_all(text.as_bytes()).map(|()| file))
            .map_err(failed)?;
        file.flush().map_err(failed)?;
        drop(file);
        set_private(&temporary)?;
        std::fs::rename(temporary.as_path(), path.as_path()).map_err(|error| {
            Exit::error(
                ErrorCode::Io,
                os_error_with_two_paths(&error, temporary.as_str(), path.as_str()),
            )
        })
    })();
    let _ = std::fs::remove_file(temporary.as_path());
    written
}

/// Gives `path` mode 0600, as `os.chmod(path, 0o600)` does: on Windows, that only clears the
/// read-only attribute.
fn set_private(path: &PurePath) -> Result {
    let error = |error: std::io::Error| {
        Exit::error(ErrorCode::Io, os_error_message(&error, Some(path.as_str())))
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path.as_path(), std::fs::Permissions::from_mode(0o600))
            .map_err(error)
    }
    #[cfg(not(unix))]
    {
        let mut permissions = std::fs::metadata(path.as_path())
            .map_err(error)?
            .permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            std::fs::set_permissions(path.as_path(), permissions).map_err(error)?;
        }
        Ok(())
    }
}

/// Returns the member `name` of the object at `id`, as `value.get(name)` finds it in the dict
/// that Python builds, in which the last of repeated names wins.
pub(crate) fn member(document: &Document, id: NodeId, name: &str) -> Option<NodeId> {
    let Node::Object(members) = document.node(id) else {
        return None;
    };
    members
        .iter()
        .rev()
        .find(|(member, _)| member.as_bytes() == name.as_bytes())
        .map(|&(_, value)| value)
}

/// Reads the JSON object at `path`, as `_read_json` does: it must hold `version_field` equal, as
/// `==` compares, to one of `versions`.
///
/// A file that cannot be read, is not UTF-8, or is not JSON fails as `failed to read
/// DESCRIPTION: PATH`, with the code [`ErrorCode::Io`], and another value or version as
/// `DESCRIPTION has an unsupported format: PATH`, with [`ErrorCode::FormatUnsupported`]. An
/// integer of more than 4300 digits fails with the `ValueError` of `int`.
pub(crate) fn read_json(
    path: &PurePath,
    description: &str,
    version_field: &str,
    versions: &[u64],
) -> Result<Document> {
    let unreadable = || {
        Exit::error(
            ErrorCode::Io,
            format!("failed to read {description}: {path}"),
        )
    };
    let bytes = std::fs::read(path.as_path()).map_err(|_| unreadable())?;
    // `read_text` decodes strictly and translates universal newlines.
    let text = decode_utf8(&bytes)
        .map_err(|_| unreadable())?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let document = match json::read(&text, &mut Loads) {
        Ok(document) => document,
        Err(ReadError::Syntax { .. }) => return Err(unreadable()),
        Err(ReadError::Hook(error)) => {
            return Err(Exit::error(ErrorCode::Io, error.message(&text)));
        }
    };
    let unsupported = || {
        Exit::error(
            ErrorCode::FormatUnsupported,
            format!("{description} has an unsupported format: {path}"),
        )
    };
    let root = document.root();
    if !matches!(document.node(root), Node::Object(_)) {
        return Err(unsupported());
    }
    let version = member(&document, root, version_field);
    if !version.is_some_and(|version| {
        versions
            .iter()
            .any(|&expected| equals_int(&document, version, expected))
    }) {
        return Err(unsupported());
    }
    Ok(document)
}

/// Returns `path.resolve()` as text.
fn resolved(path: &std::path::Path) -> Result<String> {
    let text = crate::host::cli::sandbox::path_string(path)?;
    let resolved = fs::resolve(&PurePath::native(&text)).map_err(Exit::from_os_error)?;
    Ok(resolved.as_str().to_owned())
}

/// The distro layer that `provision` records, at the path that it records for it, with the
/// binding of the layer at that path, which `start` checks.
pub(crate) struct RecordedDistro {
    /// `path.resolve()` of the layer's path, as `provision` records it.
    path: String,
    /// The layer at that path.
    layer: SandboxLayer,
    /// The binding of the layer at that path.
    binding: DistroBinding,
}

impl RecordedDistro {
    /// Returns the distro layer of `launch` at the path that `provision` records, `path.resolve()`
    /// of the path given, with a binding of the layer there: `binding`, which explicit
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
    /// `path.resolve()` of the image's path, as `provision` records it.
    path: String,
    /// The binding of the image at that path.
    binding: ScratchBinding,
}

impl RecordedScratch {
    /// Returns the scratch image of `launch` at the path that `provision` records,
    /// `path.resolve()` of the path given, with a binding of the image there, which claims
    /// nothing.
    pub(crate) fn new(launch: &SandboxLaunch) -> Result<Self> {
        let path = resolved(launch.scratch())?;
        let binding = ScratchBinding::bind(std::path::Path::new(&path))?;
        Ok(Self { path, binding })
    }
}

/// Returns the configuration that `provision` records for `launch` and the other options, as
/// `_serialize_launch` builds it, with `distro`, the launch's distro layer at the path that
/// `provision` verified and records, and its binding, and `scratch`, the launch's scratch image at
/// the path that `provision` records, and its binding.
pub(crate) fn serialize_launch(
    launch: &SandboxLaunch,
    distro: &RecordedDistro,
    scratch: &RecordedScratch,
    config: &ProvisionConfig<'_>,
) -> Result<Value> {
    // The distro layer is the one layer that a launch holds.
    let layer = &distro.layer;
    let layers = vec![dict([
        ("role", Value::from(layer.role().as_str())),
        ("path", Value::from(distro.path.as_str())),
        ("uuid", Value::from(layer.uuid())),
    ])];
    let settings = launch.settings();
    let (uid, gid) = settings.workload_identity;
    let network = match launch.network() {
        Some(network) => serde_json::to_value(network)
            .ok()
            .as_ref()
            .and_then(python_value)
            .ok_or_else(|| Exit::error(ErrorCode::Other, "cannot record the network"))?,
        None => Value::Null,
    };
    let mut value = dict([
        ("format", Value::from(EXPORT_CONFIG_FORMAT)),
        ("layers", Value::List(layers)),
        ("scratch", Value::from(scratch.path.as_str())),
        ("hostname", Value::from(settings.hostname.as_str())),
        ("workload_uid", Value::from(uid)),
        ("workload_gid", Value::from(gid)),
        ("memory_max", Value::from(settings.memory_max.clone())),
        ("pids_max", Value::from(settings.pids_max.clone())),
        ("hypervisor", Value::from(config.hypervisor)),
        ("memory_mib", Value::from(config.memory_mib.clone())),
        (NETWORK, network),
        ("cmdline", Value::from(config.cmdline)),
    ]);
    let Value::Dict(members) = &mut value else {
        unreachable!("dict builds a dict")
    };
    let binding = serde_json::to_value(&distro.binding)
        .ok()
        .as_ref()
        .and_then(python_value)
        .ok_or_else(|| Exit::error(ErrorCode::Other, "cannot record the distro binding"))?;
    members.insert(DISTRO_BINDING.to_owned(), binding);
    let binding = serde_json::to_value(&scratch.binding)
        .ok()
        .as_ref()
        .and_then(python_value)
        .ok_or_else(|| Exit::error(ErrorCode::Other, "cannot record the scratch binding"))?;
    members.insert(SCRATCH_BINDING.to_owned(), binding);
    let export = match launch.export() {
        Some(export) => serde_json::to_value(export)
            .ok()
            .as_ref()
            .and_then(python_value)
            .ok_or_else(|| {
                Exit::error(
                    ErrorCode::Other,
                    "cannot record the export plan of the shares",
                )
            })?,
        None => Value::Null,
    };
    members.insert(EXPORT.to_owned(), export);
    Ok(value)
}

/// Returns the value that `dumps` writes for `value`, a serde value of integers, strings, and
/// containers, or `None` for a value with a float.
fn python_value(value: &serde_json::Value) -> Option<Value> {
    use serde_json::Value as Json;

    Some(match value {
        Json::Null => Value::Null,
        Json::Bool(boolean) => Value::Bool(*boolean),
        Json::Number(number) => number
            .as_u64()
            .map(Value::from)
            .or_else(|| number.as_i64().map(Value::from))?,
        Json::String(text) => Value::from(text.as_str()),
        Json::Array(items) => Value::List(items.iter().map(python_value).collect::<Option<_>>()?),
        Json::Object(members) => Value::Dict(
            members
                .iter()
                .map(|(name, member)| Some((name.clone(), python_value(member)?)))
                .collect::<Option<_>>()?,
        ),
    })
}

/// Returns the serde value of the value at `id`, as the dict that Python builds holds it, in
/// which the last of repeated names wins, or `None` for a value nested more than `depth` levels
/// deep, a string that is not Unicode, or a number that JSON cannot hold.
fn json_value(document: &Document, id: NodeId, depth: usize) -> Option<serde_json::Value> {
    use serde_json::Value as Json;

    let depth = depth.checked_sub(1)?;
    Some(match document.node(id) {
        Node::Null => Json::Null,
        Node::Bool(boolean) => Json::Bool(*boolean),
        Node::Integer(literal) => Json::Number(literal.parse().ok()?),
        Node::Float(float) => Json::Number(serde_json::Number::from_f64(*float)?),
        Node::String(text) => Json::String(text.as_str()?.to_owned()),
        Node::Array(items) => Json::Array(
            items
                .iter()
                .map(|&item| json_value(document, item, depth))
                .collect::<Option<_>>()?,
        ),
        Node::Object(members) => {
            let mut object = serde_json::Map::new();
            for (name, member) in members {
                object.insert(
                    name.as_str()?.to_owned(),
                    json_value(document, *member, depth)?,
                );
            }
            Json::Object(object)
        }
    })
}

/// Reads the binding of the distro layer that a configuration records, which `start` checks.
///
/// A configuration without one, as `nvx.py` and earlier releases of `nvx-rs` wrote, fails as
/// `sandbox configuration has no distro binding`: the sandbox is provisioned again rather than
/// migrated. A binding that is not of the binding's serde form fails as `sandbox configuration is
/// malformed`.
pub(crate) fn deserialize_binding(document: &Document) -> Result<DistroBinding> {
    deserialize_recorded(document, DISTRO_BINDING, "distro binding")
}

/// Reads the binding of the scratch image that a configuration records, which `start` checks and
/// claims.
///
/// A configuration without one, as earlier releases of `nvx-rs` wrote, fails as `sandbox
/// configuration has no scratch binding`, and one that is not of the binding's serde form as
/// `sandbox configuration is malformed`, as [`deserialize_binding`] fails.
pub(crate) fn deserialize_scratch_binding(document: &Document) -> Result<ScratchBinding> {
    deserialize_recorded(document, SCRATCH_BINDING, "scratch binding")
}

/// Reads the serde form of the member `name` of a configuration, a binding that messages call
/// `description`, as [`deserialize_binding`] reads that of the distro layer.
fn deserialize_recorded<T: serde::de::DeserializeOwned>(
    document: &Document,
    name: &str,
    description: &str,
) -> Result<T> {
    let Some(binding) = member(document, document.root(), name) else {
        return Err(Exit::error(
            ErrorCode::FormatUnsupported,
            format!(
                "sandbox configuration has no {description}; deprovision the sandbox and \
                 provision it again"
            ),
        ));
    };
    json_value(document, binding, MAX_BINDING_DEPTH)
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(malformed)
}

/// Returns the error of a configuration that is not of the form that this build writes.
fn malformed() -> Exit {
    Exit::error(ErrorCode::Io, "sandbox configuration is malformed")
}

/// Reads the network that a configuration records, in its typed form, which `start` renders, or
/// `None` for no network device.
///
/// A configuration without one, as `nvx.py` and earlier releases of `nvx-rs` wrote with their
/// network options, fails as `sandbox configuration has no network policy`: the sandbox is
/// provisioned again rather than migrated. A network that is not of the network's serde form, or
/// whose policy [`SandboxNetwork::new`] refuses, fails as `sandbox configuration is malformed`.
pub(crate) fn deserialize_network(document: &Document) -> Result<Option<SandboxNetwork>> {
    let Some(network) = member(document, document.root(), NETWORK) else {
        return Err(Exit::error(
            ErrorCode::FormatUnsupported,
            "sandbox configuration has no network policy; deprovision the sandbox and provision \
             it again",
        ));
    };
    json_value(document, network, MAX_NETWORK_DEPTH)
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(malformed)
}

/// Why reading a recorded launch failed.
enum Malformed {
    /// The `KeyError`, `TypeError`, or `ValueError` that `_deserialize_launch` reports as a
    /// malformed configuration.
    Malformed,
    /// Any other failure, which it reports as it is.
    Exit(Exit),
}

impl From<Exit> for Malformed {
    fn from(exit: Exit) -> Self {
        Self::Exit(exit)
    }
}

impl From<crate::Error> for Malformed {
    fn from(error: crate::Error) -> Self {
        Self::Exit(error.into())
    }
}

/// Returns `str(value[name])` of the object at `id`.
fn text_member(
    document: &Document,
    id: NodeId,
    name: &str,
) -> std::result::Result<String, Malformed> {
    let value = member(document, id, name).ok_or(Malformed::Malformed)?;
    py_str(document, value).ok_or(Malformed::Malformed)
}

/// Returns `int(value)`, which `_deserialize_launch` reports as malformed unless it overflows.
fn integer(document: &Document, id: NodeId) -> std::result::Result<Int, Malformed> {
    py_int(document, id).map_err(|error| match error {
        IntError::Invalid => Malformed::Malformed,
        IntError::Overflow => Malformed::Exit(Exit::error(
            ErrorCode::Io,
            "cannot convert float infinity to integer",
        )),
    })
}

/// Returns the items of the list or other iterable that Python iterates at `id`, as `for item in
/// value` sees them: a list's items, a dict's names, and a string's characters, which are no
/// mappings, so that only a list of objects reads as layers.
fn iterate(document: &Document, id: NodeId) -> std::result::Result<Vec<Option<NodeId>>, Malformed> {
    match document.node(id) {
        Node::Array(items) => Ok(items.iter().map(|&item| Some(item)).collect()),
        Node::Object(members) => Ok(members.iter().map(|_| None).collect()),
        Node::String(text) => Ok(text.code_points().map(|_| None).collect()),
        _ => Err(Malformed::Malformed),
    }
}

/// The shares that a configuration records.
struct RecordedShares {
    /// Whether it records them under `export`, as format 7 does.
    exported: bool,
    /// Whether it records any.
    any: bool,
    /// Their export plan, if it records one.
    plan: Option<ExportPlan>,
}

/// Reads the shares that a configuration records: in format 7, the export plan under `export`, in
/// its serde form, or `null` for a sandbox without shares, and in format 1, none, with `mount`
/// `null` or missing.
///
/// A configuration with a share under `mount`, which only formats that `start` no longer reads
/// recorded, reads as one with shares but no plan, which [`deserialize_launch`] refuses.
fn deserialize_export(
    document: &Document,
    root: NodeId,
) -> std::result::Result<RecordedShares, Malformed> {
    if let Some(export) = member(document, root, EXPORT) {
        let plan = if matches!(document.node(export), Node::Null) {
            None
        } else {
            Some(
                json_value(document, export, MAX_EXPORT_DEPTH)
                    .and_then(|value| serde_json::from_value(value).ok())
                    .ok_or(Malformed::Malformed)?,
            )
        };
        return Ok(RecordedShares {
            exported: true,
            any: plan.is_some(),
            plan,
        });
    }
    let any = member(document, root, "mount")
        .is_some_and(|mount| !matches!(document.node(mount), Node::Null));
    Ok(RecordedShares {
        exported: false,
        any,
        plan: None,
    })
}

/// Reads the launch that a configuration records, as `_deserialize_launch` does, and checks it
/// against the host.
///
/// A missing field, or a value of the wrong type, fails as `sandbox configuration is malformed`,
/// as does a layer whose role is not `distro` or an export plan that is not of its serde form. A
/// configuration whose format is not the one that its shares need, format 7 for one that records
/// `export` and format 1 for one that does not, fails as `sandbox configuration format does not
/// match its mounts`. The export plan must then pass the checks of
/// [`SandboxLaunch::with_export`], and the launch those of [`SandboxLaunch::validated`], which
/// checks again the host objects that the plan recorded.
pub(crate) fn deserialize_launch(document: &Document) -> Result<SandboxLaunch> {
    let (launch, shares) = (|| -> std::result::Result<_, Malformed> {
        let root = document.root();
        let layers_value = member(document, root, "layers").ok_or(Malformed::Malformed)?;
        let mut layers = Vec::new();
        for layer in iterate(document, layers_value)? {
            let layer = layer.ok_or(Malformed::Malformed)?;
            if !matches!(document.node(layer), Node::Object(_)) {
                return Err(Malformed::Malformed);
            }
            let role = text_member(document, layer, "role")?;
            let path = text_member(document, layer, "path")?;
            let uuid = text_member(document, layer, "uuid")?;
            let role = LayerRole::from_name(&role).ok_or(Malformed::Malformed)?;
            layers.push(SandboxLayer::new(role, &path, uuid));
        }
        let identity = |name| -> std::result::Result<u32, Malformed> {
            let value = member(document, root, name).ok_or(Malformed::Malformed)?;
            // An identity outside the range of IDs fails as root's does.
            Ok(integer(document, value)?
                .to_u32()
                .filter(|&id| id != 0)
                .unwrap_or(0))
        };
        let workload_identity = (identity("workload_uid")?, identity("workload_gid")?);
        let scratch = text_member(document, root, "scratch")?;
        let hostname = text_member(document, root, "hostname")?;
        let limit = |name| -> std::result::Result<Option<Int>, Malformed> {
            let value = member(document, root, name).ok_or(Malformed::Malformed)?;
            if matches!(document.node(value), Node::Null) {
                return Ok(None);
            }
            integer(document, value).map(Some)
        };
        let memory_max = limit("memory_max")?;
        let pids_max = limit("pids_max")?;
        let shares = deserialize_export(document, root)?;
        let launch = SandboxLaunch::new(
            layers,
            &scratch,
            SandboxSettings {
                hostname,
                workload_identity,
                memory_max,
                pids_max,
                ..SandboxSettings::default()
            },
        )?;
        Ok((launch, shares))
    })()
    .map_err(|error| match error {
        Malformed::Malformed => malformed(),
        Malformed::Exit(exit) => exit,
    })?;
    let format = member(document, document.root(), "format");
    let expected = if shares.exported {
        EXPORT_CONFIG_FORMAT
    } else {
        CONFIG_FORMAT
    };
    if (shares.any && shares.plan.is_none())
        || !format.is_some_and(|format| equals_int(document, format, expected))
    {
        return Err(Exit::error(
            ErrorCode::FormatUnsupported,
            "sandbox configuration format does not match its mounts",
        ));
    }
    let launch = launch.with_export(shares.plan)?;
    launch.validated()?;
    Ok(launch)
}

/// Returns `str(value)` of the member `name` of the configuration, failing as Python's
/// `KeyError` reads, `'NAME'`, if the member is missing.
pub(crate) fn config_text(document: &Document, name: &str) -> Result<String> {
    let value = member(document, document.root(), name)
        .ok_or_else(|| Exit::error(ErrorCode::Io, repr(name)))?;
    py_str(document, value).ok_or_else(malformed)
}

/// Returns `int(config[name])`, failing with the `KeyError`, `TypeError`, `ValueError`, or
/// `OverflowError` that Python raises.
pub(crate) fn config_int(document: &Document, name: &str) -> Result<Int> {
    let value = member(document, document.root(), name)
        .ok_or_else(|| Exit::error(ErrorCode::Io, repr(name)))?;
    py_int(document, value).map_err(|error| match error {
        IntError::Overflow => {
            Exit::error(ErrorCode::Io, "cannot convert float infinity to integer")
        }
        IntError::Invalid => Exit::error(ErrorCode::Io, int_error(document, value)),
    })
}

/// Returns the message of the error with which `int(value)` refuses the value at `id`.
fn int_error(document: &Document, id: NodeId) -> String {
    let type_error = |name: &str| {
        format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{name}'"
        )
    };
    match document.node(id) {
        Node::Null => type_error("NoneType"),
        Node::Array(_) => type_error("list"),
        Node::Object(_) => type_error("dict"),
        Node::Float(_) => "cannot convert float NaN to integer".to_owned(),
        Node::String(text) => match text.as_str() {
            Some(text) => match Int::parse(text) {
                Err(error) => error.message(text),
                Ok(_) => String::new(),
            },
            None => format!(
                "invalid literal for int() with base 10: {}",
                value::py_repr(document, id).unwrap_or_default()
            ),
        },
        Node::Integer(_) | Node::Bool(_) => String::new(),
    }
}
