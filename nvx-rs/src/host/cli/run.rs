//! `nvx-rs run`: the port of the former `nvx.py run`, from `command_run` and the `run` parser of
//! `scripts/nvx.py`, with its options, defaults, checks, outputs, and exit statuses.
//!
//! It boots a guest in a microVM on the terminal: it prints OpenVMM's command as `>> COMMAND`,
//! then runs OpenVMM with the standard streams, unless `--dry-run` is given, and exits with
//! OpenVMM's status. The parser reads the options of `nvx.py run` with their names, defaults,
//! types, and choices, and ends a usage error with status 2.
//!
//! A cold boot forwards the terminal through the VM's boot-console listener, `hvc1`, and the
//! VM's log to standard error. Its temporary private VM directory is removed after confirmed
//! exit.
//!
//! With `--restore-snapshot NAME`, it restores the named snapshot of `--store-dir`, which
//! `snapshot capture` captured, once, in place of a cold boot, through the path of
//! `snapshot restore`: it provisions a managed sandbox in `--state-dir`, outside the store, with
//! the image, upper, and workload of the options that `snapshot restore` takes for them, which
//! `sandbox` defines and parses, and starts it, under the snapshot's lock, through the blocking
//! restore driver, which forwards the workload's output. It stops the VM once the workload has
//! exited with status 0, removes the sandbox in any case, and exits with the workload's status, as
//! [`sandbox::restore_once`] does. A raw OpenVMM snapshot, which the option named before, is
//! refused before anything is read or launched. A restore refuses the options of a cold boot that
//! its snapshot or its sandbox fixes, such as `--guest`, `--processors`, the shares, `--cmdline`,
//! and `--dry-run`, and a cold boot the options of a restore.
//!
//! A cold boot takes `--net` and `--network-profile` together. A restore takes the addressing of
//! its NIC from the snapshot, so it refuses `--net`. The network options of the restore of a
//! snapshot with a NIC, which need `--network-profile`, are the restore's own network policy, with
//! its DNS server, egress, forwards, and published listeners, as `sandbox` lowers them; those of a
//! snapshot without a NIC, which `snapshot capture` captures, are refused.
//!
//! Its shares differ from those of `sandbox`. `--mount-deny`, `--mount-allow`, and `--mount-write`
//! name host paths, which must be absolute with several shares or a shared file. A cold boot
//! checks their ownership in the filesystem export plan, exports its numbered children, and adds
//! the plan's `nvx_share=` rows to the kernel command line.
//!
//! After OpenVMM exits with a status other than 0 from a cold boot, on a host whose CPU no built-in
//! CPU profile serves, it prints the guidance of [`cpu_profile`] to standard error, which says
//! what `--cpu-profile auto` does on that CPU and how to request a built-in profile for it. It
//! never prints it after a dry run, a restore, a refused option, or Ctrl-C.

mod guest_tokens;

use std::fs::{self, File};
use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::args::{Parser, Receiver, Spec, Syntax, choice, integer};
use super::artifacts::{Located, locate};
use super::cpu_profile::{self, HostCpu};
use super::download::{self, Hold};
use super::network::{NativeRules, NetworkOptions};
use super::sandbox::{
    self, HYPERVISORS, MAX_MEMORY_MIB, MOUNT_OWNERS, copy_openvmm_outcome, error_with_cause,
    hypervisor, path_string, path_text, sandbox_network, validate_outcome_destination,
};
use super::session::{AttachmentCancellation, Session};
use super::shell::format_command;
use super::snapshots::{self, Name};
use super::{Exit, Result, check_interrupt, output};
use crate::ErrorCode;
use crate::activation::{MachineIdMode, View};
use crate::host::artifacts::{ArtifactDir, Guest, require_artifact};
use crate::host::config::{Boot, ConsoleAttachment, NicConfig, RootConfig, VmConfig};
use crate::host::filesystem::{
    self, ExportPlan, ExportRequest, Mapping, Owner, PathList, ReservedTargets, Target,
};
use crate::host::platform::{self, Transport};
use crate::host::runtime::{ArtifactIdentity, LaunchOptions, Runtime, RuntimeIdentity};
use crate::host::sandbox::SANDBOX_COMMAND_LINE_MAX_SIZE;
use crate::host::sandbox::mount::{
    MountAccess, MountKind, MountOwner, POLICY_KINDS, PolicyKind, export_command_line_fragment,
    mount_kind, parse_mount_value, require_mount_owner_supported,
};
use crate::host::sandbox::options::{SandboxOperation, validate_run_share_targets};
use crate::host::vm::{self, VmDir};

