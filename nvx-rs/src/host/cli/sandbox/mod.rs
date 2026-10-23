//! `nvx-rs sandbox`: `nvx.py sandbox`, ported from `command_sandbox` and the `sandbox` parser of
//! `scripts/nvx.py`, with its options, defaults, checks, outputs, and exit statuses.
//!
//! - `run`, the default, boots a one-shot sandbox: it prints the OpenVMM command as `>> COMMAND`,
//!   then runs it on the terminal, unless `--dry-run`, and exits with OpenVMM's status. It runs
//!   OpenVMM through a [`PreparedLaunch`] of the distro layer that it verified and of the scratch
//!   image, which it claims, so the command names their checked files: on Linux as
//!   `/proc/self/fd/N`, and on Windows by their final paths. A dry run, which launches nothing,
//!   names their paths.
//! - `provision`, `start`, `exec`, `stop`, and `deprovision` manage a persistent sandbox in its
//!   `--state-dir`; see [`lifecycle`].
//!
//! The parser reads the options of `nvx.py sandbox` with their names, defaults, types, and
//! choices, and fails as `argparse` fails, with status 2. The `--mount-deny`, `--mount-allow`,
//! and `--mount-write` options name paths of the `--mount` before them.

mod lifecycle;
mod lock;
mod state;

#[cfg(not(windows))]
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use super::args::{Parser, Receiver, Spec, Syntax, choice, integer, invalid_value};
use super::artifacts::{Located, OPENVMM_VARIABLE, locate};
use super::network::{NativeRules, NetworkOptions};
use super::{Exit, Result, output};
use crate::ErrorCode;
use crate::host::artifacts::{Guest, KERNEL_NAME};
use crate::host::network::{
    Access, EgressRule, EgressRules, HostLoopback, LoopbackForward, NetworkPolicy, NetworkProxy,
};
use crate::host::platform;
use crate::host::python::builtins::{Int, parse_float};
use crate::host::python::fs::os_error_message;
use crate::host::python::path::PurePath;
use crate::host::python::shell::format_command;
use crate::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
use crate::host::sandbox::mount::{MountOwner, PolicyKind, plan_mounts, validate_mounts};
use crate::host::sandbox::options::{
    PolicyPathOption, SandboxOperation, SandboxOptions, sandbox_mounts,
    validate_sandbox_systemd_policy,
};
use crate::host::sandbox::plan::guest_gateway;
use crate::host::sandbox::scratch::{CLAIM_RELEASE_WAIT, ScratchBinding};
use crate::host::sandbox::{
    DEFAULT_ENTRYPOINT, DEFAULT_HOSTNAME, DEFAULT_WORKLOAD_IDENTITY, PIDS_MAX_LIMIT,
    PreparedLaunch, SandboxLaunch, SandboxLayer, SandboxNetwork, SandboxPlan, SandboxSettings,
    parse_decimal, parse_workload_identity, require_artifact,
};
use crate::host::store::RuntimeIdentity;
use crate::host::supervisor::Inherited;

pub(crate) use self::lifecycle::ProvisionConfig;
use self::state::RecordedDistro;

/// The usage and help of the parser.
const SYNTAX: Syntax = Syntax {
    command: "sandbox",
    optionals: &[
        "[-h]",
        "[--layer ROLE,PATH,EROFS_UUID]",
        "[--scratch SCRATCH]",
        "[--state-dir STATE_DIR]",
        "[--entrypoint ENTRYPOINT]",
        "[--arg SANDBOX_ARG]",
        "[--hostname HOSTNAME]",
        "[--workload-user UID:GID]",
        "[--memory-max MEMORY_MAX]",
        "[--pids-max PIDS_MAX]",
        "[--memory-mib MEMORY_MIB]",
        "[--timeout TIMEOUT]",
        "[--exec-timeout-ms EXEC_TIMEOUT_MS]",
        "[--cwd CWD]",
        "[--environment KEY=VALUE]",
        "[--environment-file PATH]",
        "[--inherit-default-environment]",
        "[--hypervisor {auto,whp,kvm,mshv}]",
        "[--mount GUEST_TARGET,HOST_PATH[,ro|rw]]",
        "[--mount-deny HOST_PATH]",
        "[--mount-allow HOST_PATH]",
        "[--mount-write HOST_PATH]",
        "[--mount-owner {vmm,caller}]",
        "[--net IPV4/PREFIX]",
        "[--network-profile {portable}]",
        "[--network-egress {allow,deny}]",
        "[--network-ingress {allow,deny}]",
        "[--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]]",
        "[--network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]]",
        "[--network-egress-policy-file PATH]",
        "[--host-loopback {allow,deny}]",
        "[--network-proxy IPV4:TCP-PORT]",
        "[--host-loopback-forward HOST_LOOPBACK_FORWARD]",
        "[--outcome-report OUTCOME_REPORT]",
        "[--cmdline CMDLINE]",
        "[--dry-run]",
    ],
    positionals: &["[{run,provision,start,exec,stop,deprovision}]"],
    help: "\
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
  --dry-run             print the one-shot run command without launching it",
};

