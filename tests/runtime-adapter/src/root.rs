//! The adapter's messages about a guest's layered root: the host's root configuration, the
//! guest's readiness, the host's mapping table, and the failures of the root's stages and of the
//! table that the guest reports.
//!
//! The boot tokens of `nvx::protocol::root` carry what NVX's boot contract defines: the image's
//! format and the identity that the guest expects of it, and the upper. A [`RootConfig`] carries
//! the rest that the root needs, which only the adapter defines, after the host attaches: the
//! runtime directory, the cgroups, their resource values, the sizes that the block devices must
//! have, how long the guest waits for them, the most bytes that the first partition of a GPT disk
//! may hold, how the guest assembles the overlay over the upper, how many mappings the host's
//! mapping table holds, the workload identity of the whole sandbox, if it has one, and whether the
//! sandbox's listeners are published.
//!
//! A root configuration is UTF-8 text: one `NAME=VALUE` line per field, each ended by a newline,
//! in any order, and each at most once. Numbers are decimal, without a sign or leading zeros:
//!
//! | Name | Value | Required |
//! | --- | --- | --- |
//! | `version` | [`CONFIG_VERSION`] | yes |
//! | `runtime` | The runtime directory, below which the guest assembles the root | yes |
//! | `agent-cgroup` | The agent's cgroup, which holds the guest's own processes | yes |
//! | `workload-cgroup` | The workload cgroup, the agent's sibling | yes |
//! | `device-timeout-ms` | How long the guest waits for each block device, at most 600,000 | yes |
//! | `agent-memory-low` | The agent's `memory.low`, in bytes | no |
//! | `workload-memory-max` | The workload cgroup's `memory.max`, in bytes | no |
//! | `workload-pids-max` | The workload cgroup's `pids.max` | no |
//! | `distro-sectors` | The size of the image's block device, in 512-byte sectors | no |
//! | `scratch-sectors` | The size of the scratch block device, in 512-byte sectors | no |
//! | `first-partition-bound` | The most bytes that the first partition of a GPT disk may hold, [`DEFAULT_FIRST_PARTITION_BOUND`] by default | no |
//! | `overlay-mode` | The overlay's [mode](OverlayMode): `trusted-lowers`, the guest's default, or `untrusted-lowers` | no |
//! | `scratch-journal` | Whether an ext4 scratch may have a journal, a [`JournalPolicy`]: `allowed`, the guest's default, or `forbidden` | no |
//! | `mapping-count` | The number of mappings of the host's mapping table, at most [`MAX_HOST_MAPPINGS`] | no |
//! | `workload-uid` | The user ID of the sandbox's workload identity, as which every execution runs | with `workload-gid` |
//! | `workload-gid` | The group ID of the sandbox's workload identity | with `workload-uid` |
//! | `listeners` | Whether the sandbox's listeners are published, [`Listeners`]: `published`, the guest's default, or `unpublished` | no |
//!
//! The paths are absolute and canonical guest paths: not `/`, without an empty, `.`, or `..`
//! component, or a trailing `/`. The runtime directory holds no `,`, `:`, `\`, or `=`, which the
//! overlay's options reserve, and the two cgroups have the same parent. The optional numbers are
//! positive. A workload identity has both IDs, each below 4,294,967,295, which the kernel reads
//! as "unchanged", and no supplementary groups or capabilities; the guest checks its account in
//! the assembled root, a failure of [`RootStage::Identity`], and an execution then names no
//! identity of its own. While the sandbox's listeners are published, through private-network
//! ingress or host loopback, the guest refuses an execution `CAP_NET_BIND_SERVICE` and
//! `CAP_NET_RAW`, with which it could forge the announcements of its listeners.
//! [`RootConfig::decode`] refuses every other configuration, which the guest
//! reports as a failure of [`RootStage::Configuration`].
//!
//! A [`RootFailure`] is a stage's code, one byte, then the problem as UTF-8 text.
//!
//! Once the guest assembled the root, it answers the configuration with its [`Ready`]: the length
//! of the guest agent's build, one byte, then the build, 1 to [`MAX_BUILD_LEN`] printable ASCII
//! characters without a space, such as [`GUEST_BUILD`](crate::GUEST_BUILD). A clone, which a
//! restore of a template's snapshot started, answers the host's Activate with its `Ready` once it
//! activated, which reports its activation after the build: the canonical encoding of
//! `nvx::activation::ReadyActivation`, `1`, then the transaction ID of the restore whose record it
//! applied. A cold boot's `Ready` reports no activation, since a root configuration, not an
//! activation record, configures its root.
//!
//! The host's mapping table travels in parts, each a `MAPS` payload of `nvx::protocol`, as its
//! `encode_maps_payloads` writes it: the index in the table of the part's first mapping, then the
//! number of its mappings, each a little-endian `u32`, then each mapping: its flags, a
//! little-endian `u16` that is 1 for a read-only mapping and 0 otherwise, then the lengths of its
//! source and its target, each a little-endian `u16`, then the source, a path relative to the
//! guest's mount of the host's export, and the target, an absolute path in the root.
//! [`encode_maps`] splits a table into parts of at most [`MAX_BODY`] bytes, and [`decode_maps`]
//! reads a part. The guest answers a part with a [`MapFailure`] if it refused the part or failed
//! to apply one of its mappings: how many of the table's mappings it applied, a little-endian
//! `u32`, then the problem as UTF-8 text.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use nvx::activation::ReadyActivation;
use nvx::protocol::{
    GuestMapsPayload, HostMap, MAX_HOST_MAPPINGS, decode_guest_maps_payload, encode_maps_payloads,
};

