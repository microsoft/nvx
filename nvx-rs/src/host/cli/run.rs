//! `nvx-rs run`: the port of the former `nvx.py run`, from `command_run` and the `run` parser of
//! `scripts/nvx.py`, with its options, defaults, checks, outputs, and exit statuses.
//!
//! It boots a guest in a microVM, or restores a microVM from a snapshot, on the terminal: it
//! prints OpenVMM's command as `>> COMMAND`, then runs OpenVMM with the standard streams, unless
//! `--dry-run` is given, and exits with OpenVMM's status. The parser reads the options of
//! `nvx.py run` with their names, defaults, types, and choices, and ends a usage error with
//! status 2.
//!
//! A cold boot forwards the terminal through the VM's boot-console listener, `hvc1`, and the
//! VM's log to standard error. Its temporary private VM directory is removed after confirmed
//! exit. A restore attaches the boot console as its snapshot had it: it reads the
//! snapshot's manifest, `manifest.bin` in the snapshot's directory, dry run or not, and passes
//! `--virtio-console console` when the manifest's machine contract holds the attachment of the
//! boot console, `console:microvm-virtio0`, with the reconnect policy
//! `require-inherited-attachment`, which a capture with `--virtio-console console` records, and
//! which OpenVMM restores only with that option again. It passes no console option otherwise, which
//! OpenVMM requires of a snapshot without a boot console, and which leaves any other boot console
//! the attachment that its snapshot records. A manifest that it cannot read fails the restore
//! before OpenVMM starts, and before anything is downloaded, since OpenVMM could not restore the
//! snapshot either; the failure follows that of a missing OpenVMM, as the shares' do.
//!
//! A cold boot takes `--net` and `--network-profile` together. A restore takes the addressing of
//! its NIC from the snapshot, as OpenVMM does, so it refuses `--net`, and passes
//! `--network-profile` alone, which OpenVMM requires, with the network policy that the snapshot
//! was captured with, to restore a snapshot that has a NIC. With `--restore-network-policy`, which
//! it passes on, the restore installs the policy of its network options instead, ingress,
//! explicit host loopback, and forwards included, which OpenVMM admits for a NIC template, a
//! snapshot whose NIC was captured with the complete deny-all policy, and the guest applies the
//! new policy's DNS server and listener announcements before the host releases its input.
//!
//! Its shares differ from those of `sandbox`. `--mount-deny`, `--mount-allow`, and `--mount-write`
//! name host paths, which must be absolute with several shares or a shared file. A cold boot
//! checks their ownership in the filesystem export plan, exports its numbered children, and adds
//! the plan's `nvx_share=` rows to the kernel command line. A restore keeps its existing lowering:
//! one directory uses `--mount`, several shares or a file use an aggregate, and no new share tokens
//! replace the binds that its saved guest already made.
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
    HYPERVISORS, MAX_MEMORY_MIB, MOUNT_OWNERS, copy_openvmm_outcome, error_with_cause, hypervisor,
    path_string, path_text, run_openvmm, sandbox_network, validate_outcome_destination,
};
use super::session::Session;
use super::shell::format_command;
use super::{Exit, Result, check_interrupt, output};
use crate::ErrorCode;
use crate::activation::{MachineIdMode, View};
use crate::host::artifacts::{ArtifactDir, Guest, require_artifact};
use crate::host::config::{Boot, ConsoleAttachment, NetworkConfig, RootConfig, VmConfig};
use crate::host::filesystem::{
    self, ExportPlan, ExportRequest, Mapping, Owner, PathList, ReservedTargets, Target,
};
use crate::host::platform::{self, Transport};
use crate::host::runtime::{ArtifactIdentity, LaunchOptions, Runtime, RuntimeIdentity};
use crate::host::sandbox::mount::{
    AGGREGATE_MOUNT_TARGET, MountAccess, MountKind, MountOwner, POLICY_KINDS, PolicyKind,
    aggregate_child_argument, aggregate_child_name, aggregate_command_line_fragment,
    aggregate_share_tokens, export_command_line_fragment, mount_argument, mount_kind,
    parse_mount_value, require_mount_owner_supported,
};
use crate::host::sandbox::options::validate_run_share_targets;
use crate::host::sandbox::{SANDBOX_COMMAND_LINE_MAX_SIZE, has_kernel_separator};
use crate::host::template::read_attachments;
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
                        ubuntu 512, azurelinux 512)
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
  --restore-snapshot RESTORE_SNAPSHOT
  --restore-processors {1,2,4,8}
  --restore-memory-mib RESTORE_MEMORY_MIB
  --restore-ready-path RESTORE_READY_PATH
  --restore-network-policy
                        install the network options' policy in place of the
                        complete deny-all policy of a snapshot's NIC
                        (doc/run.md)
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
/// The choices of `--processors` and `--restore-processors`, the processor counts that a microVM
/// takes.
const PROCESSORS: [&str; 4] = ["1", "2", "4", "8"];

