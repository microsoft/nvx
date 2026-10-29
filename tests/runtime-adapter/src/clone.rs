//! The host program's clones: VMs that a restore of a generation of a template starts, which
//! NVX's template store leases to a sandbox, as `runtime-adapter-host start` asks.
//!
//! A [`Request`] holds what the host program's options choose of a clone: its image, an EROFS
//! layer or a GPT disk no longer than the template's placeholder, its upper, on an ext4 scratch
//! image or in RAM, its processor and memory targets, the workload identity as which its
//! executions run, its overlay mode, its scratch's journal policy, the bound of a GPT disk's
//! first partition, the machine ID that its executions see, the image's own or one that its
//! guest generates, the host paths that it shares with its root, as a launch shares them,
//! `sharing::HostFsSharing`, and its network, as a launch reads it, `layered::read_network`,
//! which must be at the guest's address of the template's NIC, under a policy of its own.
//! [`Request::plan`] plans the image over the upper, with the network, as a launch plans a layered
//! root, [`Request::export`] plans the export of the shares as a launch plans it, and
//! [`Request::clone_request`] returns NVX's `nvx::host::template::CloneRequest` of both, with the
//! bindings that the sandbox's store stages for its images, as `staged::Staged` records them:
//! staging hashes, and a start never does. NVX's `ClonePlan` then checks the request against the
//! leased generation before any VMM starts, from the metadata of the clone's files alone, takes
//! only a network at the guest's address of the template's NIC, whose policy the clone's restore
//! installs on the NIC in place of the template's complete deny-all policy, and only an export
//! that a cold launch attaches, which the clone's restore attaches to the template's dormant
//! filesystem slot as an aggregate.
//!
//! A clone's [record](CloneRecord) names the template store and the generation that the clone
//! leases, beside the plan and the bindings, as the record of a launch names what it launched, so
//! that a later process finds the store whose lease a stop releases.
//!
//! For tests, the host's time at which the store checks the generation's eligibility may come from
//! [`HOST_TIME_VARIABLE`], whose form [`host_time_text`] writes and [`parse_host_time`] reads:
//! a clock fixture, such as the time of another boot of the host, which no test can reach
//! otherwise.

use std::fmt::Write as _;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::{env, fs};

use nvx::activation::{
    Journal, MachineIdMode, OverlayMode as RecordOverlay, StoreGeneration, WorkloadIdentity,
    WorkloadView,
};
use nvx::host::filesystem::ExportPlan;
use nvx::host::process::BootId;
use nvx::host::sandbox::distro::DISTRO_POLICY_REVISION;
use nvx::host::sandbox::{SandboxNetwork, SandboxPlan};
use nvx::host::template::{CloneRequest, HostClock, HostTime};
use nvx::protocol::root::ImageFormat;
use nvx::uuid::Uuid;
use nvx::{Error, Result};
use serde_json::{Value, json};

use crate::layered::{Bindings, RECORD_FORMAT, RECORD_NAME, plan, unstaged, write_record};
use crate::root::{DEFAULT_FIRST_PARTITION_BOUND, JournalPolicy, OverlayMode};
use crate::sharing::HostFsSharing;
use crate::staged::invalid;

/// The mode of a clone's [record](CloneRecord).
pub const CLONE_MODE: &str = "clone";

/// The hostname that a clone's activation record names. The guest leaves the hostname to the
/// activation of the workload's view, which sets it for every execution, as it sets the machine
/// ID that the record names.
pub const CLONE_HOSTNAME: &str = "runtime-adapter";

/// The variable from which `runtime-adapter-host start` takes the host's time, for tests, in the
/// form that [`host_time_text`] writes, in place of the time that `HostTime::now` samples.
pub const HOST_TIME_VARIABLE: &str = "RUNTIME_ADAPTER_HOST_TIME";

/// The host clocks that a time contract names, by their spelling in a manifest.
const CLOCKS: [HostClock; 2] = [HostClock::LinuxBoottime, HostClock::WindowsInterruptTime];

