//! `nvx-rs sandbox`: `nvx.py sandbox`, ported from `command_sandbox` and the `sandbox` parser of
//! `scripts/nvx.py`, with its options, defaults, checks, outputs, and exit statuses.
//!
//! - `run`, the default, boots a one-shot sandbox: it prints the OpenVMM command as `>> COMMAND`,
//!   then runs it on the terminal, unless `--dry-run`, and exits with OpenVMM's status.
//! - `provision`, `start`, `exec`, `stop`, and `deprovision` will manage a persistent sandbox in
//!   its `--state-dir`.
//!
//! The parser reads the options of `nvx.py sandbox` with their names, defaults, types, and
//! choices, and fails as `argparse` fails, with status 2. The `--mount-deny`, `--mount-allow`,
//! and `--mount-write` options name paths of the `--mount` before them.

use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

use super::args::{Parser, Receiver, Spec, invalid_choice, invalid_value};
use super::artifacts::{Located, locate};
use super::{Exit, Result, output};
use crate::host::artifacts::{Guest, KERNEL_NAME};
use crate::host::network::policy::compile_policy_file;
use crate::host::platform;
use crate::host::python::builtins::{Int, parse_float};
use crate::host::python::path::PurePath;
use crate::host::python::shell::format_command;
use crate::host::sandbox::mount::{MountOwner, PolicyKind};
use crate::host::sandbox::options::{
    PolicyPathOption, SandboxOperation, SandboxOptions, sandbox_mounts,
    validate_sandbox_systemd_policy,
};
use crate::host::sandbox::{
    DEFAULT_ENTRYPOINT, DEFAULT_HOSTNAME, DEFAULT_WORKLOAD_IDENTITY, SandboxLaunch, SandboxLayer,
    SandboxSettings, parse_workload_identity, require_file,
};

/// The name of the parser in usage messages.
const PROG: &str = "nvx-rs sandbox";
/// The usage of the parser.
const USAGE: &str = "\
usage: nvx-rs sandbox [-h] [--layer ROLE,PATH,EROFS_UUID] [--scratch SCRATCH]
                      [--state-dir STATE_DIR] [--entrypoint ENTRYPOINT]
                      [--arg SANDBOX_ARG] [--hostname HOSTNAME]
                      [--workload-user UID:GID] [--memory-max MEMORY_MAX]
                      [--pids-max PIDS_MAX] [--memory-mib MEMORY_MIB]
                      [--timeout TIMEOUT] [--exec-timeout-ms EXEC_TIMEOUT_MS]
                      [--cwd CWD] [--environment KEY=VALUE]
                      [--environment-file PATH]
                      [--inherit-default-environment]
                      [--hypervisor {auto,whp,kvm,mshv}]
                      [--mount GUEST_TARGET,HOST_PATH[,ro|rw]]
                      [--mount-deny HOST_PATH] [--mount-allow HOST_PATH]
                      [--mount-write HOST_PATH] [--mount-owner {vmm,caller}]
                      [--net IPV4/PREFIX] [--network-profile {portable}]
                      [--network-egress {allow,deny}]
                      [--network-ingress {allow,deny}]
                      [--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]]
                      [--network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]]
                      [--network-egress-policy-file PATH]
                      [--host-loopback {allow,deny}]
                      [--network-proxy IPV4:TCP-PORT]
                      [--host-loopback-forward HOST_LOOPBACK_FORWARD]
                      [--outcome-report OUTCOME_REPORT] [--cmdline CMDLINE]
                      [--dry-run]
                      [{run,provision,start,exec,stop,deprovision}]";