/// The operations, in the order in which usage messages list them.
const OPERATIONS: [&str; 6] = ["run", "provision", "start", "exec", "stop", "deprovision"];
/// The choices of `--hypervisor`.
pub(super) const HYPERVISORS: [&str; 4] = ["auto", "whp", "kvm", "mshv"];
/// The choices of `--mount-owner`.
pub(super) const MOUNT_OWNERS: [&str; 2] = ["vmm", "caller"];

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
    pub(crate) memory_max: Option<u64>,
    /// `--pids-max`.
    pub(crate) pids_max: Option<u32>,
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
            "--state-dir" => self.state_dir = Some(text()),
            "--entrypoint" => self.entrypoint = text(),
            "--arg" => self.args.push(text()),
            "--hostname" => self.hostname = text(),
            "--workload-user" => {
                self.workload_user =
                    parse_workload_identity(&text()).map_err(|error| error.message().to_owned())?;
            }
            "--memory-max" => {
                self.memory_max = Some(
                    parse_decimal::<u64>(&text())
                        .filter(|&value| value != 0)
                        .ok_or_else(|| {
                            format!(
                                "expected an ASCII decimal integer between 1 and {}",
                                u64::MAX
                            )
                        })?,
                );
            }
            "--pids-max" => {
                self.pids_max = Some(
                    parse_decimal::<u32>(&text())
                        .filter(|&value| value != 0 && value <= PIDS_MAX_LIMIT)
                        .ok_or_else(|| {
                            format!(
                                "expected an ASCII decimal integer between 1 and {PIDS_MAX_LIMIT}"
                            )
                        })?,
                );
            }
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
            "--outcome-report" => self.outcome_report = Some(text()),
            "--cmdline" => self.cmdline = text(),
            "--dry-run" => self.dry_run = true,
            _ => return self.network.take(name, &text()),
        }
        Ok(())
    }
}

/// Parses the arguments of `sandbox` for the program `program`, and returns the options and the
/// arguments that the parser does not recognize.
pub(super) fn parse(arguments: &[String], program: &str) -> Result<(Options, Vec<String>)> {
    let mut options = Options::default();
    let unrecognized = Parser::new(&OPTIONS, &[Spec::optional("sandbox_operation")])
        .parse(arguments, &mut options)
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX, program))?;
    Ok((options, unrecognized))
}

/// Returns `_hypervisor(selected)`: `auto` selects `whp` on Windows and `kvm` elsewhere.
pub(super) fn hypervisor(selected: &'static str) -> &'static str {
    match selected {
        "auto" if cfg!(windows) => "whp",
        "auto" => "kvm",
        other => other,
    }
}

/// Returns `str(Path(value))`.
pub(super) fn path_text(value: &str) -> String {
    PurePath::native(value).as_str().to_owned()
}

/// Returns `path` as text, which the commands of `nvx.py` hold as `str`.
pub(crate) fn path_string(path: &std::path::Path) -> Result<String> {
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
fn sandbox_network(options: &Options, rules: &NativeRules) -> Result<Option<SandboxNetwork>> {
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
        require_artifact(&openvmm, "OpenVMM release binary")?;
        let kernel = dir.artifact(KERNEL_NAME);
        require_artifact(&kernel, "Linux direct kernel")?;
        let initrd = dir.initramfs(Guest::Alpine);
        require_artifact(&initrd, "initramfs")?;
        Ok([
            path_string(&openvmm)?,
            path_string(&kernel)?,
            path_string(&initrd)?,
        ])
    }

    /// Returns the runtime identity of the artifacts, which the install metadata of the root
    /// names, as [`ArtifactDir::runtime_identity`](crate::host::artifacts::ArtifactDir::runtime_identity)
    /// reads it.
    pub(crate) fn runtime_identity(&self) -> Result<RuntimeIdentity> {
        locate(self.root)?
            .dir
            .runtime_identity()
            .map_err(error_with_cause)
    }

    /// Returns the runtime identity under which a managed start launches OpenVMM, as
    /// [`runtime_identity`](Self::runtime_identity) does. The install metadata names the root's
    /// own OpenVMM alone, so the start refuses another one that `NVX_OPENVMM` names, which the
    /// runtime record would otherwise misname.
    pub(crate) fn launch_identity(&self) -> Result<RuntimeIdentity> {
        let Located { dir, openvmm } = locate(self.root)?;
        if openvmm != dir.openvmm() {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                format!(
                    "a managed sandbox runs the OpenVMM of the artifact root, which its install \
                     metadata identifies, and not the one that {OPENVMM_VARIABLE} names"
                ),
            ));
        }
        dir.runtime_identity().map_err(error_with_cause)
    }
}