use crate::frame::{BodyError, MAX_BODY, MAX_TEXT_BODY};

/// The version of the root configuration that this build writes and reads.
pub const CONFIG_VERSION: u32 = 1;

/// The runtime directory unless the host names another.
pub const DEFAULT_RUNTIME: &str = "/run/runtime-adapter";
/// The agent's cgroup unless the host names another.
pub const DEFAULT_AGENT_CGROUP: &str = "/sys/fs/cgroup/agent";
/// The workload cgroup unless the host names another.
pub const DEFAULT_WORKLOAD_CGROUP: &str = "/sys/fs/cgroup/workload";
/// The agent's `memory.low` unless the host names another: 16 MiB, as NVX's own guest reserves.
pub const DEFAULT_AGENT_MEMORY_LOW: u64 = 16 << 20;
/// The workload cgroup's `memory.max` unless the host names another: 128 MiB.
pub const DEFAULT_WORKLOAD_MEMORY_MAX: u64 = 128 << 20;
/// The workload cgroup's `pids.max` unless the host names another.
pub const DEFAULT_WORKLOAD_PIDS_MAX: u64 = 128;
/// How long the guest waits for each block device unless the host names another: 30 seconds, as
/// NVX's own guest waits.
pub const DEFAULT_DEVICE_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest wait for a block device that a configuration may ask for: ten minutes.
pub const MAX_DEVICE_TIMEOUT: Duration = Duration::from_secs(600);
/// The most bytes that the first partition of a GPT disk may hold unless the host names another
/// bound: 64 KiB.
pub const DEFAULT_FIRST_PARTITION_BOUND: u64 = 64 << 10;

/// The longest guest path of a configuration, in bytes: Linux's `PATH_MAX`, without the NUL.
const MAX_PATH_BYTES: usize = 4095;
/// The characters that a runtime directory may not hold, which the overlay's options reserve.
const RUNTIME_RESERVED: [char; 4] = [',', ':', '\\', '='];

/// The names of a configuration's fields.
const VERSION: &str = "version";
/// See [`VERSION`].
const RUNTIME: &str = "runtime";
/// See [`VERSION`].
const AGENT_CGROUP: &str = "agent-cgroup";
/// See [`VERSION`].
const WORKLOAD_CGROUP: &str = "workload-cgroup";
/// See [`VERSION`].
const DEVICE_TIMEOUT: &str = "device-timeout-ms";
/// See [`VERSION`].
const AGENT_MEMORY_LOW: &str = "agent-memory-low";
/// See [`VERSION`].
const WORKLOAD_MEMORY_MAX: &str = "workload-memory-max";
/// See [`VERSION`].
const WORKLOAD_PIDS_MAX: &str = "workload-pids-max";
/// See [`VERSION`].
const DISTRO_SECTORS: &str = "distro-sectors";
/// See [`VERSION`].
const SCRATCH_SECTORS: &str = "scratch-sectors";
/// See [`VERSION`].
const FIRST_PARTITION_BOUND: &str = "first-partition-bound";
/// See [`VERSION`].
const OVERLAY_MODE: &str = "overlay-mode";
/// See [`VERSION`].
const SCRATCH_JOURNAL: &str = "scratch-journal";
/// See [`VERSION`].
const MAPPING_COUNT: &str = "mapping-count";
/// See [`VERSION`].
const WORKLOAD_UID: &str = "workload-uid";
/// See [`VERSION`].
const WORKLOAD_GID: &str = "workload-gid";
/// See [`VERSION`].
const LISTENERS: &str = "listeners";
/// Every field, in the order in which [`RootConfig::encode`] writes them.
const FIELDS: [&str; 17] = [
    VERSION,
    RUNTIME,
    AGENT_CGROUP,
    WORKLOAD_CGROUP,
    DEVICE_TIMEOUT,
    AGENT_MEMORY_LOW,
    WORKLOAD_MEMORY_MAX,
    WORKLOAD_PIDS_MAX,
    DISTRO_SECTORS,
    SCRATCH_SECTORS,
    FIRST_PARTITION_BOUND,
    OVERLAY_MODE,
    SCRATCH_JOURNAL,
    MAPPING_COUNT,
    WORKLOAD_UID,
    WORKLOAD_GID,
    LISTENERS,
];