/// The usage and help of the parser.
pub(super) const SYNTAX: Syntax = Syntax {
    command: "run",
    arguments: "[OPTIONS]",
    help: "\
options:
  -h, --help            show this help message and exit
  --guest {alpine,ubuntu,azurelinux}
  --hypervisor {auto,whp,kvm,mshv}
  --machine {microvm}
  --memory-mib MEMORY_MIB
                        guest RAM in MiB (default by --guest: alpine 128,
                        ubuntu 512, azurelinux 512; for a restore: 256)
  --memory-capacity-mib MEMORY_CAPACITY_MIB
  --processors {1,2,4,8}
  --cpu-profile ID      OpenVMM CPU profile: auto (OpenVMM's default) for the
                        built-in profile of the host's CPU, a built-in profile
                        ID, or host to derive a development profile from this
                        host (doc/usage.md)
  --mount GUEST_TARGET,HOST_PATH[,ro|rw]
                        live-share a host directory, or a single regular host
                        file without its directory; the mode follows the last
                        comma, so HOST_PATH may contain commas when the mode
                        is given; repeat for more shares, each with its own
                        guest target and mode, as children of one virtio-fs
                        device
  --mount-deny MOUNT_DENY
  --mount-allow HOST_PATH
                        expose one existing path inside a --mount-deny path
                        again
  --mount-write HOST_PATH
                        make one existing path one of the only writable parts
                        of a read-write --mount
  --mount-owner {vmm,caller}
                        host identity of the share's file operations: vmm
                        (default) or caller, with guest root squashed to the
                        owner of the shared directory or file (Linux only)
  --net IPV4/PREFIX
  --network-profile {portable}
  --network-egress {allow,deny}
  --network-ingress {allow,deny}
  --network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]
                        allow guest egress to an IPv4 or IPv6 CIDR, optionally
                        only for PROTOCOL tcp, udp, or icmp, and for tcp or
                        udp only on PORT or on the inclusive range PORT-PORT;
                        repeat to add rules
  --network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]
                        deny guest egress to an IPv4 or IPv6 CIDR, optionally
                        only for PROTOCOL tcp, udp, or icmp, and for tcp or
                        udp only on PORT or on the inclusive range PORT-PORT;
                        repeat to add rules
  --network-egress-policy-file PATH
                        load bounded MXC-shaped IPv4 and IPv6 egress rules
                        from JSON
  --host-loopback {allow,deny}
  --network-proxy IPV4:TCP-PORT
  --host-loopback-forward HOST_LOOPBACK_FORWARD
  --outcome-report OUTCOME_REPORT
                        write a bounded local JSON outcome report
  --cmdline CMDLINE
  --restore-snapshot NAME
                        restore the named snapshot of --store-dir, which
                        snapshot capture captured, once, in place of a cold
                        boot, run the workload of the options below, and exit
                        with its status; a raw OpenVMM snapshot is refused
  --store-dir PATH      the named snapshot store
  --state-dir STATE_DIR
                        the restore's sandbox, outside the store, on its
                        filesystem, which the run removes once it ends
  --layer ROLE,PATH,IMAGE_UUID
  --layer-format {erofs,gpt}
  --scratch SCRATCH
  --upper {scratch,ram}
  --ram-upper-percent PERCENT
  --entrypoint ENTRYPOINT
  --arg SANDBOX_ARG
  --hostname HOSTNAME
  --workload-user UID:GID
  --timeout TIMEOUT     seconds for each wait for a lock, and for the restore
                        and its workload (default: 60)
  --dry-run",
};

/// The choices of `--guest`.
const GUESTS: [&str; 3] = [
    Guest::Alpine.name(),
    Guest::Ubuntu.name(),
    Guest::AzureLinux.name(),
];
/// The choices of `--machine`.
const MACHINES: [&str; 1] = ["microvm"];
/// The choices of `--processors`, the processor counts that a microVM takes.
const PROCESSORS: [&str; 4] = ["1", "2", "4", "8"];

/// The options of `snapshot restore` that a restore of `run` takes, which `sandbox` defines and
/// parses for both: the sandbox's state directory, the image, its upper, the workload, and the
/// timeout of the restore.
const RESTORE_INPUTS: [&str; 11] = [
    "--state-dir",
    "--layer",
    "--layer-format",
    "--scratch",
    "--upper",
    "--ram-upper-percent",
    "--entrypoint",
    "--arg",
    "--hostname",
    "--workload-user",
    "--timeout",
];

/// The options of a cold boot that a restore refuses: its snapshot fixes the guest, the processors,
/// the CPU profile, and the NIC's address, and its sandbox takes no shares, kernel command line,
/// outcome report, or dry run.
const COLD_ONLY: [&str; 13] = [
    "--guest",
    "--memory-capacity-mib",
    "--processors",
    "--cpu-profile",
    "--mount",
    "--mount-deny",
    "--mount-allow",
    "--mount-write",
    "--mount-owner",
    "--net",
    "--outcome-report",
    "--cmdline",
    "--dry-run",
];

/// The options of `nvx-rs run` but [`RESTORE_INPUTS`], which parse with the defaults of the
/// former `nvx.py run`.
const OPTIONS: [Spec; 27] = [
    Spec::one("--guest"),
    Spec::one("--hypervisor"),
    Spec::one("--machine"),
    Spec::one("--memory-mib"),
    Spec::one("--memory-capacity-mib"),
    Spec::one("--processors"),
    Spec::one("--cpu-profile"),
    Spec::one("--mount"),
    Spec::one("--mount-deny"),
    Spec::one("--mount-allow"),
    Spec::one("--mount-write"),
    Spec::one("--mount-owner"),
    Spec::one("--net"),
    Spec::one("--network-profile"),
    Spec::one("--network-egress"),
    Spec::one("--network-ingress"),
    Spec::one("--network-egress-allow"),
    Spec::one("--network-egress-deny"),
    Spec::one("--network-egress-policy-file"),
    Spec::one("--host-loopback"),
    Spec::one("--network-proxy"),
    Spec::one("--host-loopback-forward"),
    Spec::one("--outcome-report"),
    Spec::one("--cmdline"),
    Spec::one("--restore-snapshot"),
    Spec::one("--store-dir"),
    Spec::flag("--dry-run"),
];

/// Returns the options of `nvx-rs run`: its own, and [`RESTORE_INPUTS`] as `snapshot restore`
/// takes them, so that a restore of `run` takes none that `snapshot restore` refuses.
pub(super) fn options() -> Vec<Spec> {
    OPTIONS
        .into_iter()
        .chain(
            snapshots::restore_options()
                .into_iter()
                .filter(|spec| RESTORE_INPUTS.contains(&spec.name)),
        )
        .collect()
}