/// Returns the error that prints the message of `error`, followed by the I/O error or other
/// cause that it keeps, if any, as Python prints an `OSError`, with the code of `error`.
pub(crate) fn error_with_cause(error: crate::Error) -> Exit {
    let cause = std::error::Error::source(&error).map(|source| {
        match source.downcast_ref::<std::io::Error>() {
            Some(cause) => os_error_message(cause, None),
            None => source.to_string(),
        }
    });
    match cause {
        Some(cause) => Exit::error(error.code(), format!("{}: {cause}", error.message())),
        None => Exit::from(error),
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
        network_options: options.network.any(),
        net: options.network.net.is_some(),
        network_profile: options.network.network_profile.is_some(),
        entrypoint: options.entrypoint.clone(),
        outcome_report: options.outcome_report.is_some(),
        mounts: options.mounts.clone(),
        policy_paths: options.policy_paths.clone(),
        mount_owner: options.mount_owner,
        layers: !options.layers.is_empty(),
        scratch: options.scratch.is_some(),
    };
    let environment = checks.check()?;
    let launch = if matches!(
        operation,
        SandboxOperation::Run | SandboxOperation::Provision
    ) {
        let rules = options.network.egress_rules()?;
        let mounts = sandbox_mounts(&options.mounts, &options.policy_paths, options.mount_owner)?;
        let settings = SandboxSettings {
            entrypoint: options.entrypoint.clone(),
            args: options.args.clone(),
            hostname: options.hostname.clone(),
            workload_identity: options.workload_user,
            memory_max: options.memory_max,
            pids_max: options.pids_max,
        };
        let launch = SandboxLaunch::new(
            options.layers.clone(),
            options.scratch.as_deref().unwrap_or_default(),
            settings,
        )?;
        validate_mounts(&mounts)?;
        launch.validated()?;
        // The shares are planned once, from their resolved host paths, and `provision` records
        // the plan, whose identities each `start` checks again.
        let launch = launch.with_export(plan_mounts(&mounts)?)?;
        // `run` and `provision` verify the distro layer explicitly, at the path given;
        // `provision` records its binding, which `start` checks without reading the layer again.
        let binding = validate_sandbox_systemd_policy(&launch)?;
        // The network's policy is checked before anything launches or is recorded.
        let launch = launch.with_network(sandbox_network(&options, &rules)?);
        Some((launch, binding))
    } else {
        None
    };
    let artifacts = Artifacts { root };
    let state_dir = |operation: &str| {
        options.state_dir.as_deref().ok_or_else(|| {
            Exit::error(
                ErrorCode::Unsupported,
                format!("sandbox {operation} requires --state-dir"),
            )
        })
    };
    match (operation, launch) {
        (SandboxOperation::Provision, Some((launch, binding))) => {
            // `provision` records the layer at its resolved path, where `start` checks the
            // binding, with the manifest beside that path.
            let distro = RecordedDistro::new(&launch, binding)?;
            let state_dir = state_dir("provision")?;
            lifecycle::provision(
                state_dir,
                &launch,
                &distro,
                &ProvisionConfig {
                    hypervisor: hypervisor(options.hypervisor),
                    memory_mib: &options.memory_mib,
                    cmdline: &options.cmdline,
                },
                options.timeout,
            )
        }
        (SandboxOperation::Start, _) => {
            lifecycle::start(state_dir("start")?, &artifacts, options.timeout)
        }
        (SandboxOperation::Exec, _) => {
            let state_dir = state_dir("exec")?;
            if let Some(report) = &options.outcome_report {
                lifecycle::validate_outcome_destination(report)?;
            }
            let mut arguments = vec![options.entrypoint.clone()];
            arguments.extend(options.args.iter().cloned());
            let result = lifecycle::exec_workload(
                state_dir,
                &artifacts,
                &super::session::ExecRequest {
                    arguments,
                    timeout_ms: options.exec_timeout_ms.clone(),
                    cwd: options.cwd.clone(),
                    environment,
                    inherit_default_environment: options.inherit_default_environment,
                },
                options.timeout,
            )?;
            output::write_stdout(&result.stdout)
                .and_then(|()| output::write_stderr(&result.stderr))
                .map_err(Exit::from_os_error)?;
            if let Some(report) = &options.outcome_report {
                // An error of the report follows the workload's output, and starts a line of its
                // own even after standard error that the workload left without a line end, so
                // that callers find it as they find any other.
                lifecycle::write_exec_outcome(report, &result)
                    .inspect_err(|_| output::end_stderr_line(&result.stderr))?;
            }
            Err(Exit::Status(result.returncode))
        }
        (SandboxOperation::Stop, _) => {
            lifecycle::stop(state_dir("stop")?, &artifacts, options.timeout).map(drop)
        }
        (SandboxOperation::Deprovision, _) => {
            lifecycle::deprovision(state_dir("deprovision")?, &artifacts, options.timeout)
        }
        (SandboxOperation::Run, Some((launch, binding))) => {
            if options.state_dir.is_some() {
                return Err(Exit::error(
                    ErrorCode::Unsupported,
                    "one-shot sandbox execution rejects persistent --state-dir settings",
                ));
            }
            let plan = run_plan(&launch, &binding)?;
            let [openvmm, kernel, initrd] = artifacts.require()?;
            // A one-shot run delivers the shares as boot tokens. A dry run launches nothing, so it
            // shows the plan's own paths; a run launches through the checked files, which it holds
            // until OpenVMM exits.
            let (arguments, command_line, prepared) = if options.dry_run {
                (
                    plan.one_shot_openvmm_arguments()?,
                    plan.one_shot_kernel_command_line(&options.cmdline)?,
                    None,
                )
            } else {
                let prepared = prepare_run(&launch, &binding, plan, &options.cmdline)?;
                (
                    prepared.openvmm_arguments().to_vec(),
                    prepared.kernel_command_line().to_owned(),
                    Some(prepared),
                )
            };
            let mut command = vec![openvmm];
            command.extend(arguments);
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
                command_line,
            ]);
            if let Some(report) = &options.outcome_report {
                command.extend(["--microvm-report".to_owned(), path_text(report)]);
            }
            output::print_stdout(&format!(">> {}", format_command(&command)))
                .map_err(Exit::from_os_error)?;
            let Some(prepared) = prepared else {
                return Ok(());
            };
            Err(Exit::Status(run_openvmm(&command, &prepared.inherited())?))
        }
        (SandboxOperation::Run | SandboxOperation::Provision, None) => {
            unreachable!("run and provision build their launch")
        }
    }
}