/// The overlay mode that a configuration names, which the guest's assembly applies: the
/// `nvx::guest::root::OverlayMode` of the same name, which selects the overlay's one option set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayMode {
    /// `trusted-lowers`: `metacopy=on,xino=on`, for lowers whose overlay xattrs the guest trusts.
    TrustedLowers,
    /// `untrusted-lowers`: `metacopy=off,redirect_dir=nofollow`, for lowers that may carry
    /// untrusted overlay xattrs.
    UntrustedLowers,
}

impl OverlayMode {
    /// Every mode.
    pub const ALL: [Self; 2] = [Self::TrustedLowers, Self::UntrustedLowers];

    /// Returns the mode's name in a configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TrustedLowers => "trusted-lowers",
            Self::UntrustedLowers => "untrusted-lowers",
        }
    }

    /// Returns the mode named `name`, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == name)
    }
}

/// Whether the guest's assembly allows an ext4 scratch a journal, which a configuration names:
/// the `nvx::guest::root::Journal` of the same name, which the assembly checks in the scratch's
/// superblock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalPolicy {
    /// `allowed`: the scratch may have a journal.
    Allowed,
    /// `forbidden`: the assembly refuses a scratch with a journal.
    Forbidden,
}

impl JournalPolicy {
    /// Every policy.
    pub const ALL: [Self; 2] = [Self::Allowed, Self::Forbidden];

    /// Returns the policy's name in a configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Forbidden => "forbidden",
        }
    }

    /// Returns the policy named `name`, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|policy| policy.as_str() == name)
    }
}

/// Whether a sandbox's listeners are published, which a configuration names: the
/// `nvx::guest::entry::Publication` of the same name, with which the guest's executions enter the
/// root.
///
/// The host publishes them through its network: NVX's `/init` starts an announcer of the guest's
/// listeners, and OpenVMM publishes what it announces, to the host's private networks with ingress
/// allowed, or on host loopback with host-loopback access allowed. An execution that may bind a
/// privileged port, as `CAP_NET_BIND_SERVICE` allows, or open a raw socket, as `CAP_NET_RAW`
/// allows, could forge those announcements, so the entry refuses both while listeners are
/// published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listeners {
    /// `published`: the guest's listeners are published, and executions enter without
    /// `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW`.
    Published,
    /// `unpublished`: no listener is published.
    Unpublished,
}

impl Listeners {
    /// Every publication.
    pub const ALL: [Self; 2] = [Self::Published, Self::Unpublished];

    /// Returns the publication's name in a configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Unpublished => "unpublished",
        }
    }

    /// Returns the publication named `name`, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|listeners| listeners.as_str() == name)
    }
}

/// The host's configuration of a guest's layered root: everything that the root needs besides its
/// boot tokens, as the [module documentation](self) lists it.
///
/// The guest paths are text, since the host may run on another operating system than the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootConfig {
    /// The runtime directory, below which the guest assembles the root.
    pub runtime: String,
    /// The agent's cgroup, which holds the guest's own processes.
    pub agent_cgroup: String,
    /// The workload cgroup, the agent's sibling.
    pub workload_cgroup: String,
    /// How long the guest waits for each block device.
    pub device_timeout: Duration,
    /// The agent's `memory.low`, in bytes, if any.
    pub agent_memory_low: Option<u64>,
    /// The workload cgroup's `memory.max`, in bytes, if any.
    pub workload_memory_max: Option<u64>,
    /// The workload cgroup's `pids.max`, if any.
    pub workload_pids_max: Option<u64>,
    /// The size that the image's block device must have, in 512-byte sectors, if any.
    pub distro_sectors: Option<u64>,
    /// The size that the scratch block device must have, in 512-byte sectors, if any.
    pub scratch_sectors: Option<u64>,
    /// The most bytes that the first partition of a GPT disk may hold, unless
    /// [`DEFAULT_FIRST_PARTITION_BOUND`].
    pub first_partition_bound: Option<u64>,
    /// The overlay's mode, unless the guest's default.
    pub overlay_mode: Option<OverlayMode>,
    /// Whether an ext4 scratch may have a journal, unless the guest's default.
    pub scratch_journal: Option<JournalPolicy>,
    /// The number of mappings of the host's mapping table, at most [`MAX_HOST_MAPPINGS`], or 0
    /// for none.
    pub mapping_count: u32,
    /// The workload identity of the whole sandbox, its user ID and group ID, if it has one: every
    /// execution runs as it, and names no identity of its own.
    pub workload_identity: Option<(u32, u32)>,
    /// Whether the sandbox's listeners are published, unless the guest's default,
    /// [`Listeners::Published`], with which every execution enters the root.
    pub listeners: Option<Listeners>,
}

