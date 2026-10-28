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
mod tests {
    use nvx::activation::TransactionId;
    use nvx::protocol::{GuestHostMap, MAP_READ_ONLY, MAX_MAP_PATH_BYTES};

    use super::*;
    use crate::frame::Message;

    /// Returns a configuration with every field.
    fn full() -> RootConfig {
        RootConfig {
            distro_sectors: Some(116_960),
            scratch_sectors: Some(131_072),
            first_partition_bound: Some(4096),
            overlay_mode: Some(OverlayMode::UntrustedLowers),
            scratch_journal: Some(JournalPolicy::Forbidden),
            mapping_count: 3,
            workload_identity: Some((1000, 100)),
            listeners: Some(Listeners::Unpublished),
            ..RootConfig::default()
        }
    }

    #[test]
    fn a_configuration_is_one_line_per_field() {
        assert_eq!(
            String::from_utf8(full().encode()).unwrap(),
            "version=1\n\
             runtime=/run/runtime-adapter\n\
             agent-cgroup=/sys/fs/cgroup/agent\n\
             workload-cgroup=/sys/fs/cgroup/workload\n\
             device-timeout-ms=30000\n\
             agent-memory-low=16777216\n\
             workload-memory-max=134217728\n\
             workload-pids-max=128\n\
             distro-sectors=116960\n\
             scratch-sectors=131072\n\
             first-partition-bound=4096\n\
             overlay-mode=untrusted-lowers\n\
             scratch-journal=forbidden\n\
             mapping-count=3\n\
             workload-uid=1000\n\
             workload-gid=100\n\
             listeners=unpublished\n"
        );
        // Without a mapping, the configuration names no count.
        let unmapped = String::from_utf8(RootConfig::default().encode()).unwrap();
        assert!(!unmapped.contains("mapping-count"), "{unmapped}");
        // Each mode, policy, and publication has its own name.
        for mode in OverlayMode::ALL {
            assert_eq!(OverlayMode::from_name(mode.as_str()), Some(mode));
        }
        for policy in JournalPolicy::ALL {
            assert_eq!(JournalPolicy::from_name(policy.as_str()), Some(policy));
        }
        for listeners in Listeners::ALL {
            assert_eq!(Listeners::from_name(listeners.as_str()), Some(listeners));
        }
        let minimal = RootConfig {
            agent_memory_low: None,
            workload_memory_max: None,
            workload_pids_max: None,
            ..RootConfig::default()
        };
        let largest = RootConfig {
            workload_identity: Some((u32::MAX - 1, u32::MAX - 1)),
            ..RootConfig::default()
        };
        let published = RootConfig {
            listeners: Some(Listeners::Published),
            ..RootConfig::default()
        };
        for config in [full(), minimal, largest, published, RootConfig::default()] {
            assert_eq!(RootConfig::decode(&config.encode()), Ok(config));
        }
        // The fields may come in any order.
        let mut lines = String::from_utf8(full().encode())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        lines.reverse();
        let reversed = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        assert_eq!(RootConfig::decode(reversed.as_bytes()), Ok(full()));
    }