/// What the host program asks of a clone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The image, which the guest expects no longer than the template's placeholder, whose length
    /// is the capacity that the clone's restore presents.
    pub distro: PathBuf,
    /// The image's format.
    pub format: ImageFormat,
    /// The identity that the guest expects of the image.
    pub identity: Uuid,
    /// The ext4 scratch image of the upper, or `None` for an upper in RAM.
    pub scratch: Option<PathBuf>,
    /// The processor target, or `None` for the template's processors online at boot.
    pub processors: Option<u32>,
    /// The memory target, in bytes, or `None` for the template's RAM.
    pub memory: Option<u64>,
    /// The workload identity as which every execution runs, if any: its user ID and its group ID.
    pub workload_identity: Option<(u32, u32)>,
    /// The overlay's mode, or `None` for the guest's default, which trusts the lowers.
    pub overlay_mode: Option<OverlayMode>,
    /// Whether the scratch may have a journal, or `None` for the guest's default, which allows
    /// one.
    pub scratch_journal: Option<JournalPolicy>,
    /// The most bytes that the first partition of a GPT disk may hold, or `None` for
    /// [`DEFAULT_FIRST_PARTITION_BOUND`], the default of a launch's guest.
    pub first_partition_bound: Option<u64>,
    /// The machine ID that the clone's executions see: the image's own, as a launch's do, or one
    /// that the clone's guest generates.
    pub machine_id: MachineIdMode,
    /// The host paths that the clone shares with its root, as a launch shares them, if any.
    pub sharing: HostFsSharing,
    /// The network of the clone's VM, as a launch reads it, `layered::read_network`, if it has
    /// one: a network at the guest's address of the template's NIC, under a policy of its own,
    /// which the clone's restore installs on the NIC, or for a template without a NIC none, or one
    /// that lets nothing through.
    pub network: Option<SandboxNetwork>,
}

impl Request {
    /// Returns the adapter's plan of the clone's image over its upper, with its network, as a
    /// launch plans a layered root, with the paths absolute and normalized, checked against the
    /// host with `SandboxPlan::validated`. The clone's record keeps the plan, so that an execution
    /// of the clone finds the URL of its network's proxy, if any, as a launch's does.
    ///
    /// Fails as a launch plans its root: with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported)
    /// for a path that is not Unicode, and otherwise as `SandboxPlan::validated` does, such as for
    /// an image that is not a regular file.
    pub fn plan(&self) -> Result<SandboxPlan> {
        plan(
            &self.distro,
            self.format,
            self.identity,
            self.scratch.as_deref(),
        )
        .map(|plan| plan.with_network(self.network.clone()))
    }

    /// Plans the export of the host paths that the clone shares with its root, as a launch plans
    /// it, with `sharing::HostFsSharing::plan`, or returns `None` for a clone that shares none,
    /// whose restore leaves the template's filesystem slot dormant.
    ///
    /// Fails as `HostFsSharing::plan` does, such as for a share outside every host directory that
    /// the adapter authorizes. An export that no clone takes, such as one that holds the template
    /// store, or lies inside it, is NVX's `ClonePlan` to refuse.
    pub fn export(&self) -> Result<Option<ExportPlan>> {
        self.sharing.plan()
    }

    /// Returns NVX's request of the clone of `plan`, the [plan](Self::plan) of this request, and
    /// of `export`, the [export](Self::export) of its shares, if any, with the bindings that the
    /// sandbox's store stages for its images, `bindings`, as `staged::Staged::bindings` finds
    /// them, the processor target `processors`, and the memory target `memory`, in bytes.
    ///
    /// The record of the clone's activation names the workload identity, if any, the
    /// [`CLONE_HOSTNAME`], the request's machine ID, the overlay's mode, and the scratch's
    /// journal policy, each the guest's default unless the request names it, as a root
    /// configuration's are, and the bound of a GPT disk's first partition, the default of a
    /// launch's guest, [`DEFAULT_FIRST_PARTITION_BOUND`], unless the request names one. The record
    /// of a clone of an EROFS layer leaves the bound out.
    ///
    /// Fails with [`ErrorKind::Unavailable`](nvx::ErrorKind::Unavailable) if the store stages no
    /// binding for the image, or for the scratch image of an upper on one, at its path.
    pub fn clone_request(
        &self,
        plan: SandboxPlan,
        export: Option<ExportPlan>,
        bindings: &Bindings,
        processors: u32,
        memory: u64,
    ) -> Result<CloneRequest> {
        let binding = bindings
            .distro
            .clone()
            .ok_or_else(|| unstaged("image", plan.distro().path()))?;
        let scratch = match plan.upper().scratch_path() {
            Some(path) => Some(
                bindings
                    .scratch
                    .clone()
                    .ok_or_else(|| unstaged("scratch image", path))?,
            ),
            None => None,
        };
        let overlay = match self.overlay_mode {
            None | Some(OverlayMode::TrustedLowers) => RecordOverlay::TrustedLowers,
            Some(OverlayMode::UntrustedLowers) => RecordOverlay::UntrustedLowers,
        };
        let journal = match self.scratch_journal {
            None | Some(JournalPolicy::Allowed) => Journal::Allowed,
            Some(JournalPolicy::Forbidden) => Journal::Forbidden,
        };
        Ok(CloneRequest {
            adapter: plan,
            binding,
            policy_revision: DISTRO_POLICY_REVISION,
            scratch,
            export,
            processors,
            memory,
            workload: WorkloadView {
                hostname: CLONE_HOSTNAME.as_bytes().to_vec(),
                machine_id: self.machine_id,
                identity: self
                    .workload_identity
                    .map(|(uid, gid)| WorkloadIdentity { uid, gid }),
            },
            overlay,
            journal,
            first_partition_bound: self
                .first_partition_bound
                .unwrap_or(DEFAULT_FIRST_PARTITION_BOUND),
        })
    }
}