impl Default for RootConfig {
    /// Returns the configuration of the defaults: [`DEFAULT_RUNTIME`], [`DEFAULT_AGENT_CGROUP`],
    /// [`DEFAULT_WORKLOAD_CGROUP`], [`DEFAULT_DEVICE_TIMEOUT`], [`DEFAULT_AGENT_MEMORY_LOW`],
    /// [`DEFAULT_WORKLOAD_MEMORY_MAX`], and [`DEFAULT_WORKLOAD_PIDS_MAX`], with no sizes, the
    /// guest's default bound of the first partition, overlay mode, and journal policy, no mapping
    /// or workload identity, and the guest's default publication of listeners.
    fn default() -> Self {
        Self {
            runtime: DEFAULT_RUNTIME.to_owned(),
            agent_cgroup: DEFAULT_AGENT_CGROUP.to_owned(),
            workload_cgroup: DEFAULT_WORKLOAD_CGROUP.to_owned(),
            device_timeout: DEFAULT_DEVICE_TIMEOUT,
            agent_memory_low: Some(DEFAULT_AGENT_MEMORY_LOW),
            workload_memory_max: Some(DEFAULT_WORKLOAD_MEMORY_MAX),
            workload_pids_max: Some(DEFAULT_WORKLOAD_PIDS_MAX),
            distro_sectors: None,
            scratch_sectors: None,
            first_partition_bound: None,
            overlay_mode: None,
            scratch_journal: None,
            mapping_count: 0,
            workload_identity: None,
            listeners: None,
        }
    }
}

