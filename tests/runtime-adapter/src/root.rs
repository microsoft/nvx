//! The adapter's messages about a guest's layered root: the host's root configuration, and the
//! stages of the root whose failures the guest reports.
//!
//! The boot tokens of `nvx::protocol::root` carry what NVX's boot contract defines: the image's
//! format and the identity that the guest expects of it, and the upper. A [`RootConfig`] carries
//! the rest that the root needs, which only the adapter defines, after the host attaches: the
//! runtime directory, the cgroups, their resource values, the sizes that the block devices must
//! have, how long the guest waits for them, the most bytes that the first partition of a GPT disk
//! may hold, how the guest assembles the overlay over the upper, and the workload identity of the
//! whole sandbox, if it has one.
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
//! | `workload-uid` | The user ID of the sandbox's workload identity, as which every execution runs | with `workload-gid` |
//! | `workload-gid` | The group ID of the sandbox's workload identity | with `workload-uid` |
//!
//! The paths are absolute and canonical guest paths: not `/`, without an empty, `.`, or `..`
//! component, or a trailing `/`. The runtime directory holds no `,`, `:`, `\`, or `=`, which the
//! overlay's options reserve, and the two cgroups have the same parent. The optional numbers are
//! positive. A workload identity has both IDs, each below 4,294,967,295, which the kernel reads
//! as "unchanged", and no supplementary groups or capabilities; the guest checks its account in
//! the assembled root, a failure of [`RootStage::Identity`], and an execution then names no
//! identity of its own. [`RootConfig::decode`] refuses every other configuration, which the guest
//! reports as a failure of [`RootStage::Configuration`].
//!
//! A [`RootFailure`] is a stage's code, one byte, then the problem as UTF-8 text.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::frame::{BodyError, MAX_TEXT_BODY};

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
const WORKLOAD_UID: &str = "workload-uid";
/// See [`VERSION`].
const WORKLOAD_GID: &str = "workload-gid";
/// Every field, in the order in which [`RootConfig::encode`] writes them.
const FIELDS: [&str; 15] = [
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
    WORKLOAD_UID,
    WORKLOAD_GID,
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
    /// The workload identity of the whole sandbox, its user ID and group ID, if it has one: every
    /// execution runs as it, and names no identity of its own.
    pub workload_identity: Option<(u32, u32)>,
}

impl Default for RootConfig {
    /// Returns the configuration of the defaults: [`DEFAULT_RUNTIME`], [`DEFAULT_AGENT_CGROUP`],
    /// [`DEFAULT_WORKLOAD_CGROUP`], [`DEFAULT_DEVICE_TIMEOUT`], [`DEFAULT_AGENT_MEMORY_LOW`],
    /// [`DEFAULT_WORKLOAD_MEMORY_MAX`], and [`DEFAULT_WORKLOAD_PIDS_MAX`], with no sizes, the
    /// guest's default bound of the first partition, overlay mode, and journal policy, and no
    /// workload identity.
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
            workload_identity: None,
        }
    }
}

impl RootConfig {
    /// Returns the configuration's fields, as `NAME` and `VALUE`, in the order of the
    /// [module documentation](self), without the optional fields that it lacks.
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
        if let Some((uid, gid)) = self.workload_identity {
            fields.extend([
                (WORKLOAD_UID, uid.to_string()),
                (WORKLOAD_GID, gid.to_string()),
            ]);
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
            workload_identity,
        })
    }
}

/// Describes why `path` is not an absolute and canonical guest path, if it is not: the rest of a
/// sentence whose subject names the path.
fn path_problem(path: &str) -> Option<&'static str> {
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
    /// guest can assemble it, and the hostname of the executions there.
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
    /// The guest assembled the root.
    Ready,
    /// A stage of the root failed, and the guest ends.
    Failed(RootFailure),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a configuration with every field.
    fn full() -> RootConfig {
        RootConfig {
            distro_sectors: Some(116_960),
            scratch_sectors: Some(131_072),
            first_partition_bound: Some(4096),
            overlay_mode: Some(OverlayMode::UntrustedLowers),
            scratch_journal: Some(JournalPolicy::Forbidden),
            workload_identity: Some((1000, 100)),
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
             workload-uid=1000\n\
             workload-gid=100\n"
        );
        // Each mode and policy has its own name.
        for mode in OverlayMode::ALL {
            assert_eq!(OverlayMode::from_name(mode.as_str()), Some(mode));
        }
        for policy in JournalPolicy::ALL {
            assert_eq!(JournalPolicy::from_name(policy.as_str()), Some(policy));
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
        for config in [full(), minimal, largest, RootConfig::default()] {
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
        // The longest valid path, and the largest number.
        let longest = format!("/{}", "a".repeat(4094));
        let large = format!("{base}distro-sectors={}\n", u64::MAX);
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
}