/// What the host program records in a sandbox's store of the clone that it starts there, in
/// [`RECORD_NAME`], before it spawns the VMM, as the record of a launch,
/// `layered::LaunchRecord`, in its format, [`RECORD_FORMAT`]:
///
/// ```json
/// {"format": 1, "mode": "clone", "kernelCommandLine": "nvx_sandbox=1 ...",
///  "plan": PLAN, "bindings": {"distro": DISTRO_BINDING, "scratch": SCRATCH_BINDING},
///  "rootConfiguration": null,
///  "template": {"store": "/templates/store", "generation": 1}}
/// ```
///
/// The kernel command line is the template's, which the clone restores, the plan and the bindings
/// those of the clone's image and upper, with its network, as a layered launch records them, so
/// that `layered::LaunchRecord::read_plan` reads the plan back, and an execution of the clone finds
/// the URL of its network's proxy, if any, as a launch's does, and a clone sends no root
/// configuration: its activation record, which NVX keeps in the sandbox, holds its settings.
/// `template` names the template store, by its absolute path, and the generation that the clone
/// leases.
#[derive(Clone, Copy, Debug)]
pub struct CloneRecord<'a> {
    /// The template store.
    pub template: &'a Path,
    /// The generation that the clone leases.
    pub generation: StoreGeneration,
    /// The template's kernel command line.
    pub kernel_command_line: &'a str,
    /// The plan of the clone's image over its upper, with its network.
    pub plan: &'a SandboxPlan,
    /// The bindings that the clone's plan checked.
    pub bindings: &'a Bindings,
}

impl CloneRecord<'_> {
    /// Returns the record as JSON.
    ///
    /// Fails with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) for a template store
    /// whose path is not Unicode, and with [`ErrorKind::Other`](nvx::ErrorKind::Other) if the
    /// plan or a binding has no JSON form.
    pub fn to_json(&self) -> Result<Value> {
        let plan = serde_json::to_value(self.plan).map_err(|error| {
            Error::other("the plan of the clone has no JSON form").with_source(error)
        })?;
        let store = self.template.to_str().ok_or_else(|| {
            Error::unsupported(format!(
                "the path of the template store {} is not Unicode text",
                self.template.display()
            ))
        })?;
        Ok(json!({
            "format": RECORD_FORMAT,
            "mode": CLONE_MODE,
            "kernelCommandLine": self.kernel_command_line,
            "plan": plan,
            "bindings": self.bindings.to_json()?,
            "rootConfiguration": null,
            "template": {"store": store, "generation": self.generation.get().get()},
        }))
    }

    /// Writes the record in `directory`, the sandbox's store, in place of an earlier record.
    ///
    /// Fails as [`to_json`](Self::to_json) does, and with [`ErrorKind::Io`](nvx::ErrorKind::Io)
    /// if the record cannot be written.
    pub fn write(&self, directory: &Path) -> Result<()> {
        write_record(&directory.join(RECORD_NAME), &self.to_json()?)
    }

    /// Reads the template store and the generation that the record in `directory`, the sandbox's
    /// store, names, or returns `None` if the store holds no record, or one of a launch that is no
    /// clone.
    ///
    /// Fails with [`ErrorKind::Io`](nvx::ErrorKind::Io) if the record cannot be read, and with
    /// [`ErrorKind::Unavailable`](nvx::ErrorKind::Unavailable) if it is not JSON, is not in format
    /// [`RECORD_FORMAT`], or is a clone's that does not name its template store by a path and its
    /// generation by a positive number.
    pub fn read_template(directory: &Path) -> Result<Option<(PathBuf, StoreGeneration)>> {
        let path = directory.join(RECORD_NAME);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::io(format!("cannot read {}", path.display())).with_source(error));
            }
        };
        let unreadable = || {
            Error::unavailable(format!(
                "cannot read the template of the launch record {}",
                path.display()
            ))
        };
        let value = serde_json::from_str::<Value>(&text)
            .map_err(|error| unreadable().with_source(error))?;
        if value.get("format").and_then(Value::as_u64) != Some(RECORD_FORMAT) {
            let problem = format!("is not in format {RECORD_FORMAT}");
            return Err(unreadable().with_source(invalid(&problem)));
        }
        if value.get("mode").and_then(Value::as_str) != Some(CLONE_MODE) {
            return Ok(None);
        }
        let template = value.get("template");
        let store = template
            .and_then(|template| template.get("store"))
            .and_then(Value::as_str);
        let generation = template
            .and_then(|template| template.get("generation"))
            .and_then(Value::as_u64)
            .and_then(NonZeroU64::new);
        match store.zip(generation) {
            Some((store, generation)) => Ok(Some((
                PathBuf::from(store),
                StoreGeneration::new(generation),
            ))),
            None => Err(unreadable().with_source(invalid(
                "does not name its template store by a path and its generation by a positive \
                 number",
            ))),
        }
    }
}