/// The stable ID of the attachment of a microVM's boot console, `hvc1`, in a snapshot's machine
/// contract.
const BOOT_CONSOLE: &str = "console:microvm-virtio0";

/// The reconnect policy of a boot console that a capture attached to the terminal with
/// `--virtio-console console`, which OpenVMM restores only with `--virtio-console console` again.
const INHERITED_ATTACHMENT: &str = "require-inherited-attachment";

/// The options of `nvx-rs run`, which parse with the defaults of the former `nvx.py run`.
pub(super) const OPTIONS: [Spec; 30] = [
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
    Spec::one("--restore-processors"),
    Spec::one("--restore-memory-mib"),
    Spec::one("--restore-ready-path"),
    Spec::flag("--restore-network-policy"),
    Spec::flag("--dry-run"),
];

/// The parsed options of `nvx-rs run`: the namespace of the former `nvx.py run`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// `--restore-snapshot`, as given.
    pub(crate) restore_snapshot: Option<String>,
    /// `--restore-processors`.
    pub(crate) restore_processors: Option<u32>,
    /// `--restore-memory-mib`.
    pub(crate) restore_memory_mib: Option<u64>,
    /// `--restore-ready-path`, as given.
    pub(crate) restore_ready_path: Option<String>,
    /// `--restore-network-policy`.
    pub(crate) restore_network_policy: bool,
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
            restore_processors: None,
            restore_memory_mib: None,
            restore_ready_path: None,
            restore_network_policy: false,
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
            "--restore-processors" => self.restore_processors = Some(processors(&text())?),
            "--restore-memory-mib" => self.restore_memory_mib = Some(memory(&text())?),
            "--restore-ready-path" => self.restore_ready_path = Some(text()),
            "--restore-network-policy" => self.restore_network_policy = true,
            "--dry-run" => self.dry_run = true,
            _ => return self.network.take(name, &text()),
        }
        Ok(())
    }
}

/// Parses the arguments of `run`, and returns the options and the arguments that the parser does
/// not recognize.
pub(super) fn parse(arguments: &[String]) -> Result<(Options, Vec<String>)> {
    let mut options = Options::default();
    let unrecognized = Parser::new(&OPTIONS, &[])
        .parse(arguments, &mut options)
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX))?;
    Ok((options, unrecognized))
}

/// Appends `name` and `value` to `command`.
fn push(command: &mut Vec<String>, name: &str, value: impl Into<String>) {
    command.push(name.to_owned());
    command.push(value.into());
}