/// The parsed options of `nvx-rs run`: the namespace of the former `nvx.py run`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Options {
    /// `--guest`, Alpine by default.
    pub(crate) guest: Guest,
    /// `--hypervisor`, `auto` by default.
    pub(crate) hypervisor: &'static str,
    /// `--machine`, `microvm` by default.
    pub(crate) machine: &'static str,
    /// `--memory-mib`, the guest's default if not given.
    pub(crate) memory_mib: Option<u64>,
    /// `--memory-capacity-mib`.
    pub(crate) memory_capacity_mib: Option<u64>,
    /// `--processors`, 1 by default.
    pub(crate) processors: u32,
    /// `--cpu-profile`.
    pub(crate) cpu_profile: Option<String>,
    /// The `--mount` options, as given.
    pub(crate) mounts: Vec<String>,
    /// The `--mount-deny`, `--mount-allow`, and `--mount-write` options, in order, as given.
    pub(crate) policy_paths: Vec<(PolicyKind, String)>,
    /// `--mount-owner`.
    pub(crate) mount_owner: Option<MountOwner>,
    /// The network options.
    pub(crate) network: NetworkOptions,
    /// `--outcome-report`, as given.
    pub(crate) outcome_report: Option<String>,
    /// `--cmdline`, empty by default.
    pub(crate) cmdline: String,
    /// `--restore-snapshot`, as given: the name of a snapshot of `--store-dir`.
    pub(crate) restore_snapshot: Option<String>,
    /// `--store-dir`, as given.
    pub(crate) store_dir: Option<String>,
    /// The options of [`RESTORE_INPUTS`], as `sandbox` parses them, for the sandbox that a restore
    /// provisions.
    pub(crate) restore: sandbox::Options,
    /// The names of the options given, in their order, which [`check`] refuses for a cold boot or
    /// a restore that does not take them.
    pub(crate) given: Vec<&'static str>,
    /// `--dry-run`.
    pub(crate) dry_run: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            guest: Guest::Alpine,
            hypervisor: "auto",
            machine: "microvm",
            memory_mib: None,
            memory_capacity_mib: None,
            processors: 1,
            cpu_profile: None,
            mounts: Vec::new(),
            policy_paths: Vec::new(),
            mount_owner: None,
            network: NetworkOptions::default(),
            outcome_report: None,
            cmdline: String::new(),
            restore_snapshot: None,
            store_dir: None,
            restore: sandbox::Options {
                operation: SandboxOperation::Provision,
                ..sandbox::Options::default()
            },
            given: Vec::new(),
            dry_run: false,
        }
    }
}

/// Returns the processor count that `value` names, one of the [`PROCESSORS`].
fn processors(value: &str) -> std::result::Result<u32, String> {
    let count = choice(value, &PROCESSORS)?;
    Ok(count.parse().expect("the processor counts are integers"))
}

/// Returns the MiB of guest RAM that `value` gives, from 1 to [`MAX_MEMORY_MIB`].
fn memory(value: &str) -> std::result::Result<u64, String> {
    integer(value, 1..=MAX_MEMORY_MIB)
}

impl Receiver for Options {
    fn take(&mut self, name: &'static str, value: Option<&str>) -> std::result::Result<(), String> {
        self.given.push(name);
        let text = || value.unwrap_or_default().to_owned();
        match name {
            "--guest" => {
                let guest = choice(&text(), &GUESTS)?;
                self.guest = Guest::ALL
                    .into_iter()
                    .find(|candidate| candidate.name() == guest)
                    .unwrap_or(Guest::Alpine);
            }
            "--hypervisor" => self.hypervisor = choice(&text(), &HYPERVISORS)?,
            "--machine" => self.machine = choice(&text(), &MACHINES)?,
            "--memory-mib" => self.memory_mib = Some(memory(&text())?),
            "--memory-capacity-mib" => self.memory_capacity_mib = Some(memory(&text())?),
            "--processors" => self.processors = processors(&text())?,
            "--cpu-profile" => self.cpu_profile = Some(text()),
            "--mount" => self.mounts.push(text()),
            "--mount-deny" => self.policy_paths.push((PolicyKind::Denied, text())),
            "--mount-allow" => self.policy_paths.push((PolicyKind::Allowed, text())),
            "--mount-write" => self.policy_paths.push((PolicyKind::Writable, text())),
            "--mount-owner" => {
                self.mount_owner = Some(match choice(&text(), &MOUNT_OWNERS)? {
                    "caller" => MountOwner::Caller,
                    _ => MountOwner::Vmm,
                });
            }
            "--outcome-report" => self.outcome_report = Some(text()),
            "--cmdline" => self.cmdline = text(),
            "--restore-snapshot" => self.restore_snapshot = Some(text()),
            "--store-dir" => self.store_dir = Some(text()),
            "--dry-run" => self.dry_run = true,
            _ if RESTORE_INPUTS.contains(&name) => return self.restore.take(name, value),
            _ => return self.network.take(name, &text()),
        }
        Ok(())
    }
}

/// Parses the arguments of `run`, and returns the options and the arguments that the parser does
/// not recognize.
pub(super) fn parse(arguments: &[String]) -> Result<(Options, Vec<String>)> {
    let mut options = Options::default();
    let unrecognized = Parser::new(&self::options(), &[])
        .parse(arguments, &mut options)
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX))?;
    Ok((options, unrecognized))
}

/// Checks `options` before anything that it looks up: a cold boot refuses the options that only a
/// restore takes, and a restore those of [`COLD_ONLY`], in the order given.
fn check(options: &Options) -> Result {
    let refused = |message: String| Err(Exit::error(ErrorCode::Unsupported, message));
    let restore = options.restore_snapshot.is_some();
    for option in &options.given {
        if restore && COLD_ONLY.contains(option) {
            return refused(format!(
                "{option} is not accepted for restore; the snapshot and the restore's own \
                 sandbox fix it"
            ));
        }
        if !restore && (*option == "--store-dir" || RESTORE_INPUTS.contains(option)) {
            return refused(format!("{option} requires --restore-snapshot"));
        }
    }
    let network = &options.network;
    if !restore && network.net.is_some() != network.network_profile.is_some() {
        return refused("--net and --network-profile must be specified together".to_owned());
    }
    if let Some(capacity) = &options.memory_capacity_mib
        && *capacity < memory_mib(options)
    {
        return refused("--memory-capacity-mib cannot be below --memory-mib".to_owned());
    }
    Ok(())
}