    #[test]
    fn a_configuration_that_breaks_the_rules_is_refused() {
        let base = String::from_utf8(RootConfig::default().encode()).unwrap();
        let replaced = |name: &str, value: &str| {
            base.lines()
                .map(|line| match line.split_once('=') {
                    Some((field, _)) if field == name => format!("{name}={value}\n"),
                    _ => format!("{line}\n"),
                })
                .collect::<String>()
        };
        let without = |name: &str| {
            base.lines()
                .filter(|line| !line.starts_with(&format!("{name}=")))
                .map(|line| format!("{line}\n"))
                .collect::<String>()
        };
        let cases = [
            (Vec::from(&b"version=1\xff\n"[..]), "is not UTF-8 text"),
            (
                base.trim_end().as_bytes().to_vec(),
                "does not end its last line",
            ),
            (
                format!("{base}\n").into_bytes(),
                "has a line without a value",
            ),
            (
                format!("{base}color=blue\n").into_bytes(),
                "names the unknown field \"color\"",
            ),
            (
                format!("{base}runtime=/run/other\n").into_bytes(),
                "repeats the field runtime",
            ),
            (Vec::new(), "lacks the field version"),
            (without(RUNTIME).into_bytes(), "lacks the field runtime"),
            (
                without(DEVICE_TIMEOUT).into_bytes(),
                "lacks the field device-timeout-ms",
            ),
            (replaced(VERSION, "2").into_bytes(), "has version \"2\""),
            (
                replaced(RUNTIME, "run/relative").into_bytes(),
                "is not absolute",
            ),
            (
                replaced(RUNTIME, "/").into_bytes(),
                "is not canonical, or is the root",
            ),
            (
                replaced(RUNTIME, "/run//nvx").into_bytes(),
                "is not canonical",
            ),
            (
                replaced(RUNTIME, "/run/./nvx").into_bytes(),
                "is not canonical",
            ),
            (
                replaced(RUNTIME, "/run/../nvx").into_bytes(),
                "is not canonical",
            ),
            (
                replaced(RUNTIME, "/run/nvx/").into_bytes(),
                "is not canonical",
            ),
            (
                replaced(RUNTIME, "/run/a,b").into_bytes(),
                "the overlay's options reserve",
            ),
            (
                replaced(RUNTIME, "/run/a:b").into_bytes(),
                "the overlay's options reserve",
            ),
            (
                replaced(RUNTIME, "/run/a=b").into_bytes(),
                "the overlay's options reserve",
            ),
            (replaced(RUNTIME, "/run/a\0b").into_bytes(), "holds a NUL"),
            (
                replaced(RUNTIME, &format!("/{}", "a".repeat(4095))).into_bytes(),
                "longer than 4095",
            ),
            (
                replaced(AGENT_CGROUP, "agent").into_bytes(),
                "names the agent-cgroup \"agent\", which is not absolute",
            ),
            (
                replaced(WORKLOAD_CGROUP, "/sys/fs/cgroup/agent").into_bytes(),
                "not siblings",
            ),
            (
                replaced(WORKLOAD_CGROUP, "/sys/fs/cgroup/agent/workload").into_bytes(),
                "not siblings",
            ),
            (
                replaced(WORKLOAD_CGROUP, "/sys/fs/workload").into_bytes(),
                "not siblings",
            ),
            (
                replaced(AGENT_CGROUP, "/agent")
                    .replace(
                        "workload-cgroup=/sys/fs/cgroup/workload",
                        "workload-cgroup=/workload",
                    )
                    .into_bytes(),
                "names cgroups in the root directory",
            ),
            (
                replaced(DEVICE_TIMEOUT, "0").into_bytes(),
                "not a positive decimal number",
            ),
            (
                replaced(DEVICE_TIMEOUT, "600001").into_bytes(),
                "longer than 600000 ms",
            ),
            (
                replaced(AGENT_MEMORY_LOW, "-1").into_bytes(),
                "not a positive decimal number",
            ),
            (
                replaced(WORKLOAD_MEMORY_MAX, "0x10").into_bytes(),
                "not a positive decimal number",
            ),
            (
                replaced(WORKLOAD_PIDS_MAX, "010").into_bytes(),
                "not a positive decimal number",
            ),
            (
                replaced(WORKLOAD_PIDS_MAX, "").into_bytes(),
                "not a positive decimal number",
            ),
            (
                format!("{base}distro-sectors=18446744073709551616\n").into_bytes(),
                "not a positive decimal number",
            ),
            (
                format!("{base}scratch-sectors= 1\n").into_bytes(),
                "not a positive decimal number",
            ),
            (
                format!("{base}first-partition-bound=0\n").into_bytes(),
                "has a first-partition-bound that is not a positive decimal number: \"0\"",
            ),
            (
                format!("{base}overlay-mode=TrustedLowers\n").into_bytes(),
                "names the unknown overlay-mode \"TrustedLowers\"",
            ),
            (
                format!("{base}scratch-journal=\n").into_bytes(),
                "names the unknown scratch-journal \"\"",
            ),
            (
                format!("{base}mapping-count=0\n").into_bytes(),
                "has a mapping-count that is not a positive decimal number: \"0\"",
            ),
            (
                format!("{base}mapping-count=03\n").into_bytes(),
                "not a positive decimal number",
            ),
            (
                format!("{base}mapping-count=4097\n").into_bytes(),
                "announces 4097 mappings, more than 4096",
            ),
            (
                format!("{base}mapping-count=18446744073709551615\n").into_bytes(),
                "announces 18446744073709551615 mappings, more than 4096",
            ),
            (
                format!("{base}workload-uid=0\nworkload-gid=1000\n").into_bytes(),
                "has a workload-uid that is not a positive decimal number: \"0\"",
            ),
            (
                format!("{base}workload-uid=1000\nworkload-gid=0\n").into_bytes(),
                "has a workload-gid that is not a positive decimal number: \"0\"",
            ),
            (
                format!("{base}workload-uid=4294967295\nworkload-gid=1000\n").into_bytes(),
                "has a workload-uid above 4294967294, which no identity has: 4294967295",
            ),
            (
                format!("{base}workload-uid=1000\nworkload-gid=4294967296\n").into_bytes(),
                "has a workload-gid above 4294967294",
            ),
            (
                format!("{base}workload-uid=1000\n").into_bytes(),
                "names a workload identity without both workload-uid and workload-gid",
            ),
            (
                format!("{base}workload-gid=1000\n").into_bytes(),
                "names a workload identity without both workload-uid and workload-gid",
            ),
            (
                format!("{base}listeners=Published\n").into_bytes(),
                "names the unknown listeners \"Published\"",
            ),
            (
                format!("{base}listeners=published\nlisteners=unpublished\n").into_bytes(),
                "repeats the field listeners",
            ),
        ];
        for (body, problem) in cases {
            let error = RootConfig::decode(&body).unwrap_err();
            assert!(
                error.problem().starts_with("the root configuration ")
                    && error.problem().contains(problem),
                "{:?}: {error}",
                String::from_utf8_lossy(&body)
            );
        }
        // The longest valid path, and the largest numbers.
        let longest = format!("/{}", "a".repeat(4094));
        let large = format!("{base}distro-sectors={}\n", u64::MAX);
        let most_mappings = format!("{base}mapping-count=4096\n");
        assert_eq!(
            RootConfig::decode(replaced(RUNTIME, &longest).as_bytes())
                .unwrap()
                .runtime,
            longest
        );
        assert_eq!(
            RootConfig::decode(large.as_bytes()).unwrap().distro_sectors,
            Some(u64::MAX)
        );
        let config = RootConfig::decode(most_mappings.as_bytes()).unwrap();
        assert_eq!(config.mapping_count, 4096);
        assert_eq!(config.encode(), most_mappings.into_bytes());
    }