/// Checks `options` in the order of `command_run`, before anything that it looks up, except that
/// a restore refuses `--net` and takes `--network-profile` alone.
fn check(options: &Options) -> Result {
    let refused = |message: String| Err(Exit::error(ErrorCode::Unsupported, message));
    let network = &options.network;
    let restore = options.restore_snapshot.is_some();
    if restore && network.net.is_some() {
        return refused(
            "--net is not accepted for restore; the snapshot already fixes the network addressing"
                .to_owned(),
        );
    }
    if !restore && network.net.is_some() != network.network_profile.is_some() {
        return refused("--net and --network-profile must be specified together".to_owned());
    }
    for (given, option) in [
        (options.restore_ready_path.is_some(), "--restore-ready-path"),
        (options.restore_processors.is_some(), "--restore-processors"),
        (options.restore_memory_mib.is_some(), "--restore-memory-mib"),
        (options.restore_network_policy, "--restore-network-policy"),
    ] {
        if given && !restore {
            return refused(format!("{option} requires --restore-snapshot"));
        }
    }
    if options.memory_capacity_mib.is_some() && restore {
        return refused("--memory-capacity-mib is only valid for a fresh boot".to_owned());
    }
    if restore && options.guest != Guest::Alpine {
        return refused(
            "--guest is not accepted for restore; the snapshot already fixes the guest".to_owned(),
        );
    }
    if let Some(capacity) = &options.memory_capacity_mib
        && *capacity < memory_mib(options)
    {
        return refused("--memory-capacity-mib cannot be below --memory-mib".to_owned());
    }
    if let Some(restored) = &options.restore_processors
        && *restored > options.processors
    {
        return refused("--restore-processors cannot exceed --processors capacity".to_owned());
    }
    Ok(())
}

/// Returns the guest RAM in MiB: `--memory-mib`, or else the guest's default.
fn memory_mib(options: &Options) -> u64 {
    options
        .memory_mib
        .unwrap_or_else(|| u64::from(options.guest.default_memory_mib()))
}

/// Returns whether the restore of the snapshot in the directory `snapshot` attaches the boot
/// console to the terminal: whether the machine contract of the snapshot's manifest holds the
/// attachment of the boot console with the reconnect policy of one that a capture attached to the
/// terminal. OpenVMM restores such a console only with `--virtio-console console` again, and
/// refuses that option for any other snapshot.
fn restores_terminal_console(snapshot: &str) -> Result<bool> {
    let attachments =
        read_attachments(Path::new(&path_text(snapshot))).map_err(error_with_cause)?;
    // OpenVMM restores the first attachment of the boot console.
    Ok(attachments
        .iter()
        .find(|attachment| attachment.stable_id == BOOT_CONSOLE)
        .is_some_and(|attachment| attachment.reconnect_policy == INHERITED_ATTACHMENT))
}

/// Runs the microVM of `options`, as `command_run` did, once the release of a checkout without
/// artifacts is downloaded, as [`download::ensure`] decides, unless the options are refused.
pub(super) fn run(root: Option<&str>, options: Options) -> Result {
    check(&options)?;
    let rules = options.network.egress_rules()?;
    // A restore's console depends on its snapshot's manifest, which a restore that cannot read it
    // reports after a missing artifact, as the refusals of the shares and the network below are.
    let terminal_console = options
        .restore_snapshot
        .as_deref()
        .map(restores_terminal_console);
    // The shares and the network follow the artifacts on the command line, and a missing artifact
    // is reported before them, as `command_run` did, but a command that they refuse downloads
    // nothing, though it reads the artifacts under the lock of downloads all the same. Their
    // arguments are built again once the artifacts are in place, as a share's kind depends on what
    // its host path holds, which a download may install.
    let refused = if options.restore_snapshot.is_some() {
        terminal_console.as_ref().is_some_and(Result::is_err)
            || extend_shares_and_network(&mut Vec::new(), &options, &rules).is_err()
    } else {
        cold_preflight(&options, &rules).is_err()
    };
    let located = locate(root)?;
    // A restore boots no kernel or initramfs: the snapshot holds the guest.
    let guest = options.restore_snapshot.is_none().then_some(options.guest);
    let hold = if refused {
        download::hold(&located)?
    } else {
        download::ensure(&located, guest, options.hypervisor)?
    };
    let Located { dir, openvmm } = located;
    require_artifact(&openvmm, "OpenVMM release binary")?;
    if options.restore_snapshot.is_none() {
        return run_cold(dir, openvmm, &options, &rules, hold);
    }
    let mut command = vec![
        path_text(&path_string(&openvmm)?),
        "--single-process".to_owned(),
    ];
    push(&mut command, "--machine", options.machine);
    push(&mut command, "--processors", options.processors.to_string());
    push(&mut command, "--hypervisor", hypervisor(options.hypervisor));
    if let Some(snapshot) = &options.restore_snapshot {
        push(&mut command, "--restore-snapshot", path_text(snapshot));
        if let Some(processors) = &options.restore_processors {
            push(&mut command, "--restore-processors", processors.to_string());
        }
        if let Some(memory) = &options.restore_memory_mib {
            push(&mut command, "--restore-memory", format!("{memory}M"));
        }
        if let Some(ready) = &options.restore_ready_path {
            push(&mut command, "--restore-ready-path", path_text(ready));
        }
        if options.restore_network_policy {
            command.push("--restore-network-policy".to_owned());
        }
        if terminal_console.transpose()? == Some(true) {
            push(&mut command, "--virtio-console", "console");
        }
    }
    if let Some(profile) = &options.cpu_profile {
        push(&mut command, "--cpu-profile", profile.as_str());
    }
    let cmdline = extend_shares_and_network(&mut command, &options, &rules)?;
    if !cmdline.is_empty() {
        push(&mut command, "--cmdline", cmdline);
    }
    output::print_stdout(&format!(">> {}", format_command(&command)))
        .map_err(Exit::from_os_error)?;
    if options.dry_run {
        return Ok(());
    }
    let status = run_openvmm(&command, &[], hold)?;
    Err(Exit::Status(status))
}