/// Returns the guest RAM in MiB: `--memory-mib`, or else the guest's default.
fn memory_mib(options: &Options) -> u64 {
    options
        .memory_mib
        .unwrap_or_else(|| u64::from(options.guest.default_memory_mib()))
}

/// Runs the microVM of `options`, as `command_run` did, once the release of a checkout without
/// artifacts is downloaded, as [`download::ensure`] decides, unless the options are refused, or
/// restores the named snapshot of `--restore-snapshot`, as [`restore`] does.
pub(super) fn run(root: Option<&str>, options: Options) -> Result {
    check(&options)?;
    if options.restore_snapshot.is_some() {
        return restore(root, options);
    }
    let rules = options.network.egress_rules()?;
    // The shares and the network follow the artifacts on the command line, and a missing artifact
    // is reported before them, as `command_run` did, but a command that they refuse downloads
    // nothing, though it reads the artifacts under the lock of downloads all the same. Their
    // arguments are built again once the artifacts are in place, as a share's kind depends on what
    // its host path holds, which a download may install.
    let refused = cold_preflight(&options, &rules).is_err();
    let located = locate(root)?;
    let hold = if refused {
        download::hold(&located)?
    } else {
        download::ensure(&located, Some(options.guest), options.hypervisor)?
    };
    let Located { dir, openvmm } = located;
    require_artifact(&openvmm, "OpenVMM release binary")?;
    run_cold(dir, openvmm, &options, &rules, hold)
}

/// Restores the named snapshot of `--store-dir` once, through the path of `snapshot restore`, as
/// [`sandbox::restore_once`] does, with the source that [`snapshots::restore_source`] checks: in
/// a sandbox in `--state-dir`, which must not exist, since the run removes it once it ends, and
/// must lie outside the store, on the store's filesystem, since the restore links the snapshot's
/// files into it, with the image, upper, and workload of the options, and the run's network policy.
fn restore(root: Option<&str>, options: Options) -> Result {
    let refused = |message: String| Exit::error(ErrorCode::Unsupported, message);
    let raw = "it names a snapshot of --store-dir, which `snapshot capture` captured, and no \
               longer a raw OpenVMM snapshot";
    let Some(store_dir) = &options.store_dir else {
        return Err(refused(format!(
            "--restore-snapshot requires --store-dir: {raw}"
        )));
    };
    let given = options.restore_snapshot.clone().unwrap_or_default();
    let name = Name::try_from(given.clone()).map_err(|error| {
        refused(format!(
            "--restore-snapshot {given:?} is not a snapshot name: {raw}; {}",
            error.message()
        ))
    })?;
    let mut sandbox = options.restore;
    let Some(state_dir) = sandbox.state_dir.as_deref().filter(|dir| !dir.is_empty()) else {
        return Err(refused(
            "--restore-snapshot requires --state-dir: the directory of the restore's sandbox, \
             outside the store, on its filesystem"
                .to_owned(),
        ));
    };
    // The run removes the directory once it ends, so it takes only one that it creates.
    if fs::symlink_metadata(state_dir).is_ok() {
        return Err(refused(format!(
            "the restore's --state-dir {state_dir} exists: the run creates it, and removes it \
             once it ends"
        )));
    }
    let (source, nic) = snapshots::restore_source(name, store_dir, &sandbox)?;
    sandbox.network = restored_network(options.network, nic)?;
    sandbox.hypervisor = options.hypervisor;
    sandbox.memory_mib = options.memory_mib.unwrap_or(sandbox.memory_mib);
    sandbox::restore_once(root, sandbox, source)
}

/// Returns the network options of the sandbox that restores a snapshot whose NIC is `nic`, which
/// the snapshot fixes: `network`, the run's own, at the NIC's address, which need
/// `--network-profile`, as the policy of the restore's NIC, or none for a snapshot without a NIC.
pub(super) fn restored_network(
    mut network: NetworkOptions,
    nic: Option<NicConfig>,
) -> Result<NetworkOptions> {
    let refused = |message: &str| Err(Exit::error(ErrorCode::Unsupported, message));
    match (nic, network.network_profile) {
        (None, _) if network.any() => {
            refused("the snapshot has no NIC, so its restore takes no network options")
        }
        (Some(_), None) => {
            refused("the snapshot has a NIC, so its restore needs --network-profile")
        }
        (Some(nic), Some(_)) => {
            network.net = Some(nic.guest_network.to_string());
            Ok(network)
        }
        (None, _) => Ok(network),
    }
}

/// Checks only options, not paths that artifact installation may create or replace.
fn cold_preflight(options: &Options, rules: &NativeRules) -> Result {
    let shares = cold_shares(options)?;
    if let Some(owner) = options.mount_owner {
        require_mount_owner_supported(owner)?;
    }
    for (target, _, _) in &shares {
        if let Some(reserved) = ReservedTargets::BLOCKLESS_ROOT.reservation(target) {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                format!("--mount target {target} overlaps reserved guest path {reserved}"),
            ));
        }
    }
    if shares.len() > 1 {
        for (kind, given) in &options.policy_paths {
            if !Path::new(&path_text(given)).is_absolute() {
                return Err(Exit::error(
                    ErrorCode::Unsupported,
                    format!(
                        "with several --mount shares, {} must be an absolute host path",
                        kind.option()
                    ),
                ));
            }
        }
    }
    cold_vm_config(options, rules, None).map(|_| ())
}

fn cold_shares(options: &Options) -> Result<Vec<(String, String, MountAccess)>> {
    let shares = options
        .mounts
        .iter()
        .map(|value| parse_mount_value(value))
        .collect::<crate::Result<Vec<_>>>()?;
    validate_run_share_targets(
        &shares
            .iter()
            .map(|(target, _, _)| target.as_str())
            .collect::<Vec<_>>(),
    )?;
    if shares.is_empty() && (options.mount_owner.is_some() || !options.policy_paths.is_empty()) {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            if options.mount_owner.is_some() {
                "--mount-owner requires --mount"
            } else {
                "mount policy options require --mount"
            },
        ));
    }
    Ok(shares)
}

