//! `nvx-rs run`: the port of the former `nvx.py run`, from `command_run` and the `run` parser of
//! `scripts/nvx.py`, with its options, defaults, checks, outputs, and exit statuses.
//!
//! It boots a guest in a microVM, or restores a microVM from a snapshot, on the terminal: it
//! prints OpenVMM's command as `>> COMMAND`, then runs OpenVMM with the standard streams, unless
//! `--dry-run` is given, and exits with OpenVMM's status. The parser reads the options of
//! `nvx.py run` with their names, defaults, types, and choices, and ends a usage error with
//! status 2.
//!
//! A cold boot takes `--net` and `--network-profile` together. A restore takes the addressing of
//! its NIC from the snapshot, as OpenVMM does, so it refuses `--net`, and passes
//! `--network-profile` alone, which OpenVMM requires, with the network policy that the snapshot
//! was captured with, to restore a snapshot that has a NIC.
//!
//! Its shares differ from those of `sandbox`. `--mount-deny`, `--mount-allow`, and `--mount-write`
//! name host paths, which OpenVMM attributes to the share that holds them, and which must be
//! absolute with several shares or a shared file. One directory share uses `--mount`, several
//! shares or a file the children of an aggregate, whose `nvx_share=` tokens a cold boot adds to
//! the kernel command line, and a restore does not, since its guest keeps the binds that its boot
//! made.
//!
//! After OpenVMM exits with a status other than 0 from a cold boot, on a host whose CPU no built-in
//! CPU profile serves, it prints the guidance of [`cpu_profile`] to standard error, which says
//! what `--cpu-profile auto` does on that CPU and how to request a built-in profile for it. It
//! never prints it after a dry run, a restore, a refused option, or Ctrl-C.

use std::path::Path;

use super::args::{Parser, Receiver, Spec, Syntax, choice, integer};
use super::artifacts::{Located, locate};
use super::cpu_profile::{self, HostCpu};
use super::download;
use super::network::{NativeRules, NetworkOptions};
use super::sandbox::{
    HYPERVISORS, MAX_MEMORY_MIB, MOUNT_OWNERS, hypervisor, path_string, path_text, run_openvmm,
};
use super::shell::format_command;
use super::{Exit, Result, check_interrupt, output};
use crate::ErrorCode;
use crate::host::artifacts::{Guest, require_artifact};
use crate::host::sandbox::mount::{
    AGGREGATE_MOUNT_TARGET, MountKind, MountOwner, POLICY_KINDS, PolicyKind,
    aggregate_child_argument, aggregate_child_name, aggregate_command_line_fragment,
    aggregate_share_tokens, mount_argument, mount_kind, parse_mount_value,
    require_mount_owner_supported,
};
use crate::host::sandbox::options::validate_run_share_targets;
use crate::host::sandbox::{SANDBOX_COMMAND_LINE_MAX_SIZE, has_kernel_separator};

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

/// The options of `nvx-rs run`, which parse with the defaults of the former `nvx.py run`.
pub(super) const OPTIONS: [Spec; 29] = [
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

/// Runs the microVM of `options`, as `command_run` did, once the release of a checkout without
/// artifacts is downloaded, as [`download::ensure`] decides, unless the options are refused.
pub(super) fn run(root: Option<&str>, options: Options) -> Result {
    check(&options)?;
    let rules = options.network.egress_rules()?;
    // The shares and the network follow the artifacts on the command line, and a missing artifact
    // is reported before them, as `command_run` did, but a command that they refuse downloads
    // nothing, though it reads the artifacts under the lock of downloads all the same. Their
    // arguments are built again once the artifacts are in place, as a share's kind depends on what
    // its host path holds, which a download may install.
    let refused = extend_shares_and_network(&mut Vec::new(), &options, &rules).is_err();
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
    } else {
        let kernel = dir.require_kernel()?;
        let initrd = dir.require_initramfs(options.guest)?;
        push(
            &mut command,
            "--memory",
            format!("{}M", memory_mib(&options)),
        );
        push(&mut command, "--kernel", path_string(&kernel)?);
        push(&mut command, "--initrd", path_string(&initrd)?);
        if let Some(capacity) = &options.memory_capacity_mib {
            push(&mut command, "--memory-capacity", format!("{capacity}M"));
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
    if status != 0
        && options.restore_snapshot.is_none()
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
}

#[cfg(test)]
mod specification;

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