    #[test]
    fn stages_have_codes_names_and_exit_statuses() {
        let expected = [
            (RootStage::Configuration, 1, "configuration", 11),
            (RootStage::Layout, 2, "layout", 12),
            (RootStage::Bootstrap, 3, "bootstrap", 13),
            (RootStage::Assembly, 4, "assembly", 14),
            (RootStage::Teardown, 5, "teardown", 15),
            (RootStage::Identity, 6, "identity", 16),
        ];
        for (stage, code, name, status) in expected {
            assert_eq!(stage.code(), code);
            assert_eq!(RootStage::from_code(code), Some(stage));
            assert_eq!(stage.as_str(), name);
            assert_eq!(stage.to_string(), name);
            assert_eq!(RootStage::from_name(name), Some(stage));
            assert_eq!(stage.exit_status(), status);
        }
        assert_eq!(RootStage::ALL.map(RootStage::code), [1, 2, 3, 4, 5, 6]);
        assert_eq!(RootStage::from_code(0), None);
        assert_eq!(RootStage::from_code(7), None);
        assert_eq!(RootStage::from_name("mapping"), None);
    }

    #[test]
    fn a_failure_is_its_stage_then_its_problem() {
        let failure = RootFailure::new(RootStage::Assembly, "the UUID differs");
        assert_eq!(failure.encode(), b"\x04the UUID differs");
        assert_eq!(RootFailure::decode(&failure.encode()), Ok(failure.clone()));
        assert_eq!(
            failure.to_string(),
            "the assembly of the root failed: the UUID differs"
        );
        let empty = RootFailure::new(RootStage::Teardown, "");
        assert_eq!(RootFailure::decode(&[5]), Ok(empty));

        // A long problem is cut at a character's boundary, so that the body fits its limit.
        let long = "é".repeat(MAX_TEXT_BODY);
        let failure = RootFailure::new(RootStage::Configuration, long);
        assert_eq!(failure.problem().len(), MAX_PROBLEM_LEN - 1);
        assert!(failure.encode().len() <= MAX_TEXT_BODY);
        assert_eq!(RootFailure::decode(&failure.encode()), Ok(failure));

        for (body, problem) in [
            (&b""[..], "names no stage"),
            (b"\x00problem", "unknown stage 0"),
            (b"\x07problem", "unknown stage 7"),
            (b"\x01\xffproblem", "not UTF-8 text"),
        ] {
            let error = RootFailure::decode(body).unwrap_err();
            assert!(error.problem().contains(problem), "{body:?}: {error}");
        }
    }

