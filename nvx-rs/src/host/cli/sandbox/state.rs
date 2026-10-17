//! The files of a state directory: `config.json` in formats 1 to 6, `runtime.json`, and the JSON
//! reading and writing of `sandbox_lifecycle.py`.
//!
//! - [`write_json`] writes `json.dumps(value, indent=2, sort_keys=True) + "\n"` as
//!   `Path.write_text` does, with the `\n`s turned into `os.linesep`, into a temporary file of
//!   mode 0600 that then replaces the file.
//! - [`read_json`] reads a file as `json.loads(path.read_text(encoding="utf-8"))` does, and
//!   accepts only an object whose version field equals, as `==` compares, one of the given
//!   versions.
//! - [`serialize_launch`] writes a provisioned launch in the lowest format that holds it, and
//!   [`deserialize_launch`] reads it back with the conversions, defaults, and checks of
//!   `_deserialize_launch`.

use std::fs::OpenOptions;
use std::io::Write;

use super::lifecycle::ProvisionConfig;
use crate::host::cli::output::LINE_SEPARATOR;
use crate::host::cli::{Exit, Result};
use crate::host::python::builtins::{Int, repr};
use crate::host::python::dumps::{self, Value, dict};
use crate::host::python::fs::{self, os_error_message};
use crate::host::python::json::{self, Document, Loads, Node, NodeId, ReadError};
use crate::host::python::path::PurePath;
use crate::host::python::text::decode_utf8;
use crate::host::python::value::{self, IntError, equals_int, py_int, py_str};
use crate::host::sandbox::layer::{LayerRole, SandboxLayer};
use crate::host::sandbox::mount::{
    MountAccess, MountKind, MountOwner, MountPolicy, SandboxMount, validate_mount_target,
};
use crate::host::sandbox::{SandboxLaunch, SandboxSettings};

/// The format of `runtime.json`, and the version of the files that `_read_json` reads by default.
pub(crate) const STATE_FORMAT: u64 = 1;
/// The configuration of a sandbox without shares.
pub(crate) const CONFIG_FORMAT: u64 = 1;
/// The configuration of a sandbox with one share, which format-1 readers would start without it.
pub(crate) const MOUNT_CONFIG_FORMAT: u64 = 2;
/// The configuration of a sandbox with one share that the caller owns, which format-2 readers
/// would start as the VMM.
pub(crate) const OWNER_CONFIG_FORMAT: u64 = 3;
/// The configuration of a sandbox with several shares, listed under `mounts`.
pub(crate) const MULTI_MOUNT_CONFIG_FORMAT: u64 = 4;
/// The configuration of a sandbox whose shares have allowed or writable paths.
pub(crate) const POLICY_CONFIG_FORMAT: u64 = 5;
/// The configuration of a sandbox that shares a file, with the kind of each share.
pub(crate) const FILE_MOUNT_CONFIG_FORMAT: u64 = 6;
/// Every configuration format that `start` reads.
pub(crate) const CONFIG_FORMATS: [u64; 6] = [
    CONFIG_FORMAT,
    MOUNT_CONFIG_FORMAT,
    OWNER_CONFIG_FORMAT,
    MULTI_MOUNT_CONFIG_FORMAT,
    POLICY_CONFIG_FORMAT,
    FILE_MOUNT_CONFIG_FORMAT,
];

/// Returns the name of a temporary file next to `path`: `.NAME.UUID.tmp`, with a fresh UUID4 in
/// hexadecimal.
pub(crate) fn temporary_path(path: &PurePath) -> Result<PurePath> {
    let name = format!(".{}.{}.tmp", path.name(), uuid4_hex()?);
    path.with_name(&name)
        .ok_or_else(|| Exit::error(format!("{path} has an empty name")))
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
    getrandom::fill(&mut bytes)
        .map_err(|error| Exit::error(format!("cannot read random bytes: {error}")))?;
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
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file = options
            .open(temporary.as_path())
            .and_then(|mut file| file.write_all(text.as_bytes()).map(|()| file))
            .map_err(|error| Exit::error(os_error_message(&error, Some(temporary.as_str()))))?;
        file.flush()
            .map_err(|error| Exit::error(os_error_message(&error, Some(temporary.as_str()))))?;
        drop(file);
        set_private(&temporary)?;
        std::fs::rename(temporary.as_path(), path.as_path()).map_err(|error| {
            Exit::error(os_error_with_two_paths(
                &error,
                temporary.as_str(),
                path.as_str(),
            ))
        })
    })();
    let _ = std::fs::remove_file(temporary.as_path());
    written
}