/// Returns `time` in the form of [`HOST_TIME_VARIABLE`]:
/// `host=HOST,boot=BOOT,clock=CLOCK,monotonic=NANOSECONDS,utc=NANOSECONDS`, with the host's
/// identity and its boot's, each 32 lowercase hexadecimal digits, the clock as a manifest spells
/// it, `linux-boottime` or `windows-interrupt-time`, and the readings of the monotonic clock and
/// of UTC, in nanoseconds, UTC's since the Unix epoch.
pub fn host_time_text(time: &HostTime) -> String {
    let mut host = String::with_capacity(32);
    for byte in time.host_id {
        let _ = write!(host, "{byte:02x}");
    }
    format!(
        "host={host},boot={},clock={},monotonic={},utc={}",
        time.boot_id,
        time.clock.as_str(),
        time.monotonic_ns,
        time.utc_ns
    )
}

/// Reads a host's time from `text`, in the form that [`host_time_text`] writes, with its fields
/// in that order, or says why it is not one.
pub fn parse_host_time(text: &str) -> std::result::Result<HostTime, String> {
    let invalid = || {
        format!(
            "{HOST_TIME_VARIABLE} takes host=HOST,boot=BOOT,clock=CLOCK,monotonic=NANOSECONDS,\
             utc=NANOSECONDS, not {text:?}"
        )
    };
    let mut fields = text.split(',');
    let mut field = |name: &str| {
        fields
            .next()
            .and_then(|field| field.strip_prefix(name))
            .and_then(|field| field.strip_prefix('='))
            .ok_or_else(invalid)
    };
    let host = field("host")?;
    let boot = field("boot")?;
    let clock = field("clock")?;
    let monotonic = field("monotonic")?;
    let utc = field("utc")?;
    if fields.next().is_some() {
        return Err(invalid());
    }
    let host_id = hexadecimal_id(host).ok_or_else(invalid)?;
    // A boot's identity has the same text form, which its parser also checks.
    hexadecimal_id(boot).ok_or_else(invalid)?;
    let boot_id = boot.parse::<BootId>().map_err(|_| invalid())?;
    let clock = CLOCKS
        .into_iter()
        .find(|known| known.as_str() == clock)
        .ok_or_else(invalid)?;
    let number = |text: &str| {
        text.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| text.parse::<u64>().ok())
            .flatten()
            .ok_or_else(invalid)
    };
    Ok(HostTime {
        host_id,
        boot_id,
        clock,
        monotonic_ns: number(monotonic)?,
        utc_ns: number(utc)?,
    })
}

/// Returns the host's time that [`HOST_TIME_VARIABLE`] sets, or `None` without the variable, or
/// says why its value is not a host's time.
pub fn host_time_from_env() -> std::result::Result<Option<HostTime>, String> {
    match env::var_os(HOST_TIME_VARIABLE) {
        None => Ok(None),
        Some(value) => {
            let text = value
                .into_string()
                .map_err(|value| format!("{HOST_TIME_VARIABLE} is not Unicode: {value:?}"))?;
            parse_host_time(&text).map(Some)
        }
    }
}

/// Returns the 16 bytes that `text`, 32 lowercase hexadecimal digits, spells, or `None` if it is
/// not that.
fn hexadecimal_id(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).ok()?;
    }
    Some(bytes)
}