    #[test]
    fn a_readiness_is_the_length_of_its_build_then_the_build() {
        let ready = Ready::new("0.1.0").unwrap();
        assert_eq!(ready.build(), "0.1.0");
        assert_eq!(ready.activation(), None);
        assert_eq!(ready.encode(), b"\x050.1.0");
        assert_eq!(Ready::decode(&ready.encode()), Ok(ready));

        // The guest agent reports this crate's version. A build holds any printable ASCII
        // character but a space, up to its limit, which fits the framing's.
        let ours = Ready::new(crate::GUEST_BUILD).unwrap();
        assert_eq!(ours.build(), env!("CARGO_PKG_VERSION"));
        let printable = (b'!'..=b'~').map(char::from).collect::<String>();
        let longest = "b".repeat(MAX_BUILD_LEN);
        for build in [
            crate::GUEST_BUILD,
            "x",
            printable.as_str(),
            longest.as_str(),
        ] {
            let ready = Ready::new(build).unwrap();
            let body = ready.encode();
            assert_eq!(usize::from(body[0]), build.len(), "{build}");
            assert_eq!(&body[1..], build.as_bytes(), "{build}");
            assert!(body.len() <= MAX_TEXT_BODY, "{build}");
            assert_eq!(Ready::decode(&body), Ok(ready), "{build}");
        }
    }

    #[test]
    fn a_clone_s_readiness_reports_its_activation_after_the_build() {
        let clone = ReadyActivation {
            transaction: Some(TransactionId::from_bytes([0x7e; TransactionId::LEN])),
        };
        // A cold boot's activation, which the encoding takes too, though the adapter's cold boots
        // report none.
        let cold = ReadyActivation { transaction: None };
        let longest = "b".repeat(MAX_BUILD_LEN);
        for build in ["0.1.0", longest.as_str()] {
            for activation in [clone, cold] {
                let ready = Ready::new(build).unwrap().with_activation(activation);
                assert_eq!(ready.build(), build);
                assert_eq!(ready.activation(), Some(activation));
                let body = ready.encode();
                let mut expected = vec![u8::try_from(build.len()).unwrap()];
                expected.extend_from_slice(build.as_bytes());
                expected.extend(activation.encode());
                assert_eq!(body, expected, "{build}: {activation:?}");
                assert!(body.len() <= MAX_TEXT_BODY, "{build}");
                assert_eq!(Ready::decode(&body), Ok(ready), "{build}: {activation:?}");
            }
        }
        // The clone's: `1`, and the transaction ID, after the build; a cold boot's: `0`.
        let ready = Ready::new("0.1.0").unwrap().with_activation(clone);
        let mut expected = b"\x050.1.0".to_vec();
        expected.push(1);
        expected.extend([0x7e; 16]);
        assert_eq!(ready.encode(), expected);
        assert_eq!(
            ready.encode().len(),
            6 + ReadyActivation::CLONE_LEN,
            "{ready:?}"
        );
        let ready = Ready::new("0.1.0").unwrap().with_activation(cold);
        assert_eq!(ready.encode(), b"\x050.1.0\x00");
    }