/// Gives `path` mode 0600, as `os.chmod(path, 0o600)` does: on Windows, that only clears the
/// read-only attribute.
fn set_private(path: &PurePath) -> Result {
    let error = |error: std::io::Error| Exit::error(os_error_message(&error, Some(path.as_str())));
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
/// DESCRIPTION: PATH`, and another value or version as `DESCRIPTION has an unsupported format:
/// PATH`. An integer of more than 4300 digits fails with the `ValueError` of `int`.
pub(crate) fn read_json(
    path: &PurePath,
    description: &str,
    version_field: &str,
    versions: &[u64],
) -> Result<Document> {
    let unreadable = || Exit::error(format!("failed to read {description}: {path}"));
    let bytes = std::fs::read(path.as_path()).map_err(|_| unreadable())?;
    // `read_text` decodes strictly and translates universal newlines.
    let text = decode_utf8(&bytes)
        .map_err(|_| unreadable())?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let document = match json::read(&text, &mut Loads) {
        Ok(document) => document,
        Err(ReadError::Syntax { .. }) => return Err(unreadable()),
        Err(ReadError::Hook(error)) => return Err(Exit::error(error.message(&text))),
    };
    let unsupported = || Exit::error(format!("{description} has an unsupported format: {path}"));
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

/// Returns the format of the configuration of a sandbox with `mounts`: the lowest that holds
/// them, as `_config_format` chooses it.
pub(crate) fn config_format(mounts: &[SandboxMount]) -> u64 {
    if mounts.iter().any(|mount| mount.kind() == MountKind::File) {
        FILE_MOUNT_CONFIG_FORMAT
    } else if mounts
        .iter()
        .any(|mount| !mount.policy().allowed.is_empty() || !mount.policy().writable.is_empty())
    {
        POLICY_CONFIG_FORMAT
    } else if mounts.len() > 1 {
        MULTI_MOUNT_CONFIG_FORMAT
    } else if mounts.len() == 1 && mounts[0].owner() == MountOwner::Caller {
        OWNER_CONFIG_FORMAT
    } else if mounts.len() == 1 {
        MOUNT_CONFIG_FORMAT
    } else {
        CONFIG_FORMAT
    }
}

/// Returns whether a format lists every share under `mounts`.
fn lists_mounts(format: u64) -> bool {
    matches!(
        format,
        MULTI_MOUNT_CONFIG_FORMAT | POLICY_CONFIG_FORMAT | FILE_MOUNT_CONFIG_FORMAT
    )
}

/// Returns `path.resolve()` as text.
fn resolved(path: &std::path::Path) -> Result<String> {
    let text = crate::host::cli::sandbox::path_string(path)?;
    let resolved = fs::resolve(&PurePath::native(&text))
        .map_err(|error| Exit::error(os_error_message(&error, None)))?;
    Ok(resolved.as_str().to_owned())
}

/// Returns a list of strings.
fn strings(values: &[String]) -> Value {
    Value::List(
        values
            .iter()
            .map(|value| Value::from(value.as_str()))
            .collect(),
    )
}

/// Returns the configuration that `provision` records for `launch` and the other options, as
/// `_serialize_launch` builds it.
pub(crate) fn serialize_launch(
    launch: &SandboxLaunch,
    config: &ProvisionConfig<'_>,
) -> Result<Value> {
    let format = config_format(launch.mounts());
    let mut layers = Vec::new();
    for layer in launch.ordered_layers() {
        layers.push(dict([
            ("role", Value::from(layer.role().as_str())),
            ("path", Value::from(resolved(layer.path())?)),
            ("uuid", Value::from(layer.uuid())),
        ]));
    }
    let settings = launch.settings();
    let (uid, gid) = settings.workload_identity;
    let mut value = dict([
        ("format", Value::from(format)),
        ("layers", Value::List(layers)),
        ("scratch", Value::from(resolved(launch.scratch())?)),
        ("hostname", Value::from(settings.hostname.as_str())),
        ("workload_uid", Value::from(uid)),
        ("workload_gid", Value::from(gid)),
        ("memory_max", Value::from(settings.memory_max.clone())),
        ("pids_max", Value::from(settings.pids_max.clone())),
        ("hypervisor", Value::from(config.hypervisor)),
        ("memory_mib", Value::from(config.memory_mib.clone())),
        ("net", Value::from(config.net)),
        ("network_profile", Value::from(config.network_profile)),
        ("network_egress", Value::from(config.network_egress)),
        ("network_ingress", Value::from(config.network_ingress)),
        ("network_egress_allow", strings(config.network_egress_allow)),
        ("network_egress_deny", strings(config.network_egress_deny)),
        ("host_loopback", Value::from(config.host_loopback)),
        ("network_proxy", Value::from(config.network_proxy)),
        (
            "host_loopback_forward",
            strings(config.host_loopback_forward),
        ),
        ("cmdline", Value::from(config.cmdline)),
    ]);
    let Value::Dict(members) = &mut value else {
        unreachable!("dict builds a dict")
    };
    if lists_mounts(format) {
        let mut mounts = Vec::new();
        for mount in launch.mounts() {
            mounts.push(serialize_mount(
                mount,
                format != MULTI_MOUNT_CONFIG_FORMAT,
                format == FILE_MOUNT_CONFIG_FORMAT,
            )?);
        }
        members.insert("mounts".to_owned(), Value::List(mounts));
    } else {
        let mount = match launch.mounts().first() {
            Some(mount) => serialize_mount(mount, false, false)?,
            None => Value::Null,
        };
        members.insert("mount".to_owned(), mount);
    }
    Ok(value)
}

/// Returns a share as `_serialize_mount` records it, with its host path made absolute, and with
/// its allowed and writable paths and its kind if the format holds them.
fn serialize_mount(mount: &SandboxMount, policy: bool, kind: bool) -> Result<Value> {
    let absolute = mount.absolute()?;
    let mut value = dict([
        ("guest_target", Value::from(absolute.guest_target())),
        (
            "host_path",
            Value::from(crate::host::cli::sandbox::path_string(
                absolute.host_path(),
            )?),
        ),
        ("access", Value::from(absolute.access().as_str())),
        ("denied_paths", strings(&absolute.policy().denied)),
        ("owner", Value::from(absolute.owner().as_str())),
    ]);
    let Value::Dict(members) = &mut value else {
        unreachable!("dict builds a dict")
    };
    if policy {
        members.insert(
            "allowed_paths".to_owned(),
            strings(&absolute.policy().allowed),
        );
        members.insert(
            "writable_paths".to_owned(),
            strings(&absolute.policy().writable),
        );
    }
    if kind {
        members.insert("kind".to_owned(), Value::from(absolute.kind().as_str()));
    }
    Ok(value)
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
        IntError::Overflow => {
            Malformed::Exit(Exit::error("cannot convert float infinity to integer"))
        }
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

/// Returns the paths of a share's `key`, which must be a list, as `_deserialize_paths` reads
/// them.
fn paths_member(
    document: &Document,
    mount: NodeId,
    key: &str,
) -> std::result::Result<Vec<String>, Malformed> {
    let value = member(document, mount, key).ok_or(Malformed::Malformed)?;
    let Node::Array(items) = document.node(value) else {
        return Err(Malformed::Malformed);
    };
    items
        .iter()
        .map(|&item| py_str(document, item).ok_or(Malformed::Malformed))
        .collect()
}

/// Reads one recorded share, as `_deserialize_mount` does, and checks it as
/// `SandboxMount.__post_init__` does.
fn deserialize_mount(
    document: &Document,
    id: NodeId,
    policy: bool,
    kind: bool,
) -> std::result::Result<SandboxMount, Malformed> {
    if !matches!(document.node(id), Node::Object(_)) {
        return Err(Malformed::Malformed);
    }
    let guest_target = text_member(document, id, "guest_target")?;
    let host_path = text_member(document, id, "host_path")?;
    let access = text_member(document, id, "access")?;
    let denied = paths_member(document, id, "denied_paths")?;
    // Configurations written before ownership modes ran shares as the VMM.
    let owner = match member(document, id, "owner") {
        Some(owner) => py_str(document, owner).ok_or(Malformed::Malformed)?,
        None => "vmm".to_owned(),
    };
    let allowed = if policy {
        paths_member(document, id, "allowed_paths")?
    } else {
        Vec::new()
    };
    let writable = if policy {
        paths_member(document, id, "writable_paths")?
    } else {
        Vec::new()
    };
    let kind = if kind {
        text_member(document, id, "kind")?
    } else {
        "directory".to_owned()
    };
    validate_mount_target(&guest_target)?;
    let access = MountAccess::from_name(&access)?;
    let owner = MountOwner::from_name(&owner)?;
    let kind = MountKind::from_name(&kind)?;
    let policy = MountPolicy {
        denied,
        allowed,
        writable,
    };
    Ok(SandboxMount::new(
        &guest_target,
        &host_path,
        access,
        policy,
        owner,
        kind,
    )?)
}

/// Reads the shares of a recorded configuration, as `_deserialize_mounts` does.
fn deserialize_mounts(
    document: &Document,
    root: NodeId,
) -> std::result::Result<Vec<SandboxMount>, Malformed> {
    let format = member(document, root, "format");
    let is_format = |expected| format.is_some_and(|format| equals_int(document, format, expected));
    let mounts_format = [
        MULTI_MOUNT_CONFIG_FORMAT,
        POLICY_CONFIG_FORMAT,
        FILE_MOUNT_CONFIG_FORMAT,
    ]
    .into_iter()
    .find(|&format| is_format(format));
    if let Some(format) = mounts_format {
        let mounts = member(document, root, "mounts").ok_or(Malformed::Malformed)?;
        let Node::Array(items) = document.node(mounts) else {
            return Err(Malformed::Malformed);
        };
        return items
            .iter()
            .map(|&item| {
                deserialize_mount(
                    document,
                    item,
                    format != MULTI_MOUNT_CONFIG_FORMAT,
                    format == FILE_MOUNT_CONFIG_FORMAT,
                )
            })
            .collect();
    }
    match member(document, root, "mount") {
        None => Ok(Vec::new()),
        Some(mount) if matches!(document.node(mount), Node::Null) => Ok(Vec::new()),
        Some(mount) => Ok(vec![deserialize_mount(document, mount, false, false)?]),
    }
}

/// Reads the launch that a configuration records, as `_deserialize_launch` does, and checks it
/// against the host.
///
/// A missing field, or a value of the wrong type, fails as `sandbox configuration is malformed`,
/// as does a layer whose role is none of `distro`, `runtime`, and `custom`, which Python would
/// accept and then leave out of the launch. A configuration whose format is not the one that its
/// shares need fails as `sandbox configuration format does not match its mounts`.
pub(crate) fn deserialize_launch(document: &Document) -> Result<SandboxLaunch> {
    let malformed = || Exit::error("sandbox configuration is malformed");
    let launch = (|| -> std::result::Result<SandboxLaunch, Malformed> {
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
        let mounts = deserialize_mounts(document, root)?;
        let launch = SandboxLaunch::new(
            layers,
            &scratch,
            SandboxSettings {
                hostname,
                workload_identity,
                memory_max,
                pids_max,
                mounts,
                ..SandboxSettings::default()
            },
        )?;
        Ok(launch)
    })()
    .map_err(|error| match error {
        Malformed::Malformed => malformed(),
        Malformed::Exit(exit) => exit,
    })?;
    let format = member(document, document.root(), "format");
    if !format.is_some_and(|format| equals_int(document, format, config_format(launch.mounts()))) {
        return Err(Exit::error(
            "sandbox configuration format does not match its mounts",
        ));
    }
    launch.validated()?;
    Ok(launch)
}

/// Returns `str(value)` of the member `name` of the configuration, failing as Python's
/// `KeyError` reads, `'NAME'`, if the member is missing.
pub(crate) fn config_text(document: &Document, name: &str) -> Result<String> {
    let value = member(document, document.root(), name).ok_or_else(|| Exit::error(repr(name)))?;
    py_str(document, value).ok_or_else(|| Exit::error("sandbox configuration is malformed"))
}

/// Returns `int(config[name])`, failing with the `KeyError`, `TypeError`, `ValueError`, or
/// `OverflowError` that Python raises.
pub(crate) fn config_int(document: &Document, name: &str) -> Result<Int> {
    let value = member(document, document.root(), name).ok_or_else(|| Exit::error(repr(name)))?;
    py_int(document, value).map_err(|error| match error {
        IntError::Overflow => Exit::error("cannot convert float infinity to integer"),
        IntError::Invalid => Exit::error(int_error(document, value)),
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