/// Checks the launch of a one-shot run again, and returns its plan with the format and identity of
/// the distro layer that `binding`, which the run's verification of the layer made, records.
///
/// The verification read all of the layer after the shares were planned, so the launch must still
/// pass [`SandboxLaunch::validated`], as `provision` checks it before recording it and `start`
/// checks a recorded one: the directories and files of its export plan must still not overlap, by
/// path or through a hard link or bind mount, and must pass [`ExportPlan::verify`].
///
/// [`ExportPlan::verify`]: crate::host::filesystem::ExportPlan::verify
fn run_plan(launch: &SandboxLaunch, binding: &DistroBinding) -> Result<SandboxPlan> {
    launch.validated()?;
    launch.bound_plan(binding).map_err(Exit::from)
}

/// Checks the distro layer of a one-shot run against `binding`, which the run's verification of
/// the layer made, binds and claims its scratch image, and prepares the one-shot launch of `plan`,
/// which delivers the shares as boot tokens, with the checked files and the user's kernel command
/// line `cmdline`.
///
/// The checks read no content, so the layer that the run verified is the one that OpenVMM gets:
/// one that changed meanwhile fails its check.
fn prepare_run(
    launch: &SandboxLaunch,
    binding: &DistroBinding,
    plan: SandboxPlan,
    cmdline: &str,
) -> Result<PreparedLaunch> {
    let distro = binding.check(launch.distro(), DISTRO_POLICY_REVISION)?;
    let scratch = ScratchBinding::bind(launch.scratch())?
        .check(launch.scratch(), Instant::now() + CLAIM_RELEASE_WAIT)?;
    PreparedLaunch::one_shot(plan, distro, Some(scratch), cmdline).map_err(Exit::from)
}