    #[test]
    fn a_readiness_that_breaks_the_rules_is_refused() {
        let too_long = "b".repeat(MAX_BUILD_LEN + 1);
        let builds = [
            "",
            "0.1 0",
            "0.1.0\n",
            "0.1.0\0",
            "0.1-é",
            "\u{7f}",
            too_long.as_str(),
        ];
        for build in builds {
            let error = Ready::new(build).unwrap_err();
            assert!(
                error
                    .problem()
                    .contains("does not hold 1 to 255 printable ASCII characters without a space"),
                "{build:?}: {error}"
            );
        }
        let cases: [(&[u8], &str); 11] = [
            (b"", "names no build"),
            (
                b"\x00",
                "names a build that does not hold 1 to 255 printable ASCII characters",
            ),
            (b"\x060.1.0", "ends early"),
            (b"\xff0.1.0", "ends early"),
            (
                b"\x050.1.0\x01",
                "has 1 byte after its build, which is not an activation",
            ),
            (
                b"\x050.1.0\x02",
                "has 1 byte after its build, which is not an activation",
            ),
            (
                b"\x040.1.0",
                "has 1 byte after its build, which is not an activation",
            ),
            (
                b"\x010.1.0",
                "has 4 bytes after its build, which are not an activation",
            ),
            (b"\x050.1 0", "without a space: \"0.1 0\""),
            (b"\x050.1\xff0", "without a space: \"0.1\u{fffd}0\""),
            (b"\x02\xc3\xa9", "without a space: \"\u{e9}\""),
        ];
        for (body, problem) in cases {
            let error = Ready::decode(body).unwrap_err();
            assert!(
                error.problem().starts_with("the guest's readiness ")
                    && error.problem().contains(problem),
                "{body:?}: {error}"
            );
        }
        // Bytes after the build that are neither `0`, nor `1` followed by a transaction ID, which
        // `ReadyActivation::decode` refuses: a cold boot's or a clone's with bytes after it, a
        // clone's whose transaction ID ends early, and another first byte.
        let reported = |rest: &[u8]| {
            let mut body = b"\x050.1.0".to_vec();
            body.extend_from_slice(rest);
            body
        };
        for (body, count) in [
            (reported(&[0, 0]), 2),
            (reported(&[0; 17]), 17),
            (reported(&[1; 18]), 18),
            (reported(&[1; 16]), 16),
            (reported(&[2; 17]), 17),
        ] {
            let error = Ready::decode(&body).unwrap_err();
            assert_eq!(
                error.problem(),
                format!(
                    "the guest's readiness has {count} bytes after its build, which are not an \
                     activation: what Ready reports is neither 0, nor 1 followed by a transaction \
                     ID"
                ),
                "{body:?}"
            );
        }
    }

    /// Returns the host's mapping of `source` at `target`, read-only if `read_only`.
    fn mapping(source: &str, target: &str, read_only: bool) -> HostMap {
        HostMap {
            source: source.to_owned(),
            target: target.to_owned(),
            read_only,
        }
    }

    /// Returns `map` as the guest reads it.
    fn read(map: &HostMap) -> GuestHostMap {
        GuestHostMap {
            source: map.source.as_bytes().to_vec(),
            target: map.target.as_bytes().to_vec(),
            read_only: map.read_only,
        }
    }