impl RootConfig {
    /// Returns the configuration's fields, as `NAME` and `VALUE`, in the order of the
    /// [module documentation](self), without the optional fields that it lacks, a mapping count of
    /// 0 among them.
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let mut fields = vec![
            (VERSION, CONFIG_VERSION.to_string()),
            (RUNTIME, self.runtime.clone()),
            (AGENT_CGROUP, self.agent_cgroup.clone()),
            (WORKLOAD_CGROUP, self.workload_cgroup.clone()),
            (DEVICE_TIMEOUT, self.device_timeout.as_millis().to_string()),
        ];
        let optional = [
            (AGENT_MEMORY_LOW, self.agent_memory_low),
            (WORKLOAD_MEMORY_MAX, self.workload_memory_max),
            (WORKLOAD_PIDS_MAX, self.workload_pids_max),
            (DISTRO_SECTORS, self.distro_sectors),
            (SCRATCH_SECTORS, self.scratch_sectors),
            (FIRST_PARTITION_BOUND, self.first_partition_bound),
        ];
        fields.extend(
            optional
                .into_iter()
                .filter_map(|(name, value)| Some((name, value?.to_string()))),
        );
        let named = [
            (OVERLAY_MODE, self.overlay_mode.map(OverlayMode::as_str)),
            (
                SCRATCH_JOURNAL,
                self.scratch_journal.map(JournalPolicy::as_str),
            ),
        ];
        fields.extend(
            named
                .into_iter()
                .filter_map(|(name, value)| Some((name, value?.to_owned()))),
        );
        if self.mapping_count > 0 {
            fields.push((MAPPING_COUNT, self.mapping_count.to_string()));
        }
        if let Some((uid, gid)) = self.workload_identity {
            fields.extend([
                (WORKLOAD_UID, uid.to_string()),
                (WORKLOAD_GID, gid.to_string()),
            ]);
        }
        if let Some(listeners) = self.listeners {
            fields.push((LISTENERS, listeners.as_str().to_owned()));
        }
        fields
    }

    /// Returns the configuration's encoding, its [fields](Self::fields) as `NAME=VALUE` lines,
    /// as the host sends it, whether or not the guest accepts it.
    pub fn encode(&self) -> Vec<u8> {
        self.fields()
            .into_iter()
            .flat_map(|(name, value)| format!("{name}={value}\n").into_bytes())
            .collect()
    }

    /// Reads a configuration from its encoding, and checks it as the
    /// [module documentation](self) describes.
    ///
    /// Fails with a [`BodyError`] that names the first field that breaks the rules.
    pub fn decode(body: &[u8]) -> Result<Self, BodyError> {
        let refused = |problem: String| BodyError::new(format!("the root configuration {problem}"));
        let text = std::str::from_utf8(body).map_err(|_| refused("is not UTF-8 text".into()))?;
        let mut values = BTreeMap::new();
        if !text.is_empty() {
            let lines = text
                .strip_suffix('\n')
                .ok_or_else(|| refused("does not end its last line with a newline".into()))?;
            for line in lines.split('\n') {
                let (name, value) = line
                    .split_once('=')
                    .ok_or_else(|| refused(format!("has a line without a value: {line:?}")))?;
                if !FIELDS.contains(&name) {
                    return Err(refused(format!("names the unknown field {name:?}")));
                }
                if values.insert(name, value).is_some() {
                    return Err(refused(format!("repeats the field {name}")));
                }
            }
        }
        let required = |name: &str| {
            values
                .get(name)
                .copied()
                .ok_or_else(|| refused(format!("lacks the field {name}")))
        };
        let version = required(VERSION)?;
        if version != CONFIG_VERSION.to_string() {
            return Err(refused(format!(
                "has version {version:?}, but this guest reads version {CONFIG_VERSION}"
            )));
        }
        let path = |name: &str| {
            let path = required(name)?;
            match path_problem(path) {
                None => Ok(path.to_owned()),
                Some(problem) => Err(refused(format!(
                    "names the {name} {path:?}, which {problem}"
                ))),
            }
        };
        let runtime = path(RUNTIME)?;
        if runtime.contains(RUNTIME_RESERVED) {
            return Err(refused(format!(
                "names a runtime directory with a `,`, `:`, `\\`, or `=`, which the overlay's \
                 options reserve: {runtime:?}"
            )));
        }
        let agent_cgroup = path(AGENT_CGROUP)?;
        let workload_cgroup = path(WORKLOAD_CGROUP)?;
        if agent_cgroup == workload_cgroup || parent(&agent_cgroup) != parent(&workload_cgroup) {
            return Err(refused(format!(
                "names cgroups that are not siblings: {agent_cgroup:?} and {workload_cgroup:?}"
            )));
        }
        // The guest mounts the cgroup hierarchy at the cgroups' parent unless it is a cgroup.
        if parent(&agent_cgroup).is_empty() {
            return Err(refused(format!(
                "names cgroups in the root directory: {agent_cgroup:?} and {workload_cgroup:?}"
            )));
        }
        let number = |name: &str| -> Result<Option<u64>, BodyError> {
            let Some(text) = values.get(name) else {
                return Ok(None);
            };
            match decimal(text) {
                Some(value) if value > 0 => Ok(Some(value)),
                _ => Err(refused(format!(
                    "has a {name} that is not a positive decimal number: {text:?}"
                ))),
            }
        };
        let timeout = number(DEVICE_TIMEOUT)?
            .ok_or_else(|| refused(format!("lacks the field {DEVICE_TIMEOUT}")))?;
        let device_timeout = Duration::from_millis(timeout);
        if device_timeout > MAX_DEVICE_TIMEOUT {
            return Err(refused(format!(
                "asks to wait {timeout} ms for each block device, longer than {} ms",
                MAX_DEVICE_TIMEOUT.as_millis()
            )));
        }
        let unknown =
            |name: &str, text: &str| refused(format!("names the unknown {name} {text:?}"));
        let overlay_mode = values
            .get(OVERLAY_MODE)
            .map(|text| OverlayMode::from_name(text).ok_or_else(|| unknown(OVERLAY_MODE, text)))
            .transpose()?;
        let scratch_journal = values
            .get(SCRATCH_JOURNAL)
            .map(|text| {
                JournalPolicy::from_name(text).ok_or_else(|| unknown(SCRATCH_JOURNAL, text))
            })
            .transpose()?;
        let mapping_count = number(MAPPING_COUNT)?.unwrap_or(0);
        if mapping_count > MAX_HOST_MAPPINGS as u64 {
            return Err(refused(format!(
                "announces {mapping_count} mappings, more than {MAX_HOST_MAPPINGS}"
            )));
        }
        let id = |name: &str| -> Result<Option<u32>, BodyError> {
            let Some(value) = number(name)? else {
                return Ok(None);
            };
            match u32::try_from(value) {
                Ok(id) if id < u32::MAX => Ok(Some(id)),
                _ => Err(refused(format!(
                    "has a {name} above 4294967294, which no identity has: {value}"
                ))),
            }
        };
        let workload_identity = match (id(WORKLOAD_UID)?, id(WORKLOAD_GID)?) {
            (Some(uid), Some(gid)) => Some((uid, gid)),
            (None, None) => None,
            _ => {
                return Err(refused(format!(
                    "names a workload identity without both {WORKLOAD_UID} and {WORKLOAD_GID}"
                )));
            }
        };
        let listeners = values
            .get(LISTENERS)
            .map(|text| Listeners::from_name(text).ok_or_else(|| unknown(LISTENERS, text)))
            .transpose()?;
        Ok(Self {
            runtime,
            agent_cgroup,
            workload_cgroup,
            device_timeout,
            agent_memory_low: number(AGENT_MEMORY_LOW)?,
            workload_memory_max: number(WORKLOAD_MEMORY_MAX)?,
            workload_pids_max: number(WORKLOAD_PIDS_MAX)?,
            distro_sectors: number(DISTRO_SECTORS)?,
            scratch_sectors: number(SCRATCH_SECTORS)?,
            first_partition_bound: number(FIRST_PARTITION_BOUND)?,
            overlay_mode,
            scratch_journal,
            // The count is at most `MAX_HOST_MAPPINGS`, so it fits a `u32`.
            mapping_count: mapping_count as u32,
            workload_identity,
            listeners,
        })
    }
}

