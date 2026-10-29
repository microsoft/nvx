//! The parser of `nvx-rs sandbox`, with the options of the former `nvx.py sandbox`, and the
//! network that its network options describe.

use std::path::Path;

use crate::ErrorCode;
use crate::host::cli::args::{Parser, Receiver, Spec, Syntax, choice, integer};
use crate::host::cli::network::{NativeRules, NetworkOptions};
use crate::host::cli::{Exit, Result};
use crate::host::network::{
    Access, EgressRule, EgressRules, HostLoopback, LoopbackForward, NetworkPolicy, NetworkProxy,
};
use crate::host::paths;
use crate::host::sandbox::mount::{MountOwner, PolicyKind};
use crate::host::sandbox::options::{PolicyPathOption, SandboxOperation};
use crate::host::sandbox::plan::guest_gateway;
use crate::host::sandbox::{
    DEFAULT_ENTRYPOINT, DEFAULT_HOSTNAME, DEFAULT_WORKLOAD_IDENTITY, PIDS_MAX_LIMIT, SandboxLayer,
    SandboxNetwork, parse_workload_identity,
};
use crate::protocol::root::RootUpper;

/// The usage and help of the parser.
pub(in crate::host::cli) const SYNTAX: Syntax = Syntax {
    command: "sandbox",
    arguments: "[{run,provision,start,exec,stop,deprovision}] [OPTIONS]",
    help: "\
positional arguments:
  {run,provision,start,exec,stop,deprovision}

options:
  -h, --help            show this help message and exit
  --layer ROLE,PATH,EROFS_UUID
  --scratch SCRATCH
  --upper {scratch,ram}
                        where the overlay keeps the sandbox's writes: scratch
                        (default), on the ext4 image of --scratch, or ram, on
                        a tmpfs in guest memory, without a scratch image
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
  --dry-run             print the one-shot run command without launching it",
};

/// The operations, in the order in which usage messages list them.
const OPERATIONS: [&str; 6] = ["run", "provision", "start", "exec", "stop", "deprovision"];
/// The choices of `--upper`, the names of the uppers.
const UPPERS: [&str; 2] = [RootUpper::Scratch.as_str(), RootUpper::Ram.as_str()];
/// The choices of `--hypervisor`.
pub(in crate::host::cli) const HYPERVISORS: [&str; 4] = ["auto", "whp", "kvm", "mshv"];
/// The choices of `--mount-owner`.
pub(in crate::host::cli) const MOUNT_OWNERS: [&str; 2] = ["vmm", "caller"];
/// The most MiB of guest RAM that `--memory-mib` and the memory options of `run` take: the most
/// whose count of bytes fits in 64 bits, as OpenVMM reads a size of RAM.
pub(in crate::host::cli) const MAX_MEMORY_MIB: u64 = u64::MAX >> 20;

/// Returns the seconds that `value` gives, a finite number greater than 0, as Rust's `f64`
/// parses it: digits with an optional fraction and exponent, after an optional sign.
fn seconds(value: &str) -> std::result::Result<f64, String> {
    value
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .ok_or_else(|| format!("expected a finite number of seconds greater than 0, not {value:?}"))
}

/// The options of `nvx-rs sandbox`, which parse with the defaults of the former
/// `nvx.py sandbox`.
pub(in crate::host::cli) const OPTIONS: [Spec; 36] = [
    Spec::one("--layer"),
    Spec::one("--scratch"),
    Spec::one("--upper"),
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

/// The parsed options of `nvx-rs sandbox`: the namespace of the former `nvx.py sandbox`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Options {
    /// The operation, `run` by default.
    pub(crate) operation: SandboxOperation,
    /// `--layer`, parsed as `SandboxLayer.parse` parses it.
    pub(crate) layers: Vec<SandboxLayer>,
    /// `--scratch`, as given.
    pub(crate) scratch: Option<String>,
    /// `--upper`, if given; a launch keeps its upper on scratch without it.
    pub(crate) upper: Option<RootUpper>,
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
    pub(crate) memory_max: Option<u64>,
    /// `--pids-max`.
    pub(crate) pids_max: Option<u32>,
    /// `--memory-mib`, 256 by default.
    pub(crate) memory_mib: u64,
    /// `--timeout` in seconds, 60 by default.
    pub(crate) timeout: f64,
    /// `--exec-timeout-ms`, 0 by default.
    pub(crate) exec_timeout_ms: u32,
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
    /// The network options.
    pub(crate) network: NetworkOptions,
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
            upper: None,
            state_dir: None,
            entrypoint: DEFAULT_ENTRYPOINT.to_owned(),
            args: Vec::new(),
            hostname: DEFAULT_HOSTNAME.to_owned(),
            workload_user: DEFAULT_WORKLOAD_IDENTITY,
            memory_max: None,
            pids_max: None,
            memory_mib: 256,
            timeout: 60.0,
            exec_timeout_ms: 0,
            cwd: None,
            environment: Vec::new(),
            environment_file: None,
            inherit_default_environment: false,
            hypervisor: "auto",
            mounts: Vec::new(),
            policy_paths: Vec::new(),
            mount_owner: None,
            network: NetworkOptions::default(),
            outcome_report: None,
            cmdline: String::new(),
            dry_run: false,
        }
    }
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
            "--upper" => {
                self.upper = Some(match choice(&text(), &UPPERS)? {
                    "ram" => RootUpper::Ram,
                    _ => RootUpper::Scratch,
                });
            }
            "--state-dir" => self.state_dir = Some(text()),
            "--entrypoint" => self.entrypoint = text(),
            "--arg" => self.args.push(text()),
            "--hostname" => self.hostname = text(),
            "--workload-user" => {
                self.workload_user =
                    parse_workload_identity(&text()).map_err(|error| error.message().to_owned())?;
            }
            "--memory-max" => self.memory_max = Some(integer(&text(), 1..=u64::MAX)?),
            "--pids-max" => self.pids_max = Some(integer(&text(), 1..=PIDS_MAX_LIMIT)?),
            "--memory-mib" => self.memory_mib = integer(&text(), 1..=MAX_MEMORY_MIB)?,
            "--timeout" => self.timeout = seconds(&text())?,
            "--exec-timeout-ms" => self.exec_timeout_ms = integer(&text(), 0..=u32::MAX)?,
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
            "--outcome-report" => self.outcome_report = Some(text()),
            "--cmdline" => self.cmdline = text(),
            "--dry-run" => self.dry_run = true,
            _ => return self.network.take(name, &text()),
        }
        Ok(())
    }
}