/// Checks only options, not paths that artifact installation may create or replace.
fn cold_preflight(options: &Options, rules: &NativeRules) -> Result {
    let shares = cold_shares(options)?;
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
    if let Some(owner) = options.mount_owner {
        require_mount_owner_supported(owner)?;
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
    let network = sandbox_network(&options.network, rules)?
        .map(|network| -> Result<NetworkConfig> {
            Ok(NetworkConfig {
                guest_network: Some(
                    crate::net::Ipv4Cidr::parse(network.guest_network())
                        .map_err(|error| Exit::error(ErrorCode::Unsupported, error.to_string()))?,
                ),
                policy: network.policy().clone(),
            })
        })
        .transpose()?;
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
    connect_before_exit(
        endpoint,
        || platform::connect_endpoint(endpoint, pid, Duration::from_millis(250)),
        |timeout| launched.wait_status(timeout),
    )
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

struct TerminalGuard(bool);

impl TerminalGuard {
    fn new() -> Result<Self> {
        let terminal = io::stdin().is_terminal();
        if terminal {
            crossterm::terminal::enable_raw_mode().map_err(Exit::from_os_error)?;
        }
        Ok(Self(terminal))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.0
            && let Err(error) = crossterm::terminal::disable_raw_mode()
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
            ..Self::new()
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
            match transport.read(&mut buffer, Some(Duration::from_millis(10))) {
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
fn attach_with_output<T: Send>(
    attach: impl FnOnce() -> Result<T> + Send,
    mut forward: impl FnMut() -> Result<bool>,
) -> Result<T> {
    thread::scope(|scope| {
        let attaching = thread::Builder::new()
            .name("nvx-run-attach".to_owned())
            .spawn_scoped(scope, attach)
            .map_err(Exit::from_os_error)?;
        while !attaching.is_finished() {
            check_interrupt()?;
            if !forward()? {
                thread::sleep(Duration::from_millis(10));
            }
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
                        if let Err(attach) = attached {
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
        let child = launched.child_mut().map_err(Exit::from_os_error)?;
        match attach_with_output(
            || Session::connect_launched(&endpoint, child, &capability, 60.0),
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
        let drained = output.finish();
        let status = match (status, drained) {
            (Err(error), Err(drain)) => {
                drain.report();
                Err(error)
            }
            (status, Ok(())) => status,
            (Ok(_), Err(error)) => Err(error),
        };
        check_interrupt()?;
        let status = status?;
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

/// Appends the shares of `options`, their policy paths and owner, the network options with the
/// egress `rules`, and the outcome report to `command`, as `command_run` did, and returns the
/// kernel command line: the one of `--cmdline`, and on the cold boot of an aggregate, the
/// `nvx_share=` tokens of its children, which with OpenVMM's tokens must fit the 1024-byte budget.
fn extend_shares_and_network(
    command: &mut Vec<String>,
    options: &Options,
    rules: &NativeRules,
) -> Result<String> {
    let shares = options
        .mounts
        .iter()
        .map(|value| parse_mount_value(value))
        .collect::<crate::Result<Vec<_>>>()?;
    // A file is shared as the child of an aggregate, even alone, because the guest mounts the
    // device's root, which is a directory.
    let kinds: Vec<MountKind> = shares
        .iter()
        .map(|(_, host_path, _)| mount_kind(Path::new(host_path)))
        .collect();
    let aggregate = shares.len() > 1 || kinds.contains(&MountKind::File);
    match shares.as_slice() {
        [] => {}
        [(target, host_path, access)] if !aggregate => {
            if has_kernel_separator(target) {
                return Err(Exit::error(
                    ErrorCode::Unsupported,
                    format!(
                        "--mount target {target:?} must be one kernel token without whitespace, control characters, or quotes"
                    ),
                ));
            }
            push(
                command,
                "--mount",
                mount_argument(target, host_path, *access),
            );
        }
        _ => {
            let targets: Vec<&str> = shares
                .iter()
                .map(|(target, _, _)| target.as_str())
                .collect();
            validate_run_share_targets(&targets)?;
            push(command, "--mount-aggregate", AGGREGATE_MOUNT_TARGET);
            // A restore names the children as the boot did, so OpenVMM refuses a restore whose
            // targets differ from those that the guest bound.
            for (index, ((target, host_path, access), kind)) in
                shares.iter().zip(&kinds).enumerate()
            {
                let name = aggregate_child_name(index, target);
                let child = aggregate_child_argument(&name, host_path, *access, *kind);
                push(command, "--mount-child", child);
            }
        }
    }
    if options.mount_owner.is_some() && options.mounts.is_empty() {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "--mount-owner requires --mount",
        ));
    }
    for kind in POLICY_KINDS {
        for (_, given) in options.policy_paths.iter().filter(|(of, _)| *of == kind) {
            let path = path_text(given);
            // OpenVMM attributes each path of an aggregate to the child whose host directory
            // contains it.
            if aggregate && !Path::new(&path).is_absolute() {
                return Err(Exit::error(
                    ErrorCode::Unsupported,
                    format!(
                        "with several --mount shares, or a shared file, {} {path} must be an \
                         absolute host path",
                        kind.option(),
                    ),
                ));
            }
            push(command, kind.option(), path);
        }
    }
    if let Some(owner) = options.mount_owner {
        require_mount_owner_supported(owner)?;
        push(command, "--mount-owner", owner.as_str());
    }
    options.network.extend_arguments(command, rules);
    if let Some(report) = &options.outcome_report {
        push(command, "--microvm-report", path_text(report));
    }
    // A restored guest keeps the binds that its boot made, at the targets that its children's
    // names pin.
    if !aggregate || options.restore_snapshot.is_some() {
        return Ok(options.cmdline.clone());
    }
    let tokens = aggregate_share_tokens(
        &shares
            .iter()
            .zip(&kinds)
            .map(|((target, _, access), kind)| (target.as_str(), *access, *kind))
            .collect::<Vec<_>>(),
    );
    let cmdline = std::iter::once(options.cmdline.as_str())
        .chain(tokens.iter().map(String::as_str))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let fragment = aggregate_command_line_fragment(
        &shares
            .iter()
            .map(|(_, _, access)| *access)
            .collect::<Vec<_>>(),
    );
    if cmdline.len() + fragment.len() + 1 > SANDBOX_COMMAND_LINE_MAX_SIZE {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "the --mount shares and --cmdline exceed the 1024-byte budget of the kernel command \
             line; attach fewer shares or use shorter targets",
        ));
    }
    Ok(cmdline)
}