fn cold_config(options: &Options, rules: &NativeRules) -> Result<VmConfig> {
    let shares = cold_shares(options)?;
    let hosts: Vec<_> = shares
        .iter()
        .map(|(_, host, _)| Path::new(host).to_owned())
        .collect();
    let aggregate =
        shares.len() > 1 || hosts.iter().any(|host| mount_kind(host) == MountKind::File);
    let mut policies: [Vec<std::path::PathBuf>; 3] = Default::default();
    for kind in POLICY_KINDS {
        for (_, given) in options.policy_paths.iter().filter(|(of, _)| *of == kind) {
            let given = path_text(given);
            let path = Path::new(&given);
            if aggregate && !path.is_absolute() {
                return Err(Exit::error(
                    ErrorCode::Unsupported,
                    format!(
                        "with several --mount shares, or a shared file, {} {given} must be an absolute host path",
                        kind.option()
                    ),
                ));
            }
            let path = if path.is_absolute() {
                path.to_owned()
            } else {
                hosts[0].join(path)
            };
            policies[match kind {
                PolicyKind::Denied => 0,
                PolicyKind::Allowed => 1,
                PolicyKind::Writable => 2,
            }]
            .push(path);
        }
    }
    let mappings: Vec<_> = shares
        .iter()
        .zip(&hosts)
        .map(|((target, _, access), host)| Mapping {
            name: "--mount",
            path: host,
            target: Target::Guest(target),
            read_only: *access == MountAccess::ReadOnly,
        })
        .collect();
    let filesystem = filesystem::plan(&ExportRequest {
        mappings: &mappings,
        denied: PathList {
            name: "--mount-deny",
            paths: &policies[0],
        },
        allowed: PathList {
            name: "--mount-allow",
            paths: &policies[1],
        },
        write: PathList {
            name: "--mount-write",
            paths: &policies[2],
        },
        owner: if options.mount_owner == Some(MountOwner::Caller) {
            Owner::Caller
        } else {
            Owner::Vmm
        },
        reserved: ReservedTargets::BLOCKLESS_ROOT,
    })
    .map_err(error_with_cause)?;
    cold_vm_config(options, rules, filesystem)
}

fn cold_vm_config(
    options: &Options,
    rules: &NativeRules,
    filesystem: Option<ExportPlan>,
) -> Result<VmConfig> {
    let network = sandbox_network(&options.network, rules)?;
    let config = VmConfig {
        processors: options.processors,
        memory_mib: memory_mib(options),
        root: RootConfig::Blockless,
        view: View {
            hostname: b"nvx".to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network,
        console: ConsoleAttachment::Listener,
        boot: Some(Boot {
            command_line: guest_tokens::render(&options.cmdline, filesystem.as_ref())
                .map_err(error_with_cause)?,
            memory_capacity_mib: options.memory_capacity_mib,
            capture: None,
        }),
        filesystem,
    };
    if let Some(export) = &config.filesystem {
        let line = config.kernel_command_line().map_err(error_with_cause)?;
        if line.len() + export_command_line_fragment(export).len() + 1
            > SANDBOX_COMMAND_LINE_MAX_SIZE
        {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                "the --mount shares and --cmdline exceed the 1024-byte budget of the kernel command \
                 line; attach fewer shares or use shorter targets",
            ));
        }
    }
    config.check_command_line().map_err(error_with_cause)?;
    Ok(config)
}

fn remove_vm_directory(dir: &VmDir) -> Result {
    for name in [
        "openvmm.log",
        "outcome.json",
        "control.sock",
        "console.sock",
    ] {
        match fs::remove_file(dir.path().join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(Exit::from_os_error(error)),
        }
    }
    fs::remove_dir(dir.path()).map_err(Exit::from_os_error)
}

fn connect_running(
    launched: &mut vm::Launched,
    endpoint: &str,
) -> Result<Option<Box<dyn Transport>>> {
    let pid = launched.process.reference().pid;
    // On Linux, OpenVMM may write its console output and exit before the console's peer check.
    #[cfg(target_os = "linux")]
    let connect = || platform::connect_child_endpoint(endpoint, pid);
    #[cfg(not(target_os = "linux"))]
    let connect = || platform::connect_endpoint(endpoint, pid, Duration::from_millis(250));
    connect_before_exit(endpoint, connect, |timeout| launched.wait_status(timeout))
}