    /// Returns a part of a mapping table, whose first mapping has the index `first` in the table,
    /// of `count` mappings, of which it holds `entries`, each its flags, source, and target.
    fn part(first: u32, count: u32, entries: &[(u16, &[u8], &[u8])]) -> Vec<u8> {
        let mut body = [first.to_le_bytes(), count.to_le_bytes()].concat();
        for (flags, source, target) in entries {
            let length = |path: &[u8]| u16::try_from(path.len()).unwrap().to_le_bytes();
            body.extend_from_slice(&flags.to_le_bytes());
            body.extend_from_slice(&length(source));
            body.extend_from_slice(&length(target));
            body.extend_from_slice(source);
            body.extend_from_slice(target);
        }
        body
    }

    /// Checks that `parts` carry `table` in order, each within the framing's limit, and returns
    /// their number.
    fn check_parts(parts: &[Vec<u8>], table: &[HostMap]) -> usize {
        let mut read_back = Vec::new();
        for body in parts {
            assert!(body.len() <= MAX_BODY, "{}", body.len());
            assert!(Message::Maps(body.clone()).encode().is_ok());
            let part = decode_maps(body).unwrap();
            assert_eq!(part.first as usize, read_back.len());
            read_back.extend(part.maps);
        }
        assert_eq!(read_back, table.iter().map(read).collect::<Vec<_>>());
        parts.len()
    }

    #[test]
    fn a_mapping_table_travels_in_parts_that_the_guest_reads_in_order() {
        // An empty table needs no part.
        assert_eq!(encode_maps(&[]), Ok(Vec::new()));

        // A part holds the index of its first mapping and their number, then each mapping's
        // flags, the lengths of its source and target, and both.
        let table = [
            mapping("dir", "/srv/dir", true),
            mapping("dir/inner", "/srv/dir/inner", false),
        ];
        let parts = encode_maps(&table).unwrap();
        let expected = part(
            0,
            2,
            &[
                (MAP_READ_ONLY, b"dir", b"/srv/dir"),
                (0, b"dir/inner", b"/srv/dir/inner"),
            ],
        );
        assert_eq!(parts, [expected]);
        assert_eq!(check_parts(&parts, &table), 1);

        // A table of long paths travels in several parts, which continue the table in order.
        let long = "p".repeat(MAX_MAP_PATH_BYTES - 6);
        let table = (0..100)
            .map(|index| {
                let source = format!("{index:03}/{long}");
                mapping(&source, &format!("/{source}"), index % 3 == 0)
            })
            .collect::<Vec<_>>();
        let parts = encode_maps(&table).unwrap();
        assert!(check_parts(&parts, &table) > 1, "{}", parts.len());

        // So does the largest table.
        let largest = (0..MAX_HOST_MAPPINGS)
            .map(|index| mapping(&format!("s{index}"), &format!("/t/{index}"), false))
            .collect::<Vec<_>>();
        check_parts(&encode_maps(&largest).unwrap(), &largest);
    }