/// The help of the parser, after its usage.
const HELP: &str = concat!(
    "\
usage: nvx-rs sandbox [-h] [--layer ROLE,PATH,EROFS_UUID] [--scratch SCRATCH]
                      [--state-dir STATE_DIR] [--entrypoint ENTRYPOINT]
                      [--arg SANDBOX_ARG] [--hostname HOSTNAME]
                      [--workload-user UID:GID] [--memory-max MEMORY_MAX]
                      [--pids-max PIDS_MAX] [--memory-mib MEMORY_MIB]
                      [--timeout TIMEOUT] [--exec-timeout-ms EXEC_TIMEOUT_MS]
                      [--cwd CWD] [--environment KEY=VALUE]
                      [--environment-file PATH]
                      [--inherit-default-environment]
                      [--hypervisor {auto,whp,kvm,mshv}]
                      [--mount GUEST_TARGET,HOST_PATH[,ro|rw]]
                      [--mount-deny HOST_PATH] [--mount-allow HOST_PATH]
                      [--mount-write HOST_PATH] [--mount-owner {vmm,caller}]
                      [--net IPV4/PREFIX] [--network-profile {portable}]
                      [--network-egress {allow,deny}]
                      [--network-ingress {allow,deny}]
                      [--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]]
                      [--network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]]
                      [--network-egress-policy-file PATH]
                      [--host-loopback {allow,deny}]
                      [--network-proxy IPV4:TCP-PORT]
                      [--host-loopback-forward HOST_LOOPBACK_FORWARD]
                      [--outcome-report OUTCOME_REPORT] [--cmdline CMDLINE]
                      [--dry-run]
                      [{run,provision,start,exec,stop,deprovision}]
",
    "
positional arguments:
  {run,provision,start,exec,stop,deprovision}

options:
  -h, --help            show this help message and exit
  --layer ROLE,PATH,EROFS_UUID
  --scratch SCRATCH
  --state-dir STATE_DIR
  --entrypoint ENTRYPOINT
  --arg SANDBOX_ARG
  --hostname HOSTNAME
  --workload-user UID:GID
                        fixed non-root workload identity (default:
                        65534:65534)
  --memory-max MEMORY_MAX
  --pids-max PIDS_MAX
  --memory-mib MEMORY_MIB
  --timeout TIMEOUT     control operation timeout, and the longest wait for
                        another lifecycle transition of the sandbox, in
                        seconds (default: 60)
  --exec-timeout-ms EXEC_TIMEOUT_MS
                        guest workload timeout in milliseconds; zero disables
                        it
  --cwd CWD             absolute guest working directory for managed exec
                        (default: /)
  --environment KEY=VALUE
                        set the exact managed exec environment; repeat for
                        multiple entries
  --environment-file PATH
                        read the exact managed exec environment from a JSON
                        string array
  --inherit-default-environment
                        layer the managed exec environment over the guest
                        default environment instead of replacing it
  --hypervisor {auto,whp,kvm,mshv}
  --mount GUEST_TARGET,HOST_PATH[,ro|rw]
                        live-share a host directory, or a single regular host
                        file without its directory, inside the container
                        rootfs; the mode follows the last comma, so HOST_PATH
                        may contain commas when the mode is given; repeat to
                        attach more shares, each with its own target and mode,
                        as children of one virtio-fs device
  --mount-deny HOST_PATH
                        hide one existing path inside a --mount host
                        directory; with several --mount options, it applies to
                        the --mount before it
  --mount-allow HOST_PATH
                        expose one existing path inside a --mount-deny path of
                        the same share again; the hidden directories on the
                        way list only it
  --mount-write HOST_PATH
                        make one existing path one of the only writable parts
                        of a read-write --mount; the rest of the share is
                        read-only
  --mount-owner {vmm,caller}
                        host identity of the share's file operations: vmm
                        (default) or caller, the workload identity with guest
                        root squashed to the owner of the shared directory or
                        file (Linux only)
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
                        load bounded IPv4 and IPv6 ranges and rule-local
                        exclusions for run/provision
  --host-loopback {allow,deny}
  --network-proxy IPV4:TCP-PORT
  --host-loopback-forward HOST_LOOPBACK_FORWARD
  --outcome-report OUTCOME_REPORT
                        write a bounded local JSON outcome report for run or
                        exec
  --cmdline CMDLINE
  --dry-run             print the one-shot run command without launching it"
);