/// Connects to the verified endpoint, or yields to the owned child's confirmed exit.
fn connect_before_exit<T>(
    endpoint: &str,
    mut connect: impl FnMut() -> io::Result<T>,
    mut wait_status: impl FnMut(Duration) -> io::Result<Option<i32>>,
) -> Result<Option<T>> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        check_interrupt()?;
        if wait_status(Duration::ZERO)
            .map_err(Exit::from_os_error)?
            .is_some()
        {
            return Ok(None);
        }
        let error = match connect() {
            Ok(transport) => return Ok(Some(transport)),
            Err(error) => error,
        };
        let retry = matches!(
            error.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::TimedOut
                | io::ErrorKind::WouldBlock
        );
        // Peer authentication may observe exit just before the child status becomes available.
        let exit_wait = if retry {
            Duration::ZERO
        } else {
            Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now()))
        };
        if wait_status(exit_wait)
            .map_err(Exit::from_os_error)?
            .is_some()
        {
            return Ok(None);
        }
        if !retry {
            return Err(Exit::from_os_error(error));
        }
        if Instant::now() >= deadline {
            return Err(Exit::error(
                ErrorCode::TimedOut,
                format!("VM console endpoint did not become available: {endpoint}"),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Keeps standard input, when it is a terminal, in raw mode with terminal interrupts, and restores
/// the terminal's entry mode when dropped, on success and error paths alike.
struct TerminalGuard(Option<EntryMode>);

/// The console input mode on entry, which [`TerminalGuard`] restores exactly: on Windows,
/// crossterm's `disable_raw_mode` sets line input, echo and processed input instead of restoring
/// their entry values.
#[cfg(windows)]
type EntryMode = windows_sys::Win32::System::Console::CONSOLE_MODE;

/// Nothing: crossterm saves the terminal's entry mode, which its `disable_raw_mode` restores.
#[cfg(not(windows))]
struct EntryMode;

impl TerminalGuard {
    fn new() -> Result<Self> {
        if !io::stdin().is_terminal() {
            return Ok(Self(None));
        }
        let entry = Self::entry_mode().map_err(Exit::from_os_error)?;
        crossterm::terminal::enable_raw_mode().map_err(Exit::from_os_error)?;
        let guard = Self(Some(entry));
        Self::enable_interrupts().map_err(Exit::from_os_error)?;
        Ok(guard)
    }

    /// Reads the console input mode that the guard restores.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn entry_mode() -> io::Result<EntryMode> {
        use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE};

        let mut mode = 0;
        // SAFETY: stdin is this process's console input, and `mode` outlives the call.
        if unsafe { GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(mode)
    }

    /// Restores the console input mode that [`Self::entry_mode`] read.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn restore(entry: EntryMode) -> io::Result<()> {
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};

        // SAFETY: stdin is this process's console input, whose entry mode the guard saved.
        if unsafe { SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), entry) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Reads nothing: crossterm saves the terminal's entry mode when it enables raw mode.
    #[cfg(not(windows))]
    fn entry_mode() -> io::Result<EntryMode> {
        Ok(EntryMode)
    }

    /// Restores the terminal mode that crossterm saved when it enabled raw mode.
    #[cfg(not(windows))]
    fn restore(_entry: EntryMode) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }

    #[cfg(unix)]
    fn enable_interrupts() -> io::Result<()> {
        use nix::sys::termios::{
            LocalFlags, SetArg, SpecialCharacterIndices, tcgetattr, tcsetattr,
        };

        let stdin = io::stdin();
        let mut mode = tcgetattr(&stdin)?;
        mode.local_flags.insert(LocalFlags::ISIG);
        // Only the interrupt key is a host signal; quit/suspend remain guest input.
        for key in [
            SpecialCharacterIndices::VQUIT,
            SpecialCharacterIndices::VSUSP,
            #[cfg(any(
                target_vendor = "apple",
                target_os = "freebsd",
                target_os = "dragonfly",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "solaris",
                target_os = "illumos",
                target_os = "aix"
            ))]
            SpecialCharacterIndices::VDSUSP,
        ] {
            mode.control_chars[key as usize] = nix::libc::_POSIX_VDISABLE as _;
        }
        tcsetattr(&stdin, SetArg::TCSANOW, &mode)?;
        Ok(())
    }

    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn enable_interrupts() -> io::Result<()> {
        use windows_sys::Win32::System::Console::{
            ENABLE_PROCESSED_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
        };

        let mut mode = 0;
        // SAFETY: stdin is this process's terminal, whose saved entry mode the guard restores.
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            if GetConsoleMode(input, &mut mode) == 0
                || SetConsoleMode(input, mode | ENABLE_PROCESSED_INPUT) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn enable_interrupts() -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(entry) = self.0.take()
            && let Err(error) = Self::restore(entry)
        {
            output::print_stderr(&format!("cannot restore terminal mode: {error}"));
        }
    }
}

type InputCompletion = mpsc::Receiver<io::Result<()>>;

fn console_input(console: Box<dyn Transport>) -> Result<(Box<dyn Transport>, InputCompletion)> {
    #[cfg(any(target_os = "linux", windows))]
    {
        let duplicate = || {
            console
                .try_clone_handle()
                .map_err(Exit::from_os_error)?
                .ok_or_else(|| {
                    Exit::error(
                        ErrorCode::Unsupported,
                        "the VM console cannot be duplicated",
                    )
                })
        };
        let reader = duplicate()?;
        let writer = duplicate()?;
        drop(console);
        let reader = platform::transport_from_handle(reader).map_err(Exit::from_os_error)?;
        let mut writer = platform::transport_from_handle(writer).map_err(Exit::from_os_error)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("nvx-run-input".to_owned())
            .spawn(move || {
                let result = (|| {
                    let mut stdin = io::stdin().lock();
                    let mut buffer = [0; 8192];
                    loop {
                        let count = stdin.read(&mut buffer)?;
                        if count == 0 {
                            return Ok(());
                        }
                        match writer.write_all(&buffer[..count], None) {
                            Ok(()) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::BrokenPipe
                                        | io::ErrorKind::ConnectionReset
                                        | io::ErrorKind::WriteZero
                                ) =>
                            {
                                return Ok(());
                            }
                            Err(error) => return Err(error),
                        }
                    }
                })();
                let _ = sender.send(result);
            })
            .map_err(Exit::from_os_error)?;
        Ok((reader, receiver))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        drop(console);
        Err(Exit::error(
            ErrorCode::Unsupported,
            "foreground console I/O requires a supported NVX host",
        ))
    }
}

struct ConsoleOutput {
    write: fn(&[u8]) -> io::Result<()>,
    #[cfg(windows)]
    console: bool,
    #[cfg(windows)]
    pending: [u8; 3],
    #[cfg(windows)]
    pending_len: usize,
}

impl ConsoleOutput {
    fn new() -> Self {
        Self {
            write: output::write_stdout,
            #[cfg(windows)]
            console: io::stdout().is_terminal(),
            #[cfg(windows)]
            pending: [0; 3],
            #[cfg(windows)]
            pending_len: 0,
        }
    }