/// Describes why `path` is not an absolute and canonical guest path, if it is not: the rest of a
/// sentence whose subject names the path.
pub(crate) fn path_problem(path: &str) -> Option<&'static str> {
    if path.len() > MAX_PATH_BYTES {
        return Some("is longer than 4095 bytes");
    }
    if path.contains(['\0', '\n']) {
        return Some("holds a NUL or a newline");
    }
    let Some(relative) = path.strip_prefix('/') else {
        return Some("is not absolute");
    };
    if relative
        .split('/')
        .any(|component| matches!(component, "" | "." | ".."))
    {
        return Some("is not canonical, or is the root");
    }
    None
}

/// Returns the parent of the canonical absolute path `path`.
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Parses decimal digits without a sign or leading zeros, as a `u64`.
fn decimal(text: &str) -> Option<u64> {
    if text.is_empty()
        || !text.bytes().all(|byte| byte.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return None;
    }
    text.parse().ok()
}

/// The status with which the guest's worker, and with it the VM, ends once a stage of its root
/// failed: this base plus the stage's [code](RootStage::code).
pub const FAILURE_STATUS_BASE: u8 = 10;

/// A stage of a guest's layered root, which a [`RootFailure`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RootStage {
    /// Receiving and checking the root configuration.
    Configuration,
    /// Reading the boot layout of the root from the kernel command line, and checking that the
    /// guest can assemble it.
    Layout,
    /// Bootstrapping the guest for the root: its cgroups, and user namespaces.
    Bootstrap,
    /// Assembling the root.
    Assembly,
    /// Tearing the root down.
    Teardown,
    /// Checking the account of the sandbox's workload identity in the assembled root, in
    /// existing-account mode, once the root is assembled.
    Identity,
}

impl RootStage {
    /// Every stage, in the order of their codes.
    pub const ALL: [Self; 6] = [
        Self::Configuration,
        Self::Layout,
        Self::Bootstrap,
        Self::Assembly,
        Self::Teardown,
        Self::Identity,
    ];

    /// Returns the stage's code in a [`RootFailure`]'s body: its position in [`ALL`](Self::ALL),
    /// from 1.
    pub const fn code(self) -> u8 {
        match self {
            Self::Configuration => 1,
            Self::Layout => 2,
            Self::Bootstrap => 3,
            Self::Assembly => 4,
            Self::Teardown => 5,
            Self::Identity => 6,
        }
    }

    /// Returns the stage that `code` numbers, if any.
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|stage| stage.code() == code)
    }

    /// Returns the stage's name: `configuration`, `layout`, `bootstrap`, `assembly`,
    /// `teardown`, or `identity`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Layout => "layout",
            Self::Bootstrap => "bootstrap",
            Self::Assembly => "assembly",
            Self::Teardown => "teardown",
            Self::Identity => "identity",
        }
    }

    /// Returns the stage named `name`, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|stage| stage.as_str() == name)
    }

    /// Returns the status with which the guest's worker, and with it the VM, ends once this stage
    /// failed: [`FAILURE_STATUS_BASE`] plus its code, from 11 for the configuration to 16 for
    /// the identity.
    pub const fn exit_status(self) -> u8 {
        FAILURE_STATUS_BASE + self.code()
    }
}

impl fmt::Display for RootStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The most bytes of a [`RootFailure`]'s problem: what its body holds after the stage's code.
pub const MAX_PROBLEM_LEN: usize = MAX_TEXT_BODY - 1;

/// A failure of a stage of a guest's layered root, which the guest reports to the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootFailure {
    /// The stage that failed.
    stage: RootStage,
    /// What failed.
    problem: String,
}

impl RootFailure {
    /// Returns the failure of `stage` that `problem` describes, which is cut to its first
    /// [`MAX_PROBLEM_LEN`] bytes, at a character's boundary, if it is longer.
    pub fn new(stage: RootStage, problem: impl Into<String>) -> Self {
        let mut problem = problem.into();
        let length = crate::frame::truncated(&problem, MAX_PROBLEM_LEN).len();
        problem.truncate(length);
        Self { stage, problem }
    }

    /// Returns the stage that failed.
    pub fn stage(&self) -> RootStage {
        self.stage
    }

    /// Returns what failed.
    pub fn problem(&self) -> &str {
        &self.problem
    }

    /// Returns the failure's encoding: the stage's code, then the problem.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = vec![self.stage.code()];
        body.extend_from_slice(self.problem.as_bytes());
        body
    }

    /// Reads a failure from its encoding.
    ///
    /// Fails with a [`BodyError`] for an empty body, an unknown stage, or a problem that is not
    /// UTF-8 text.
    pub fn decode(body: &[u8]) -> Result<Self, BodyError> {
        let Some((&code, problem)) = body.split_first() else {
            return Err(BodyError::new("a root's failure names no stage"));
        };
        let stage = RootStage::from_code(code).ok_or_else(|| {
            BodyError::new(format!("a root's failure names the unknown stage {code}"))
        })?;
        let problem = std::str::from_utf8(problem)
            .map_err(|_| BodyError::new("a root's failure is not UTF-8 text"))?;
        Ok(Self::new(stage, problem))
    }
}