/// The operations, in the order in which usage messages list them.
const OPERATIONS: [&str; 6] = ["run", "provision", "start", "exec", "stop", "deprovision"];
/// The choices of `--hypervisor`.
const HYPERVISORS: [&str; 4] = ["auto", "whp", "kvm", "mshv"];
/// The choices of `--mount-owner`.
const MOUNT_OWNERS: [&str; 2] = ["vmm", "caller"];
/// The choices of `--network-profile`.
const NETWORK_PROFILES: [&str; 1] = ["portable"];
/// The choices of `--network-egress`, `--network-ingress`, and `--host-loopback`.
const ACCESS: [&str; 2] = ["allow", "deny"];

/// The options of `nvx-rs sandbox`, which parse with the defaults of `nvx.py sandbox`.
const OPTIONS: [Spec; 35] = [
    Spec::one("--layer"),
    Spec::one("--scratch"),
    Spec::one("--state-dir"),
    Spec::one("--entrypoint"),
    Spec::one("--arg"),
    Spec::one("--hostname"),
    Spec::one("--workload-user"),
    Spec::one("--memory-max"),
    Spec::one("--pids-max"),
    Spec::one("--memory-mib"),
    Spec::one("--timeout"),
    Spec::one("--exec-timeout-ms"),
    Spec::one("--cwd"),
    Spec::one("--environment"),
    Spec::one("--environment-file"),
    Spec::flag("--inherit-default-environment"),
    Spec::one("--hypervisor"),
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
    Spec::flag("--dry-run"),
];

/// The parsed options of `nvx-rs sandbox`: the namespace of `nvx.py sandbox`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Options {
    /// The operation, `run` by default.
    pub(crate) operation: SandboxOperation,
    /// `--layer`, parsed as `SandboxLayer.parse` parses it.
    pub(crate) layers: Vec<SandboxLayer>,
    /// `--scratch`, as given.
    pub(crate) scratch: Option<String>,
    /// `--state-dir`, as given.
    pub(crate) state_dir: Option<String>,
    /// `--entrypoint`.
    pub(crate) entrypoint: String,
    /// The `--arg` options.
    pub(crate) args: Vec<String>,
    /// `--hostname`.
    pub(crate) hostname: String,
    /// `--workload-user`.
    pub(crate) workload_user: (u32, u32),
    /// `--memory-max`.
    pub(crate) memory_max: Option<Int>,
    /// `--pids-max`.
    pub(crate) pids_max: Option<Int>,
    /// `--memory-mib`, 256 by default.
    pub(crate) memory_mib: Int,
    /// `--timeout` in seconds, 60 by default.
    pub(crate) timeout: f64,
    /// `--exec-timeout-ms`, 0 by default.
    pub(crate) exec_timeout_ms: Int,
    /// `--cwd`.
    pub(crate) cwd: Option<String>,
    /// The `--environment` options.
    pub(crate) environment: Vec<String>,
    /// `--environment-file`, as given.
    pub(crate) environment_file: Option<String>,
    /// `--inherit-default-environment`.
    pub(crate) inherit_default_environment: bool,
    /// `--hypervisor`, `auto` by default.
    pub(crate) hypervisor: &'static str,
    /// The `--mount` options.
    pub(crate) mounts: Vec<String>,
    /// The `--mount-deny`, `--mount-allow`, and `--mount-write` options, with the `--mount`
    /// before each.
    pub(crate) policy_paths: Vec<PolicyPathOption>,
    /// `--mount-owner`.
    pub(crate) mount_owner: Option<MountOwner>,
    /// `--net`.
    pub(crate) net: Option<String>,
    /// `--network-profile`.
    pub(crate) network_profile: Option<&'static str>,
    /// `--network-egress`.
    pub(crate) network_egress: Option<&'static str>,
    /// `--network-ingress`.
    pub(crate) network_ingress: Option<&'static str>,
    /// The `--network-egress-allow` options.
    pub(crate) network_egress_allow: Vec<String>,
    /// The `--network-egress-deny` options.
    pub(crate) network_egress_deny: Vec<String>,
    /// `--network-egress-policy-file`, as given.
    pub(crate) network_egress_policy_file: Option<String>,
    /// `--host-loopback`.
    pub(crate) host_loopback: Option<&'static str>,
    /// `--network-proxy`.
    pub(crate) network_proxy: Option<String>,
    /// The `--host-loopback-forward` options.
    pub(crate) host_loopback_forward: Vec<String>,
    /// `--outcome-report`, as given.
    pub(crate) outcome_report: Option<String>,
    /// `--cmdline`, empty by default.
    pub(crate) cmdline: String,
    /// `--dry-run`.
    pub(crate) dry_run: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            operation: SandboxOperation::Run,
            layers: Vec::new(),
            scratch: None,
            state_dir: None,
            entrypoint: DEFAULT_ENTRYPOINT.to_owned(),
            args: Vec::new(),
            hostname: DEFAULT_HOSTNAME.to_owned(),
            workload_user: DEFAULT_WORKLOAD_IDENTITY,
            memory_max: None,
            pids_max: None,
            memory_mib: Int::from(256u32),
            timeout: 60.0,
            exec_timeout_ms: Int::zero(),
            cwd: None,
            environment: Vec::new(),
            environment_file: None,
            inherit_default_environment: false,
            hypervisor: "auto",
            mounts: Vec::new(),
            policy_paths: Vec::new(),
            mount_owner: None,
            net: None,
            network_profile: None,
            network_egress: None,
            network_ingress: None,
            network_egress_allow: Vec::new(),
            network_egress_deny: Vec::new(),
            network_egress_policy_file: None,
            host_loopback: None,
            network_proxy: None,
            host_loopback_forward: Vec::new(),
            outcome_report: None,
            cmdline: String::new(),
            dry_run: false,
        }
    }
}