    fn stderr() -> Self {
        Self {
            write: output::write_stderr,
            #[cfg(windows)]
            console: io::stderr().is_terminal(),
            #[cfg(windows)]
            pending: [0; 3],
            #[cfg(windows)]
            pending_len: 0,
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        #[cfg(windows)]
        if self.console {
            let combined;
            let bytes = if self.pending_len == 0 {
                data
            } else {
                combined = [&self.pending[..self.pending_len], data].concat();
                combined.as_slice()
            };
            // Only a final incomplete sequence crosses reads; other invalid bytes render now.
            let incomplete =
                bytes.utf8_chunks().last().map_or(0, |chunk| {
                    match std::str::from_utf8(chunk.invalid()) {
                        Err(error) if error.error_len().is_none() => chunk.invalid().len(),
                        _ => 0,
                    }
                });
            let ready = bytes.len() - incomplete;
            (self.write)(String::from_utf8_lossy(&bytes[..ready]).as_bytes())?;
            self.pending[..incomplete].copy_from_slice(&bytes[ready..]);
            self.pending_len = incomplete;
            return Ok(());
        }
        (self.write)(data)
    }

    fn finish(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        if self.pending_len != 0 {
            (self.write)("\u{fffd}".as_bytes())?;
            self.pending_len = 0;
        }
        Ok(())
    }
}

struct ForegroundOutput<'a> {
    log: &'a mut File,
    console: Option<Box<dyn Transport>>,
    stdout: ConsoleOutput,
    stderr: ConsoleOutput,
}

impl<'a> ForegroundOutput<'a> {
    fn new(log: &'a mut File) -> Self {
        Self {
            log,
            console: None,
            stdout: ConsoleOutput::new(),
            stderr: ConsoleOutput::stderr(),
        }
    }

    fn forward(&mut self) -> Result<bool> {
        let mut buffer = [0; 8192];
        let count = self.log.read(&mut buffer).map_err(Exit::from_os_error)?;
        let mut progressed = count != 0;
        self.stderr
            .write_all(&buffer[..count])
            .map_err(Exit::from_os_error)?;
        if let Some(transport) = &mut self.console {
            let timeout = if progressed {
                Duration::ZERO
            } else {
                Duration::from_millis(10)
            };
            match transport.read(&mut buffer, Some(timeout)) {
                Ok(0) => {
                    self.console = None;
                    self.stdout.finish().map_err(Exit::from_os_error)?;
                }
                Ok(count) => {
                    self.stdout
                        .write_all(&buffer[..count])
                        .map_err(Exit::from_os_error)?;
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    self.console = None;
                    self.stdout.finish().map_err(Exit::from_os_error)?;
                }
                Err(error) => return Err(Exit::from_os_error(error)),
            }
        }
        Ok(progressed)
    }

    fn drain(&mut self) -> Result {
        while self.forward()? {}
        Ok(())
    }

    fn finish(&mut self) -> Result {
        self.drain()?;
        self.stdout.finish().map_err(Exit::from_os_error)?;
        self.stderr.finish().map_err(Exit::from_os_error)
    }
}