impl fmt::Display for RootFailure {
    /// Writes `the STAGE of the root failed: PROBLEM`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the {} of the root failed: {}",
            self.stage, self.problem
        )
    }
}

impl std::error::Error for RootFailure {}

/// How the guest answered a root configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootReply {
    /// The guest assembled the root, and reported its readiness.
    Ready(Ready),
    /// A stage of the root failed, and the guest ends.
    Failed(RootFailure),
}

/// The most bytes of the guest agent's build in a [`Ready`]: as many as the byte before it counts.
pub const MAX_BUILD_LEN: usize = u8::MAX as usize;

/// The guest's readiness, with which it answers a root configuration once it assembled the root,
/// or a clone's Activate once it activated the clone: the guest agent's build, which the host can
/// check against the build that it ships, and, for a clone, what the guest reports about its
/// activation, which the host can check against the record that it sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    /// The guest agent's build.
    build: String,
    /// What the guest reports about its activation, if anything: a clone's, but not a cold
    /// boot's, whose root a root configuration configures.
    activation: Option<ReadyActivation>,
}

impl Ready {
    /// Returns the readiness of the guest agent of the build `build`, such as
    /// [`GUEST_BUILD`](crate::GUEST_BUILD), which reports no activation.
    ///
    /// Fails with a [`BodyError`] unless the build holds 1 to [`MAX_BUILD_LEN`] printable ASCII
    /// characters, without a space.
    pub fn new(build: impl Into<String>) -> Result<Self, BodyError> {
        let build = build.into();
        if !valid_build(build.as_bytes()) {
            return Err(BodyError::new(format!(
                "the guest agent's build {build:?} does not hold 1 to {MAX_BUILD_LEN} printable \
                 ASCII characters without a space"
            )));
        }
        Ok(Self {
            build,
            activation: None,
        })
    }

    /// Returns the readiness, which reports `activation`: what `nvx::guest::clone::Activation`
    /// returns once it completed a clone's activation, that the activation applied, with the
    /// restore's transaction ID.
    pub fn with_activation(self, activation: ReadyActivation) -> Self {
        Self {
            activation: Some(activation),
            ..self
        }
    }

    /// Returns the guest agent's build.
    pub fn build(&self) -> &str {
        &self.build
    }

    /// Returns what the guest reports about its activation, if anything.
    pub fn activation(&self) -> Option<ReadyActivation> {
        self.activation
    }

    /// Returns the readiness's encoding: the build's length, one byte, then the build, then the
    /// canonical encoding of the activation that it reports, if any, which
    /// `ReadyActivation::encode` writes.
    pub fn encode(&self) -> Vec<u8> {
        let activation = self.activation.as_ref().map(ReadyActivation::encode);
        let reported = activation.as_ref().map_or(0, Vec::len);
        let mut body = Vec::with_capacity(1 + self.build.len() + reported);
        // The build has at most `MAX_BUILD_LEN` bytes, so its length fits the byte.
        body.push(self.build.len() as u8);
        body.extend_from_slice(self.build.as_bytes());
        body.extend(activation.into_iter().flatten());
        body
    }

    /// Reads a readiness from its encoding, which reports an activation if it has bytes after its
    /// build.
    ///
    /// Fails with a [`BodyError`] for a body that ends before its build does, for a build that
    /// [`new`](Self::new) refuses, and for bytes after the build that `ReadyActivation::decode`
    /// refuses: anything but `0`, or `1` followed by a transaction ID.
    pub fn decode(body: &[u8]) -> Result<Self, BodyError> {
        let refused = |problem: String| BodyError::new(format!("the guest's readiness {problem}"));
        let Some((&length, rest)) = body.split_first() else {
            return Err(refused("names no build".to_owned()));
        };
        let Some((build, reported)) = rest.split_at_checked(usize::from(length)) else {
            return Err(refused("ends early".to_owned()));
        };
        if !valid_build(build) {
            return Err(refused(format!(
                "names a build that does not hold 1 to {MAX_BUILD_LEN} printable ASCII \
                 characters without a space: {:?}",
                String::from_utf8_lossy(build)
            )));
        }
        let activation = match reported.len() {
            0 => None,
            count => Some(ReadyActivation::decode(reported).map_err(|error| {
                let (bytes, verb) = if count == 1 {
                    ("byte", "is")
                } else {
                    ("bytes", "are")
                };
                refused(format!(
                    "has {count} {bytes} after its build, which {verb} not an activation: \
                     {error}"
                ))
            })?),
        };
        Ok(Self {
            build: build.iter().copied().map(char::from).collect(),
            activation,
        })
    }
}

/// Returns whether `build` holds 1 to [`MAX_BUILD_LEN`] printable ASCII characters, without a
/// space, as the build of a guest agent does.
fn valid_build(build: &[u8]) -> bool {
    (1..=MAX_BUILD_LEN).contains(&build.len()) && build.iter().all(u8::is_ascii_graphic)
}