/// Returns the choice that `value` names among `choices`, as `argparse` checks `choices`.
fn choice(value: &str, choices: &[&'static str]) -> std::result::Result<&'static str, String> {
    choices
        .iter()
        .find(|choice| **choice == value)
        .copied()
        .ok_or_else(|| invalid_choice(value, choices))
}

/// Returns `int(value)` for an option of `type=int`.
fn integer(value: &str) -> std::result::Result<Int, String> {
    Int::parse(value).map_err(|_| invalid_value("int", value))
}

impl Receiver for Options {
    fn take(&mut self, name: &'static str, value: Option<&str>) -> std::result::Result<(), String> {
        let text = || value.unwrap_or_default().to_owned();
        match name {
            "sandbox_operation" => {
                let operation = choice(value.unwrap_or("run"), &OPERATIONS)?;
                self.operation = match operation {
                    "provision" => SandboxOperation::Provision,
                    "start" => SandboxOperation::Start,
                    "exec" => SandboxOperation::Exec,
                    "stop" => SandboxOperation::Stop,
                    "deprovision" => SandboxOperation::Deprovision,
                    _ => SandboxOperation::Run,
                };
            }
            "--layer" => self
                .layers
                .push(SandboxLayer::parse(&text()).map_err(|error| error.message().to_owned())?),
            "--scratch" => self.scratch = Some(text()),
            "--state-dir" => self.state_dir = Some(text()),
            "--entrypoint" => self.entrypoint = text(),
            "--arg" => self.args.push(text()),
            "--hostname" => self.hostname = text(),
            "--workload-user" => {
                self.workload_user =
                    parse_workload_identity(&text()).map_err(|error| error.message().to_owned())?;
            }
            "--memory-max" => self.memory_max = Some(integer(&text())?),
            "--pids-max" => self.pids_max = Some(integer(&text())?),
            "--memory-mib" => self.memory_mib = integer(&text())?,
            "--timeout" => {
                let timeout =
                    parse_float(&text()).ok_or_else(|| invalid_value("positive_float", &text()))?;
                if !timeout.is_finite() || timeout <= 0.0 {
                    return Err("must be finite and greater than 0".to_owned());
                }
                self.timeout = timeout;
            }
            "--exec-timeout-ms" => self.exec_timeout_ms = integer(&text())?,
            "--cwd" => self.cwd = Some(text()),
            "--environment" => self.environment.push(text()),
            "--environment-file" => self.environment_file = Some(text()),
            "--inherit-default-environment" => self.inherit_default_environment = true,
            "--hypervisor" => self.hypervisor = choice(&text(), &HYPERVISORS)?,
            "--mount" => self.mounts.push(text()),
            "--mount-deny" | "--mount-allow" | "--mount-write" => {
                let kind = match name {
                    "--mount-deny" => PolicyKind::Denied,
                    "--mount-allow" => PolicyKind::Allowed,
                    _ => PolicyKind::Writable,
                };
                self.policy_paths.push(PolicyPathOption {
                    kind,
                    preceding_mount: self.mounts.len().checked_sub(1),
                    path: text(),
                });
            }
            "--mount-owner" => {
                self.mount_owner = Some(match choice(&text(), &MOUNT_OWNERS)? {
                    "caller" => MountOwner::Caller,
                    _ => MountOwner::Vmm,
                });
            }
            "--net" => self.net = Some(text()),
            "--network-profile" => self.network_profile = Some(choice(&text(), &NETWORK_PROFILES)?),
            "--network-egress" => self.network_egress = Some(choice(&text(), &ACCESS)?),
            "--network-ingress" => self.network_ingress = Some(choice(&text(), &ACCESS)?),
            "--network-egress-allow" => self.network_egress_allow.push(text()),
            "--network-egress-deny" => self.network_egress_deny.push(text()),
            "--network-egress-policy-file" => self.network_egress_policy_file = Some(text()),
            "--host-loopback" => self.host_loopback = Some(choice(&text(), &ACCESS)?),
            "--network-proxy" => self.network_proxy = Some(text()),
            "--host-loopback-forward" => self.host_loopback_forward.push(text()),
            "--outcome-report" => self.outcome_report = Some(text()),
            "--cmdline" => self.cmdline = text(),
            "--dry-run" => self.dry_run = true,
            _ => unreachable!("the parser knows only these options"),
        }
        Ok(())
    }
}

/// Parses the arguments of `sandbox`, and returns the options and the arguments that the
/// parser does not recognize.
pub(super) fn parse(arguments: &[String]) -> Result<(Options, Vec<String>)> {
    let mut options = Options::default();
    let unrecognized = Parser::new(&OPTIONS, &[Spec::optional("sandbox_operation")])
        .parse(arguments, &mut options)
        .map_err(|stop| Exit::from_stop(stop, PROG, USAGE, HELP))?;
    Ok((options, unrecognized))
}

/// Returns `_hypervisor(selected)`: `auto` selects `whp` on Windows and `kvm` elsewhere.
fn hypervisor(selected: &'static str) -> &'static str {
    match selected {
        "auto" if cfg!(windows) => "whp",
        "auto" => "kvm",
        other => other,
    }
}

/// Returns `str(Path(value))`.
fn path_text(value: &str) -> String {
    PurePath::native(value).as_str().to_owned()
}

/// Returns `path` as text, which the commands of `nvx.py` hold as `str`.
pub(crate) fn path_string(path: &std::path::Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Exit::error(format!("path is not valid Unicode: {}", path.display())))
}

/// The rules of the egress policy of a launch.
struct EgressRules {
    /// The `--network-egress-allow` rules.
    allow: Vec<String>,
    /// The `--network-egress-deny` rules.
    deny: Vec<String>,
}

/// Returns `_resolve_network_egress_rules(args)`: the explicit rules, or those that the
/// `--network-egress-policy-file` compiles to, which neither rule option nor a missing
/// `--network-egress` may accompany.
fn resolve_network_egress_rules(options: &Options) -> Result<EgressRules> {
    let Some(policy) = &options.network_egress_policy_file else {
        return Ok(EgressRules {
            allow: options.network_egress_allow.clone(),
            deny: options.network_egress_deny.clone(),
        });
    };
    if !options.network_egress_allow.is_empty() || !options.network_egress_deny.is_empty() {
        return Err(Exit::error(
            "--network-egress-policy-file cannot be combined with --network-egress-allow or \
             --network-egress-deny",
        ));
    }
    if options.network_egress.is_none() {
        return Err(Exit::error(
            "--network-egress is required with --network-egress-policy-file",
        ));
    }
    let compiled = compile_policy_file(std::path::Path::new(policy))?;
    Ok(EgressRules {
        allow: compiled.allow,
        deny: compiled.deny,
    })
}

/// Appends the network options of `options` to `command`, as `_extend_network_arguments` does.
fn extend_network_arguments(command: &mut Vec<String>, options: &Options, rules: &EgressRules) {
    let mut push = |name: &str, value: &str| {
        command.push(name.to_owned());
        command.push(value.to_owned());
    };
    if let Some(net) = &options.net {
        push("--net", net);
        push(
            "--network-profile",
            options.network_profile.unwrap_or_default(),
        );
    }
    if let Some(egress) = options.network_egress {
        push("--network-egress", egress);
    }
    if let Some(ingress) = options.network_ingress {
        push("--network-ingress", ingress);
    }
    for rule in &rules.allow {
        push("--network-egress-allow", rule);
    }
    for rule in &rules.deny {
        push("--network-egress-deny", rule);
    }
    if let Some(host_loopback) = options.host_loopback {
        push("--host-loopback", host_loopback);
    }
    if let Some(proxy) = &options.network_proxy {
        push("--network-proxy", proxy);
    }
    for forward in &options.host_loopback_forward {
        push("--host-loopback-forward", forward);
    }
}

/// The artifacts of a sandbox, found when an operation first needs them.
pub(crate) struct Artifacts<'a> {
    /// `--root`, if given.
    root: Option<&'a str>,
}

impl Artifacts<'_> {
    /// Returns the OpenVMM executable, kernel, and Alpine initramfs, checked in that order as
    /// `nvx.py sandbox run` and `start` check them.
    pub(crate) fn require(&self) -> Result<[String; 3]> {
        let Located { dir, openvmm } = locate(self.root)?;
        require_file(&openvmm, "OpenVMM release binary")?;
        let kernel = dir.artifact(KERNEL_NAME);
        require_file(&kernel, "Linux direct kernel")?;
        let initrd = dir.initramfs(Guest::Alpine);
        require_file(&initrd, "initramfs")?;
        Ok([
            path_string(&openvmm)?,
            path_string(&kernel)?,
            path_string(&initrd)?,
        ])
    }
}