/// Keeps both boot streams moving during the synchronous control handshake.
pub(super) fn attach_with_output<T: Send>(
    attach: impl FnOnce(AttachmentCancellation) -> Result<T> + Send,
    mut forward: impl FnMut() -> Result<bool>,
) -> Result<T> {
    thread::scope(|scope| {
        let cancellation = AttachmentCancellation::default();
        let pending = cancellation.clone();
        let attaching = thread::Builder::new()
            .name("nvx-run-attach".to_owned())
            .spawn_scoped(scope, move || attach(pending))
            .map_err(Exit::from_os_error)?;
        let forwarded: Result = (|| {
            while !attaching.is_finished() {
                check_interrupt()?;
                if !forward()? {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            Ok(())
        })();
        if let Err(error) = forwarded {
            let mut cancel_failure = None;
            while !attaching.is_finished() {
                if let Err(failure) = cancellation.cancel() {
                    cancel_failure.get_or_insert(failure);
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = attaching
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            if !platform::interrupted()
                && let Some(failure) = cancel_failure
            {
                Exit::from_os_error(failure).report();
            }
            return Err(error);
        }
        let attached = attaching
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        if attached.is_err() {
            let deadline = Instant::now() + Duration::from_millis(250);
            loop {
                match forward() {
                    Ok(true) if Instant::now() < deadline => {}
                    Ok(_) => break,
                    Err(error) => {
                        if !platform::interrupted()
                            && let Err(attach) = attached
                        {
                            attach.report();
                        }
                        return Err(error);
                    }
                }
            }
        }
        attached
    })
}

fn foreground(
    launched: &mut vm::Launched,
    output: &mut ForegroundOutput<'_>,
    managed: bool,
    control: &mut Option<Session>,
) -> Result<i32> {
    let _terminal = TerminalGuard::new()?;
    let endpoint = launched
        .outputs
        .console
        .clone()
        .expect("a cold run requests a console listener");
    let input = if let Some(console) = connect_running(launched, &endpoint)? {
        let (console, input) = console_input(console)?;
        output.console = Some(console);
        Some(input)
    } else {
        None
    };
    let mut attach_closed = None;
    if managed && output.console.is_some() {
        let endpoint = launched.outputs.endpoint.clone();
        let capability = *launched.capability.as_bytes();
        let process = &launched.process;
        match attach_with_output(
            |cancellation| {
                Session::connect_launched_cancellable(
                    &endpoint,
                    process,
                    &capability,
                    60.0,
                    cancellation,
                )
            },
            || output.forward(),
        ) {
            Ok(session) => *control = Some(session),
            Err(Exit::EndpointClosed) => {
                attach_closed = Some(Instant::now() + Duration::from_secs(60));
            }
            Err(error) => return Err(error),
        }
    }
    loop {
        check_interrupt()?;
        let progressed = output.forward()?;
        let status = launched
            .wait_status(Duration::ZERO)
            .map_err(Exit::from_os_error)?;
        if let Some(input) = &input
            && status.is_none()
            && let Ok(result) = input.try_recv()
        {
            result.map_err(Exit::from_os_error)?;
        }
        if let Some(status) = status {
            output.finish()?;
            return Ok(status);
        }
        if attach_closed.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(Exit::EndpointClosed);
        }
        if !progressed {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Completes stopped output after the caller has published the requested native report.
fn finish_foreground(output: &mut ForegroundOutput<'_>, status: Result<i32>) -> Result<i32> {
    check_interrupt()?;
    let drained = output.finish();
    let status = match (status, drained) {
        (Err(error), Err(drain)) => {
            check_interrupt()?;
            drain.report();
            Err(error)
        }
        (status, Ok(())) => status,
        (Ok(_), Err(error)) => Err(error),
    };
    check_interrupt()?;
    status
}

fn run_cold(
    dir: ArtifactDir,
    openvmm: std::path::PathBuf,
    options: &Options,
    rules: &NativeRules,
    hold: Hold,
) -> Result {
    let kernel = dir.require_kernel()?;
    let initramfs = dir.require_initramfs(options.guest)?;
    let config = cold_config(options, rules)?;
    if !options.dry_run
        && let Some(report) = &options.outcome_report
    {
        validate_outcome_destination(report)?;
    }
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|error| {
        Exit::error(
            ErrorCode::Io,
            format!("cannot name the temporary VM: {error}"),
        )
    })?;
    let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    // Cold CLI runs neither capture nor reattach; this per-invocation identity is not persisted.
    let identity = ArtifactIdentity::new(format!("unrecorded-run:{name}"))?;
    let runtime = Runtime {
        openvmm,
        kernel,
        initramfs,
        identity: RuntimeIdentity::new(identity.clone(), identity.clone(), identity),
        hypervisor: hypervisor(options.hypervisor).parse()?,
        cpu_profile: options.cpu_profile.clone(),
        windows_breakaway: false,
        options: LaunchOptions::default(),
    };
    let path = std::env::temp_dir().join(format!("nvx-run-{name}"));
    if path.try_exists().map_err(Exit::from_os_error)? {
        return Err(Exit::error(
            ErrorCode::Io,
            "the temporary VM directory already exists",
        ));
    }
    vm::create_private_dir(&path)?;
    let vm_dir = VmDir::open(&path)?;
    let mut cleanup = true;
    let result = (|| {
        let prepared = vm::create(&runtime, &config, None, &vm_dir)
            .map_err(error_with_cause)?
            .for_foreground();
        let command: Vec<_> = std::iter::once(&runtime.openvmm)
            .map(|path| path_string(path))
            .chain(prepared.arguments().iter().map(|arg| {
                arg.to_str().map(str::to_owned).ok_or_else(|| {
                    Exit::error(ErrorCode::Unsupported, "an OpenVMM argument is not Unicode")
                })
            }))
            .collect::<Result<_>>()?;
        output::print_stdout(&format!(">> {}", format_command(&command)))
            .map_err(Exit::from_os_error)?;
        if options.dry_run {
            return Ok(());
        }
        let mut log = File::open(&prepared.outputs().log).map_err(Exit::from_os_error)?;
        if platform::opened_file_id(&log).map_err(Exit::from_os_error)?
            != platform::opened_file_id(prepared.log()).map_err(Exit::from_os_error)?
        {
            return Err(Exit::error(
                ErrorCode::Io,
                "the VM log was replaced before its reader opened",
            ));
        }
        let mut launched = vm::spawn(prepared).map_err(|error| {
            cleanup = error.kind() != crate::ErrorKind::StillRunning;
            error_with_cause(error)
        })?;
        cleanup = false;
        drop(hold);
        let mut control = None;
        let managed = config.boot.as_ref().is_some_and(|boot| {
            boot.command_line
                .split_ascii_whitespace()
                .any(|token| token == "nvx_mode=managed")
        });
        let mut output = ForegroundOutput::new(&mut log);
        let status = foreground(&mut launched, &mut output, managed, &mut control);
        let now = Instant::now();
        let mut graceful_error = None;
        let stopped = vm::stop(
            &launched.process,
            &launched.outputs,
            |deadline| {
                let timeout = deadline.saturating_duration_since(Instant::now());
                if timeout.is_zero() {
                    return false;
                }
                control
                    .as_mut()
                    .is_some_and(|session| match session.stop(timeout.as_secs_f64()) {
                        Ok(()) => true,
                        Err(error) => {
                            graceful_error = Some(error);
                            false
                        }
                    })
            },
            now + Duration::from_millis(250),
            now + Duration::from_secs(10),
        )
        .map_err(error_with_cause)?;
        cleanup = true;
        if let Some(error) = graceful_error {
            output::print_stderr(&format!("VM graceful stop failed: {error:?}"));
        }
        if let Some(report) = &options.outcome_report
            && let Err(error) = copy_openvmm_outcome(&launched.outputs.outcome, report)
        {
            error.report();
        }
        let status = finish_foreground(&mut output, status)?;
        if let Err(error) = stopped.report {
            output::print_stderr(&format!(
                "error[io]: {}",
                error_with_cause(error)
                    .message()
                    .unwrap_or("cannot read VM outcome")
            ));
        }
        if status != 0
            && let Some(guidance) = cpu_profile::guidance(
                options.cpu_profile.as_deref(),
                HostCpu::current().as_ref(),
                &format_command(&command[..1]),
                hypervisor(options.hypervisor),
            )
        {
            check_interrupt()?;
            output::print_stderr(&guidance);
        }
        Err(Exit::Status(status))
    })();
    if cleanup && let Err(error) = remove_vm_directory(&vm_dir) {
        output::print_stderr(&format!(
            "cannot remove the temporary VM directory {}: {error:?}",
            vm_dir.path().display()
        ));
        if result.is_ok() || matches!(result, Err(Exit::Status(0))) {
            return Err(error);
        }
    }
    result
}

#[cfg(test)]
mod tests;