/// Returns the bodies of the `Maps` messages that carry the host's mapping table `table`, in
/// order: none for an empty table, and otherwise the parts into which
/// `nvx::protocol::encode_maps_payloads` splits it, each of at most [`MAX_BODY`] bytes.
///
/// Fails with a [`BodyError`] for a table that `encode_maps_payloads` refuses: one of more than
/// [`MAX_HOST_MAPPINGS`] mappings, or one with a source or a target that the guest refuses.
pub fn encode_maps(table: &[HostMap]) -> Result<Vec<Vec<u8>>, BodyError> {
    if table.is_empty() {
        return Ok(Vec::new());
    }
    encode_maps_payloads(table, MAX_BODY)
        .map_err(|error| BodyError::new(format!("the mapping table does not encode: {error}")))
}

/// Reads a part of the host's mapping table from the body of a `Maps`, with
/// `nvx::protocol::decode_guest_maps_payload`, which takes its sources and targets as bytes, as
/// Linux takes paths.
///
/// Fails with a [`BodyError`] for a payload that `decode_guest_maps_payload` refuses, such as
/// one that ends early, or that holds a source or a target that the guest refuses, and for a part
/// without a mapping, or one whose mappings would end past the largest table, of
/// [`MAX_HOST_MAPPINGS`] mappings. Whether the part continues the table, and fits the count that
/// the root configuration announced, is for the guest to check.
pub fn decode_maps(body: &[u8]) -> Result<GuestMapsPayload, BodyError> {
    let refused =
        |problem: String| BodyError::new(format!("a part of the mapping table {problem}"));
    let part = decode_guest_maps_payload(body)
        .map_err(|error| refused(format!("does not decode: {error}")))?;
    if part.maps.is_empty() {
        return Err(refused("holds no mapping".to_owned()));
    }
    let end = u64::from(part.first) + part.maps.len() as u64;
    if end > MAX_HOST_MAPPINGS as u64 {
        return Err(refused(format!(
            "ends the table at {end} mappings, more than {MAX_HOST_MAPPINGS}"
        )));
    }
    Ok(part)
}

/// The most bytes of a [`MapFailure`]'s problem: what its body holds after the count of applied
/// mappings.
pub const MAX_MAP_PROBLEM_LEN: usize = MAX_TEXT_BODY - 4;

/// A failure of the host's mapping table, which the guest reports to the host: how many of the
/// table's mappings it applied, and the problem.
///
/// The guest applies the table's mappings in order, and keeps those before the first that it
/// refuses or fails to apply, so that the count is the index in the table of the mapping that
/// failed. A part that the guest refuses as a whole, such as one that does not decode, fails at
/// the mapping that the guest expected next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapFailure {
    /// How many of the table's mappings the guest applied.
    applied: u32,
    /// What failed.
    problem: String,
}

impl MapFailure {
    /// Returns the failure, once the guest applied `applied` of the table's mappings, that
    /// `problem` describes, which is cut to its first [`MAX_MAP_PROBLEM_LEN`] bytes, at a
    /// character's boundary, if it is longer.
    pub fn new(applied: u32, problem: impl Into<String>) -> Self {
        let mut problem = problem.into();
        let length = crate::frame::truncated(&problem, MAX_MAP_PROBLEM_LEN).len();
        problem.truncate(length);
        Self { applied, problem }
    }

    /// Returns how many of the table's mappings the guest applied: the index in the table of the
    /// mapping that failed.
    pub fn applied(&self) -> u32 {
        self.applied
    }

    /// Returns what failed.
    pub fn problem(&self) -> &str {
        &self.problem
    }

    /// Returns the failure's encoding: how many mappings the guest applied, a little-endian `u32`,
    /// then the problem.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = self.applied.to_le_bytes().to_vec();
        body.extend_from_slice(self.problem.as_bytes());
        body
    }

    /// Reads a failure from its encoding.
    ///
    /// Fails with a [`BodyError`] for a body that ends before its count does, or whose problem is
    /// not UTF-8 text.
    pub fn decode(body: &[u8]) -> Result<Self, BodyError> {
        let Some((applied, problem)) = body.split_first_chunk::<4>() else {
            return Err(BodyError::new("a failure of the mapping table ends early"));
        };
        let problem = std::str::from_utf8(problem)
            .map_err(|_| BodyError::new("a failure of the mapping table is not UTF-8 text"))?;
        Ok(Self::new(u32::from_le_bytes(*applied), problem))
    }
}

impl fmt::Display for MapFailure {
    /// Writes `the mapping table failed after N mappings: PROBLEM`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plural = if self.applied == 1 { "" } else { "s" };
        write!(
            formatter,
            "the mapping table failed after {} mapping{plural}: {}",
            self.applied, self.problem
        )
    }
}

impl std::error::Error for MapFailure {}

#[cfg(test)]
mod tests;