/// Runs the operation of `options`, as `command_sandbox` does.
pub(super) fn run(root: Option<&str>, options: Options) -> Result {
    let operation = options.operation;
    let checks = SandboxOptions {
        operation,
        dry_run: options.dry_run,
        cwd: options.cwd.clone(),
        environment: options.environment.clone(),
        environment_file: options.environment_file.clone(),
        inherit_default_environment: options.inherit_default_environment,
        network_options: options.net.is_some()
            || options.network_profile.is_some()
            || options.network_egress.is_some()
            || options.network_ingress.is_some()
            || options.network_egress_policy_file.is_some()
            || options.host_loopback.is_some()
            || options.network_proxy.is_some()
            || !options.network_egress_allow.is_empty()
            || !options.network_egress_deny.is_empty()
            || !options.host_loopback_forward.is_empty(),
        net: options.net.is_some(),
        network_profile: options.network_profile.is_some(),
        entrypoint: options.entrypoint.clone(),
        outcome_report: options.outcome_report.is_some(),
        mounts: options.mounts.clone(),
        policy_paths: options.policy_paths.clone(),
        mount_owner: options.mount_owner,
        layers: !options.layers.is_empty(),
        scratch: options.scratch.is_some(),
    };
    checks.check()?;
    let launch = if matches!(
        operation,
        SandboxOperation::Run | SandboxOperation::Provision
    ) {
        let rules = resolve_network_egress_rules(&options)?;
        let mounts = sandbox_mounts(&options.mounts, &options.policy_paths, options.mount_owner)?;
        let settings = SandboxSettings {
            entrypoint: options.entrypoint.clone(),
            args: options.args.clone(),
            hostname: options.hostname.clone(),
            workload_identity: options.workload_user,
            memory_max: options.memory_max.clone(),
            pids_max: options.pids_max.clone(),
            mounts,
        };
        let launch = SandboxLaunch::new(
            options.layers.clone(),
            options.scratch.as_deref().unwrap_or_default(),
            settings,
        )?;
        launch.validated()?;
        validate_sandbox_systemd_policy(&launch)?;
        Some((launch, rules))
    } else {
        None
    };
    let artifacts = Artifacts { root };
    match (operation, launch) {
        (SandboxOperation::Run, Some((launch, rules))) => {
            if options.state_dir.is_some() {
                return Err(Exit::error(
                    "one-shot sandbox execution rejects persistent --state-dir settings",
                ));
            }
            let [openvmm, kernel, initrd] = artifacts.require()?;
            let mut command = vec![openvmm];
            command.extend(launch.openvmm_arguments()?);
            command.extend(
                [
                    "--microvm-lifecycle",
                    "one-shot",
                    "--single-process",
                    "--hypervisor",
                    hypervisor(options.hypervisor),
                ]
                .map(str::to_owned),
            );
            command.extend([
                "--memory".to_owned(),
                format!("{}M", options.memory_mib),
                "--kernel".to_owned(),
                kernel,
                "--initrd".to_owned(),
                initrd,
                "--cmdline".to_owned(),
                launch.kernel_command_line(&options.cmdline)?,
            ]);
            extend_network_arguments(&mut command, &options, &rules);
            if let Some(report) = &options.outcome_report {
                command.extend(["--microvm-report".to_owned(), path_text(report)]);
            }
            output::print_stdout(&format!(">> {}", format_command(&command)))
                .map_err(|error| Exit::error(output::stream_error(&error)))?;
            if options.dry_run {
                return Ok(());
            }
            Err(Exit::Status(run_openvmm(&command)?))
        }
        (SandboxOperation::Run, None) => unreachable!("run builds its launch"),
        // The lifecycle of a managed sandbox follows.
        (operation, _) => Err(Exit::error(format!(
            "sandbox {} is not supported yet",
            operation.as_str()
        ))),
    }
}