    #[test]
    fn mapping_tables_and_parts_that_break_the_rules_are_refused() {
        // The tables that the host cannot send.
        let too_many = vec![mapping("a", "/a", false); MAX_HOST_MAPPINGS + 1];
        let tables = [
            too_many,
            vec![mapping("/a", "/a", false)],
            vec![mapping("a", "a", false)],
            vec![mapping("a/../b", "/a", false)],
            vec![mapping("a", "/a/./b", false)],
            vec![mapping("a//b", "/a", false)],
            vec![mapping("a", "/a\0b", false)],
            vec![mapping(
                "a",
                &format!("/{}", "a".repeat(MAX_MAP_PATH_BYTES)),
                false,
            )],
        ];
        for table in tables {
            let error = encode_maps(&table).unwrap_err();
            assert!(
                error
                    .problem()
                    .starts_with("the mapping table does not encode: a host mapping"),
                "{error}"
            );
        }

        // The parts that the guest refuses.
        let entry: (u16, &[u8], &[u8]) = (0, b"a", b"/a");
        let cases = [
            (
                Vec::new(),
                "does not decode: the host mapping request is truncated",
            ),
            (vec![0; 7], "is truncated"),
            (part(0, 1, &[]), "is truncated"),
            (
                part(0, 2, &[entry])[..15].to_vec(),
                "does not decode: the host mapping request is truncated",
            ),
            (
                part(0, 1, &[(2, b"a", b"/a")]),
                "does not decode: a host mapping has unknown flags",
            ),
            (part(0, 1, &[(0, b"/a", b"/a")]), "the guest agent refuses"),
            (part(0, 1, &[(0, b"a", b"a")]), "the guest agent refuses"),
            (
                part(0, 1, &[(0, b"a/./b", b"/a")]),
                "the guest agent refuses",
            ),
            (
                part(0, 1, &[(0, b"a", b"/a/..")]),
                "the guest agent refuses",
            ),
            (part(0, 1, &[(0, b"a\0", b"/a")]), "the guest agent refuses"),
            (
                [part(0, 1, &[entry]), vec![0]].concat(),
                "does not decode: the host mapping request has trailing bytes",
            ),
            (
                part(0, 4097, &[]),
                "does not decode: the host mapping request has too many entries",
            ),
            (part(0, 0, &[]), "holds no mapping"),
            (
                part(4096, 1, &[entry]),
                "ends the table at 4097 mappings, more than 4096",
            ),
            (
                part(u32::MAX, 1, &[entry]),
                "ends the table at 4294967296 mappings, more than 4096",
            ),
        ];
        for (body, problem) in cases {
            let error = decode_maps(&body).unwrap_err();
            assert!(
                error.problem().starts_with("a part of the mapping table ")
                    && error.problem().contains(problem),
                "{body:?}: {error}"
            );
        }

        // The last mapping of the largest table, whose paths need not be UTF-8 text, as Linux's
        // need not.
        let last = decode_maps(&part(4095, 1, &[(MAP_READ_ONLY, b"\xff", b"/\xfe")])).unwrap();
        assert_eq!(last.first, 4095);
        let expected = GuestHostMap {
            source: vec![0xff],
            target: b"/\xfe".to_vec(),
            read_only: true,
        };
        assert_eq!(last.maps, [expected]);
    }

    #[test]
    fn a_failure_of_the_mapping_table_is_its_count_then_its_problem() {
        let problem = "failed to mount the mapping at /srv/out: the runtime reserves the target";
        let failure = MapFailure::new(2, problem);
        assert_eq!(failure.applied(), 2);
        assert_eq!(failure.problem(), problem);
        let expected = [2u32.to_le_bytes().as_slice(), problem.as_bytes()].concat();
        assert_eq!(failure.encode(), expected);
        assert_eq!(MapFailure::decode(&failure.encode()), Ok(failure.clone()));
        assert_eq!(
            failure.to_string(),
            format!("the mapping table failed after 2 mappings: {problem}")
        );
        assert_eq!(
            MapFailure::new(1, "refused").to_string(),
            "the mapping table failed after 1 mapping: refused"
        );
        assert_eq!(MapFailure::decode(&[0; 4]), Ok(MapFailure::new(0, "")));
        let last = MapFailure::new(u32::MAX, "x");
        assert_eq!(last.encode(), b"\xff\xff\xff\xffx");
        assert_eq!(MapFailure::decode(&last.encode()), Ok(last));

        // A long problem is cut at a character's boundary, so that the body fits its limit.
        let long = format!("x{}", "é".repeat(MAX_TEXT_BODY));
        let failure = MapFailure::new(4096, long);
        assert_eq!(failure.problem().len(), MAX_MAP_PROBLEM_LEN - 1);
        assert_eq!(failure.encode().len(), MAX_TEXT_BODY - 1);
        assert!(Message::MapFailed(failure.encode()).encode().is_ok());
        assert_eq!(MapFailure::decode(&failure.encode()), Ok(failure));

        for (body, problem) in [
            (&b""[..], "ends early"),
            (b"\x01\x00\x00", "ends early"),
            (b"\x01\x00\x00\x00\xffproblem", "is not UTF-8 text"),
        ] {
            let error = MapFailure::decode(body).unwrap_err();
            assert!(
                error
                    .problem()
                    .starts_with("a failure of the mapping table ")
                    && error.problem().contains(problem),
                "{body:?}: {error}"
            );
        }
    }
}