/// Runs OpenVMM on the terminal, as `subprocess.run(command)` does, and returns its status.
///
/// OpenVMM inherits the descriptors or handles `inherit`, under their numbers in this process, and
/// no other of this process beyond its standard streams, as with `subprocess.run`, which closes
/// the others on Linux and passes none on Windows: on Linux, `inherit` names the checked files of
/// the run; on Windows, it names the claim's handle of the run's scratch image, OpenVMM's handle
/// list holds copies of this process's standard handles and that handle alone, and OpenVMM opens
/// the checked files by their final paths, which the checked files hold in place. `nvx-rs run`,
/// which holds no checked files, passes none, as `nvx.py run` passed none to `subprocess.run`.
///
/// OpenVMM shares the terminal and its process group, so Ctrl-C reaches it too. As
/// `subprocess.run` does, the CLI then gives it 0.25 s to exit on Unix before it kills it, and on
/// Windows, where Python cannot interrupt its wait, waits for it to exit; either way the command
/// ends as interrupted, also when OpenVMM exited first, since Python's wait raises
/// `KeyboardInterrupt` once Ctrl-C arrives.
pub(super) fn run_openvmm(command: &[String], inherit: &[Inherited<'_>]) -> Result<i32> {
    let mut openvmm = OneShot::spawn(command, inherit)?;
    loop {
        let status = openvmm.poll()?;
        if platform::interrupted() {
            if status.is_none() {
                end_interrupted(&mut openvmm)?;
            }
            return Err(Exit::Interrupted);
        }
        if let Some(status) = status {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The OpenVMM process of a one-shot run.
struct OneShot {
    /// The process, which `std::process` started.
    #[cfg(not(windows))]
    child: Child,
    /// The process, which [`platform::spawn_foreground`] started.
    #[cfg(windows)]
    process: platform::LaunchedProcess,
}

impl OneShot {
    /// Starts OpenVMM with `command`, with the descriptors or handles `inherit`, as
    /// [`run_openvmm`] says.
    fn spawn(command: &[String], inherit: &[Inherited<'_>]) -> Result<Self> {
        let failed = |error: std::io::Error| {
            // The host's own refusals of a command, such as of a batch file on Windows, carry no
            // OS error.
            let refused =
                error.kind() == std::io::ErrorKind::InvalidInput && error.raw_os_error().is_none();
            let code = if refused {
                ErrorCode::Unsupported
            } else {
                ErrorCode::Io
            };
            Exit::error(code, spawn_error(&error, &command[0]))
        };
        #[cfg(windows)]
        {
            let arguments: Vec<std::ffi::OsString> = command[1..].iter().map(Into::into).collect();
            platform::spawn_foreground(std::path::Path::new(&command[0]), &arguments, inherit)
                .map(|process| Self { process })
                .map_err(failed)
        }
        #[cfg(not(windows))]
        {
            let mut child = Command::new(&command[0]);
            child.args(&command[1..]);
            platform::inherit_only(&mut child, inherit).map_err(failed)?;
            child.spawn().map(|child| Self { child }).map_err(failed)
        }
    }

    /// Returns OpenVMM's status if it has exited: `Popen.returncode`, which on Unix is the
    /// negated signal of a process that a signal ended, and on Windows the status of
    /// `GetExitCodeProcess`, as `std::process::ExitStatus::code` reads it.
    fn poll(&mut self) -> Result<Option<i32>> {
        #[cfg(windows)]
        {
            let status = self
                .process
                .wait_timeout(Duration::ZERO)
                .map_err(Exit::from_os_error)?;
            Ok(status.map(|status| status as i32))
        }
        #[cfg(not(windows))]
        {
            let status = self.child.try_wait().map_err(Exit::from_os_error)?;
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
    }
}

/// Ends the OpenVMM of an interrupted run as `subprocess.run` does: on Unix, it waits 0.25 s for
/// OpenVMM to exit and then kills it, and on Windows, it waits for OpenVMM to exit.
fn end_interrupted(openvmm: &mut OneShot) -> Result {
    #[cfg(not(windows))]
    let deadline = Instant::now() + Duration::from_millis(250);
    while openvmm.poll()?.is_none() {
        #[cfg(not(windows))]
        if cfg!(unix) && Instant::now() >= deadline {
            let _ = openvmm.child.kill();
            let _ = openvmm.child.wait();
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Returns the message of the `OSError` with which `subprocess` fails to start `program`: with
/// the program's name on POSIX, and without on Windows.
pub(crate) fn spawn_error(error: &std::io::Error, program: &str) -> String {
    let program = if cfg!(windows) { None } else { Some(program) };
    crate::host::python::fs::os_error_message(error, program)
}

#[cfg(test)]
mod tests;