/// Runs OpenVMM on the terminal, as `subprocess.run(command)` does, and returns its status.
///
/// OpenVMM shares the terminal and its process group, so Ctrl-C reaches it too. As
/// `subprocess.run` does, the CLI then gives it 0.25 s to exit on Unix before it kills it, and on
/// Windows, where Python cannot interrupt its wait, waits for it to exit; either way the command
/// ends as interrupted.
fn run_openvmm(command: &[String]) -> Result<i32> {
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .spawn()
        .map_err(|error| Exit::error(spawn_error(&error, &command[0])))?;
    loop {
        if let Some(status) = poll(&mut child)? {
            return Ok(status);
        }
        if platform::interrupted() {
            if cfg!(windows) {
                while poll(&mut child)?.is_none() {
                    thread::sleep(Duration::from_millis(10));
                }
            } else {
                let deadline = std::time::Instant::now() + Duration::from_millis(250);
                while poll(&mut child)?.is_none() {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            return Err(Exit::Interrupted);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Returns the status of `child` if it has exited: `Popen.returncode`, which on Unix is the
/// negated signal of a process that a signal ended.
fn poll(child: &mut Child) -> Result<Option<i32>> {
    let status = child
        .try_wait()
        .map_err(|error| Exit::error(crate::host::python::fs::os_error_message(&error, None)))?;
    Ok(status.map(|status| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;

            status
                .code()
                .unwrap_or_else(|| -status.signal().unwrap_or_default())
        }
        #[cfg(not(unix))]
        status.code().unwrap_or_default()
    }))
}

/// Returns the message of the `OSError` with which `subprocess` fails to start `program`: with
/// the program's name on POSIX, and without on Windows.
pub(crate) fn spawn_error(error: &std::io::Error, program: &str) -> String {
    let program = if cfg!(windows) { None } else { Some(program) };
    crate::host::python::fs::os_error_message(error, program)
}