/// Parses the arguments of `sandbox`, and returns the options and the arguments that the parser
/// does not recognize.
pub(in crate::host::cli) fn parse(arguments: &[String]) -> Result<(Options, Vec<String>)> {
    let mut options = Options::default();
    let unrecognized = Parser::new(&OPTIONS, &[Spec::optional("sandbox_operation")])
        .parse(arguments, &mut options)
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX))?;
    Ok((options, unrecognized))
}

/// Returns `_hypervisor(selected)`: `auto` selects `whp` on Windows and `kvm` elsewhere.
pub(in crate::host::cli) fn hypervisor(selected: &'static str) -> &'static str {
    match selected {
        "auto" if cfg!(windows) => "whp",
        "auto" => "kvm",
        other => other,
    }
}

/// Returns the path `value` spelled with its components alone, as [`paths::normalized`] spells
/// it.
pub(in crate::host::cli) fn path_text(value: &str) -> String {
    paths::normalized(Path::new(value)).display().to_string()
}

/// Returns `path` as text, which the commands of `nvx.py` hold as `str`.
pub(crate) fn path_string(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        Exit::error(
            ErrorCode::Unsupported,
            format!("path is not valid Unicode: {}", path.display()),
        )
    })
}

/// Returns the network that the network options of `options` describe, with `rules`, the egress
/// rules that [`NetworkOptions::egress_rules`] resolved: the typed policy at the guest network of
/// `--net`, which [`SandboxNetwork::new`] checks before any launch, or `None` without `--net`, which
/// no other network option may then accompany.
///
/// Omitted options take OpenVMM's defaults: egress allowed, ingress denied, and host-loopback
/// access from the guest alone, [`HostLoopback::Outbound`]. As OpenVMM does, the policy takes
/// egress rules only with an explicit `--network-egress`, and reads `--network-egress-allow`,
/// `--network-egress-deny`, and `--host-loopback-forward` in OpenVMM's form. `--network-proxy` must
/// name the guest's gateway, and makes the policy proxy-only, with no scheme in its URL.
pub(super) fn sandbox_network(
    options: &Options,
    rules: &NativeRules,
) -> Result<Option<SandboxNetwork>> {
    let network = &options.network;
    let Some(net) = &network.net else {
        let network_options = network.network_egress.is_some()
            || network.network_ingress.is_some()
            || network.network_egress_policy_file.is_some()
            || network.host_loopback.is_some()
            || network.network_proxy.is_some()
            || !rules.allow.is_empty()
            || !rules.deny.is_empty()
            || !network.host_loopback_forward.is_empty();
        if network_options {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                "network policy options require --net",
            ));
        }
        return Ok(None);
    };
    let gateway = guest_gateway(net)?;
    if network.network_egress.is_none() && (!rules.allow.is_empty() || !rules.deny.is_empty()) {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "--network-egress is required with --network-egress-allow or --network-egress-deny",
        ));
    }
    let access = |value: Option<&str>, default| match value {
        Some("allow") => Access::Allow,
        Some(_) => Access::Deny,
        None => default,
    };
    // A refused value keeps the code of its refusal, after the option that gave it.
    let refused = |option: &str, error: crate::Error| {
        Exit::error(error.code(), format!("{option} {}", error.message()))
    };
    let parsed_rules = |option: &str, native: &[String]| -> Result<EgressRules> {
        let rules = native
            .iter()
            .map(|rule| EgressRule::parse_native(rule).map_err(|error| refused(option, error)))
            .collect::<Result<_>>()?;
        Ok(EgressRules {
            name: option.to_owned(),
            rules,
        })
    };
    let mut policy = NetworkPolicy::new(access(network.network_egress, Access::Allow));
    policy.allow = parsed_rules("--network-egress-allow", &rules.allow)?;
    policy.deny = parsed_rules("--network-egress-deny", &rules.deny)?;
    policy.ingress = access(network.network_ingress, Access::Deny);
    policy.host_loopback = match network.host_loopback {
        Some(host_loopback) => access(Some(host_loopback), Access::Deny).into(),
        None => HostLoopback::Outbound,
    };
    for forward in &network.host_loopback_forward {
        let forward = LoopbackForward::parse(forward)
            .map_err(|error| refused("--host-loopback-forward", error))?;
        policy.host_loopback_forwards.push(forward);
    }
    if let Some(proxy) = &network.network_proxy {
        let proxy = NetworkProxy::parse_endpoint(proxy, gateway)
            .map_err(|error| refused("--network-proxy", error))?;
        policy.proxy = Some(proxy);
    }
    Ok(Some(SandboxNetwork::new(net, policy)?))
}
