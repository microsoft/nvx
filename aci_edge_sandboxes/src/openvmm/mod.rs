//! Default backend: drives the `openvmm` binary directly.
//!
//! Each sandbox is a microVM that runs the NVX guest's Alpine Linux userland directly from its
//! initramfs; there are no image layers, scratch disks, or container namespaces. Guest state lives
//! in memory and lasts until the sandbox stops.
//!
//! [`OpenVmmBackend`] converts the contract's requests into the launch specifications, policies,
//! and workloads of [`nvx`], and maps NVX's errors onto the contract's error codes. It keeps each
//! sandbox in a state directory under [`OpenVmmConfig::state_root`], which holds the sandbox's
//! configuration and the store of its VM [`Instance`](nvx::host::instance::Instance): the lock,
//! the launch journal, and the runtime record of NVX's sandbox store, so every operation can run
//! in a different process:
//!
//! - **provision** validates the request and records the configuration. No VM runs yet.
//! - **start** launches a detached OpenVMM process with the managed lifecycle, passes it a fresh
//!   32-byte capability through standard input, and waits until the guest agent answers on the
//!   authenticated control console. The agent must also advertise the control features this
//!   backend depends on (cancellation, workload accounts, workload containment, per-execution
//!   environments, working directories, and host path mapping tables). A guest image that lacks
//!   one is terminated and start fails with
//!   [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable), because such an
//!   image would silently ignore the policy or request that needs the feature. Start then hands
//!   the agent the table of mapped host paths, and fails if the agent cannot mount them all.
//! - **exec** runs the workload through the control console and streams its output live.
//! - **stop** asks the guest to shut down and falls back to terminating OpenVMM after
//!   [`OpenVmmConfig::stop_timeout`].
//! - **deprovision** deletes the state directory.
//!
//! Every operation first finds the sandbox's VM as NVX's supervisor reattaches to it after a
//! restart: it ends a start whose caller died, and a sandbox whose VM died is treated as
//! provisioned again. The store records the runtime identity of each VM: the identities of its
//! OpenVMM, kernel, and initramfs, which [`OpenVmmConfig::runtime_identity`] gives or the install
//! metadata of the artifacts' release or checkout names, and the contract versions of the build
//! that started it. A VM whose runtime identity differs from the backend's, as after an upgrade,
//! is never driven, since its guest might speak other contracts: **stop** terminates its OpenVMM
//! through its verified process identity, without a control session, waiting up to
//! [`OpenVmmConfig::stop_timeout`] for it to exit, and every other operation fails with
//! [`ErrorCode::BackendError`](crate::ErrorCode::BackendError) and leaves the VM running.
//!
//! # Policy honor matrix
//!
//! | Field | provision | exec |
//! | --- | --- | --- |
//! | `filesystem.readonlyPaths`, `readwritePaths` | mapped at their [`nvx::guest_path`] | n/a |
//! | `filesystem.deniedPaths` | hidden inside mapped paths | n/a |
//! | `network.egress` | default and IPv4 and IPv6 rules applied | n/a |
//! | `network.ingress.default` | `allow` and `deny` applied; see below | n/a |
//! | `network.ingress.hostLoopback` | `allow` and `deny` applied, omitted means `deny`; see below | n/a |
//! | `spec.hostLoopbackForwards` | applied; see below | n/a |
//! | `microvm.provision.memoryMib` | applied | n/a |
//! | `process.commandLine` | n/a | run as `/bin/sh -c <commandLine>`, at most 4096 bytes |
//! | `process.cwd` | n/a | an absolute guest path of at most 4095 bytes; `/` when omitted |
//! | `process.timeout` | n/a | up to 4,294,967,295 ms (about 49.7 days), the most that MXC allows |
//! | `process.env`, `inheritDefaultEnv` | n/a | applied per execution; see [Environment](#environment) |
//! | piped stdin | n/a | rejected |
//!
//! The guest agent enters the working directory with the workload's identity before it starts
//! the workload. A directory that does not exist, is not a directory, or that the workload cannot
//! search fails the launch instead: the execution ends with [`ExecFailure::WorkingDirectory`]
//! after a diagnostic on standard error, and nothing runs. Relative paths are rejected with
//! [`ErrorCode::PolicyValidation`](crate::ErrorCode::PolicyValidation); [`nvx::guest_path`]
//! translates host paths.
//!
//! Host paths share OpenVMM's single virtio-fs device: the backend exports each outermost mapped
//! directory, and the parent of each outermost mapped file, as one child of an aggregate that
//! only the guest's root can enter, and the guest agent bind-mounts each mapped path, read-only
//! or read-write. OpenVMM enforces each mapping's access on the host, and hides denied paths and
//! everything that is not mapped beside mapped files. A read-only path inside a read-write
//! directory, which the host could not keep read-only, is refused with
//! [`ErrorCode::PolicyValidation`](crate::ErrorCode::PolicyValidation); a read-write path inside
//! a read-only directory is not. A whole volume, or a file directly in a volume's root, is not
//! mapped.
//! Egress rules are lowered into OpenVMM's IPv4 and IPv6 rules exactly, by the lowering that
//! compiles NVX's egress policy files ([`nvx::host::network::policy`]): destinations and port
//! ranges merge into the fewest rules, and an action that still needs more than 256 OpenVMM rules
//! is rejected. Egress denied without allow rules, with ingress and host loopback denied,
//! attaches no network device. With `network.ingress.default: allow`, OpenVMM listens on every
//! host address at the TCP and UDP ports where the workload listens, and admits clients on the
//! host's private networks, whose addresses are not globally reachable, but not clients inside
//! the guest network, IPv6 link-local clients, or the host itself unless host loopback is
//! allowed; replies to their connections pass whatever the egress
//! policy. With `network.ingress.hostLoopback: allow`, the workload reaches host loopback
//! services through its gateway, subject to the egress policy, and the host reaches the
//! workload: without ingress allowed, OpenVMM listens on host IPv4 and IPv6 loopback alone, never
//! displacing a host listener, at the ports where the workload listens on a non-loopback address,
//! and admits only the host, whose replies pass whatever the egress policy. A workload listener on
//! guest loopback stays private. The sandbox spec's `hostLoopbackForwards`
//! ([`AciEdgeSandbox::provision_with`](crate::AciEdgeSandbox::provision_with)) publish up to 64
//! further host loopback ports at guest ports of their choice, one forward per protocol and guest
//! port; they need host loopback allowed. OpenVMM lets the workload's replies on a forwarded TCP
//! connection through whatever the egress policy, but filters its UDP replies, which go to
//! addresses inside the guest network, so UDP forwards also need an egress default of `allow`,
//! and no deny rule may cover the guest network. Workloads run as the fixed non-root
//! identity of [`OpenVmmConfig::workload_uid`] and [`OpenVmmConfig::workload_gid`] (see
//! [`OpenVmmConfig::map_host_identity`] for Linux hosts) with no capabilities, read end-of-file on
//! standard input, and may produce at most 1 MiB of combined output.
//!
//! # Environment
//!
//! Each execution starts from its own environment, so nothing carries over from an earlier one.
//! Without `process.env`, the workload gets the guest's default environment: `PATH`, `TERM`, the
//! `HOME`, `USER`, and `LOGNAME` of the workload identity, and `PWD`, which names the working
//! directory, plus a few variables that the guest's boot leaves behind. It never holds the host's
//! variables. Supplied entries, even an empty list, are the complete environment, unless
//! `inheritDefaultEnv` is true, which layers them over the default one, each entry replacing the
//! default variable of the same name. Without `process.env`, `inheritDefaultEnv` has no effect.
//!
//! The exec request carries the entries, at most 256 with unique names, in a field of their own,
//! next to the working directory and the arguments. The guest agent enters the working directory
//! with the workload's identity, and applies the entries after it has dropped the workload's
//! privileges, just before it starts the program, so a `process.argv` program receives exactly
//! the requested environment. A `process.commandLine` runs in `/bin/sh`, and that shell is the
//! workload, so it exports variables of its own: BusyBox's `sh` sets `SHLVL` and `PWD`, also
//! when an entry has one of those names.
//!
//! # Concurrency and cancellation
//!
//! Lifecycle transitions of one sandbox are serialized by the lock of its store. Executions are
//! serialized by the guest: a second exec waits up to [`OpenVmmConfig::control_timeout`] for the
//! first to finish. A stop that cannot reach the guest within its timeout terminates the VM, and
//! any running exec then fails.
//!
//! The guest agent contains each workload in a cgroup and kills whatever the workload leaves
//! behind when its first process exits, so no workload process outlives its exec.
//! [`Canceller::cancel`](crate::Canceller::cancel) asks the guest to kill the workload; the
//! execution then ends with [`ExecOutcome::Cancelled`]. If the process that runs an execution
//! dies, the guest kills the workload as soon as it notices that the control session is gone.
//!
//! # Errors
//!
//! NVX's errors keep their messages. [`nvx::ErrorKind::Unavailable`] becomes
//! [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable),
//! [`nvx::ErrorKind::Unsupported`] becomes
//! [`ErrorCode::PolicyValidation`](crate::ErrorCode::PolicyValidation),
//! [`nvx::ErrorKind::Exited`] becomes [`ErrorCode::NotStarted`](crate::ErrorCode::NotStarted)
//! when an exec finds the sandbox's VM gone, and every other error becomes
//! [`ErrorCode::BackendError`](crate::ErrorCode::BackendError). Each keeps the finer
//! [`nvx::ErrorCode`] of the NVX error, which [`Error::nvx_code`](crate::Error::nvx_code) returns.

mod config;
mod state;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use nvx::Artifacts;
use nvx::host::filesystem::{self, ExportPlan, ExportRequest, Mapping, PathList, ReservedTargets};
use nvx::host::instance::{self, Execution, Found, LockedInstance, WorkloadOutcome};
use nvx::host::launch::{LaunchSpec, WorkloadIdentity};
use nvx::host::report::Report;
use nvx::host::store::RuntimeIdentity;
use nvx::host::{network, platform};
use nvx::protocol::{
    self, MAX_ARGUMENT_BYTES, MAX_CWD_BYTES, MAX_OUTPUT_BYTES, MAX_TIMEOUT_MS, Workload,
    WorkloadEnvironment,
};

pub use self::config::OpenVmmConfig;
use self::state::{BACKEND_KEY, STATE_FORMAT, SandboxRecord, StateStore};
use crate::backend::{Backend, ExecControl, ExecIo};
use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::exec::{Completion, ExecFailure, ExecOutcome};
use crate::id::SandboxId;
use crate::model::{
    Access, Command, DeprovisionResult, ExecRequest, FilesystemPolicy, ForwardProtocol,
    HostLoopbackForward, Metadata, NetworkPolicy, NetworkRule, ProcessSpec, Protocol,
    ProvisionRequest, ProvisionResult, StartResult, StopResult, duration_millis,
};
use crate::spec::SandboxSpec;

/// Shell that interprets `process.commandLine`.
const SHELL: &str = "/bin/sh";

/// [`Backend`] that drives the `openvmm` binary directly.
///
/// See the [module documentation](self) for the lifecycle, policy honor matrix, and concurrency
/// behavior.
#[derive(Debug)]
pub struct OpenVmmBackend {
    config: OpenVmmConfig,
    store: StateStore,
}

impl OpenVmmBackend {
    /// Creates the backend, resolving relative paths and creating the state root.
    ///
    /// Fails with [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable) when
    /// the configuration cannot work on this host.
    pub fn new(config: OpenVmmConfig) -> Result<Self> {
        let config = config.normalized()?;
        let store = StateStore::open(&config.state_root).map_err(|error| {
            Error::backend_unavailable(format!(
                "cannot open the sandbox state root {}",
                config.state_root.display()
            ))
            .with_source(error)
        })?;
        Ok(Self { config, store })
    }

    /// Returns the normalized configuration.
    pub fn config(&self) -> &OpenVmmConfig {
        &self.config
    }

    /// Returns the path of the OpenVMM log of a sandbox's latest start, for diagnostics.
    pub fn log_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.store.log_path(sandbox_id)
    }

    /// Finds the VM of the sandbox that `locked` holds, under the runtime identity of the
    /// configured artifacts, which it reads only if the store records a VM; see
    /// [`LockedInstance::find`].
    fn find(&self, locked: &mut LockedInstance) -> Result<Found> {
        Ok(locked.find(|| self.config.identify())?)
    }

    /// Checks that this host can run the configured artifacts, as [`Backend::probe`] does, and
    /// returns their runtime identity.
    fn check_host(&self) -> Result<RuntimeIdentity> {
        Artifacts::new(
            &self.config.openvmm,
            &self.config.kernel,
            &self.config.initrd,
        )
        .probe()?;
        let runtime = self.config.identify()?;
        #[cfg(feature = "testing")]
        if self.config.skip_hypervisor_probe {
            return Ok(runtime);
        }
        platform::probe_hypervisor(self.config.hypervisor)?;
        Ok(runtime)
    }

    /// Returns the launch specification of a provisioned sandbox.
    fn launch_spec(&self, record: &SandboxRecord) -> LaunchSpec {
        LaunchSpec {
            openvmm: self.config.openvmm.clone(),
            kernel: self.config.kernel.clone(),
            initrd: self.config.initrd.clone(),
            hypervisor: self.config.hypervisor,
            memory_mib: record.memory_mib,
            identity: WorkloadIdentity {
                uid: record.workload_uid,
                gid: record.workload_gid,
                create_account: record.create_workload_account,
            },
            hostname: record.hostname.clone(),
            kernel_parameters: self.config.kernel_command_line.clone(),
            guest_network: self.config.guest_network.clone(),
            network: record
                .network
                .as_ref()
                .map(|policy| network_policy(policy, &record.host_loopback_forwards)),
            filesystem: record.filesystem.clone(),
            breakaway_from_job: self.config.breakaway_from_job,
        }
    }
}

/// Converts the contract's network section and the sandbox spec's host-loopback forwards into
/// NVX's network posture, under the contract's field names. An omitted host-loopback access is
/// denied.
fn network_policy(
    policy: &NetworkPolicy,
    forwards: &[HostLoopbackForward],
) -> network::NetworkPolicy {
    let access = |access: Access| match access {
        Access::Allow => network::Access::Allow,
        Access::Deny => network::Access::Deny,
    };
    let rules = |name: &str, rules: &[NetworkRule]| network::EgressRules {
        name: name.to_owned(),
        rules: rules.iter().map(egress_rule).collect(),
    };
    network::NetworkPolicy {
        egress: access(policy.egress.default),
        allow: rules("network.egress.allow", &policy.egress.allow),
        deny: rules("network.egress.deny", &policy.egress.deny),
        ingress: access(policy.ingress.default),
        host_loopback: access(policy.ingress.host_loopback.unwrap_or(Access::Deny)).into(),
        host_loopback_forwards: forwards
            .iter()
            .map(|forward| network::LoopbackForward {
                protocol: match forward.protocol {
                    ForwardProtocol::Tcp => network::ForwardProtocol::Tcp,
                    ForwardProtocol::Udp => network::ForwardProtocol::Udp,
                },
                host_port: forward.host_port,
                guest_port: forward.guest_port,
            })
            .collect(),
        proxy: None,
    }
}

fn egress_rule(rule: &NetworkRule) -> network::EgressRule {
    network::EgressRule {
        to: rule
            .to
            .iter()
            .map(|peer| network::Destination {
                cidr: peer.cidr.clone(),
                except: peer.except.clone(),
            })
            .collect(),
        ports: rule
            .ports
            .iter()
            .map(|port| network::PortSelector {
                protocol: match port.protocol {
                    Protocol::Tcp => network::Protocol::Tcp,
                    Protocol::Udp => network::Protocol::Udp,
                    Protocol::Icmp => network::Protocol::Icmp,
                    Protocol::Any => network::Protocol::Any,
                },
                port: port.port,
                end_port: port.end_port,
            })
            .collect(),
    }
}

/// Plans the export of the contract's filesystem section, under the contract's field names, at
/// the guest paths of the host-path convention in the blockless root.
fn export_plan(policy: &FilesystemPolicy) -> Result<Option<ExportPlan>> {
    let list = |name, paths| PathList { name, paths };
    let mappings: Vec<Mapping<'_>> = Mapping::convention(
        list("filesystem.readonlyPaths", &policy.readonly_paths),
        true,
    )
    .chain(Mapping::convention(
        list("filesystem.readwritePaths", &policy.readwrite_paths),
        false,
    ))
    .collect();
    Ok(filesystem::plan(&ExportRequest {
        denied: list("filesystem.deniedPaths", &policy.denied_paths),
        ..ExportRequest::new(&mappings, ReservedTargets::BLOCKLESS_ROOT)
    })?)
}

/// Maps a failure to reach the VM of a running sandbox for an exec: a VM that is gone leaves the
/// sandbox not started.
fn exec_error(error: nvx::Error) -> Error {
    if error.kind() != nvx::ErrorKind::Exited {
        return error.into();
    }
    Error::from_nvx(crate::ErrorCode::NotStarted, error)
}

/// Summarizes OpenVMM's outcome report as stop metadata.
fn report_metadata(report: &Report) -> Metadata {
    let mut metadata = Metadata::new();
    if let Some(category) = &report.category {
        metadata.insert("vmOutcome".to_owned(), category.as_str().into());
    }
    if let Some(status) = report.status_code {
        metadata.insert("vmStatusCode".to_owned(), status.into());
    }
    if let Some(complete) = report.teardown_complete {
        metadata.insert("teardownComplete".to_owned(), complete.into());
    }
    metadata
}

fn workload_argv(process: &ProcessSpec) -> Result<Vec<String>> {
    match &process.command {
        Command::CommandLine(command_line) => {
            if command_line.len() > MAX_ARGUMENT_BYTES {
                return Err(Error::policy_validation(format!(
                    "process.commandLine exceeds the {MAX_ARGUMENT_BYTES}-byte limit of the \
                     openvmm backend"
                )));
            }
            Ok(vec![
                SHELL.to_owned(),
                "-c".to_owned(),
                command_line.clone(),
            ])
        }
        Command::Argv(argv) => Ok(argv.clone()),
    }
}

fn exec_timeout_ms(process: &ProcessSpec) -> Result<u32> {
    let timeout = process.timeout.unwrap_or_default();
    // The guest agent accepts every timeout that the request's 32-bit field carries. Compare the
    // exact duration: whole milliseconds would drop a fraction above the limit.
    u32::try_from(duration_millis(timeout))
        .ok()
        .filter(|_| timeout <= Duration::from_millis(MAX_TIMEOUT_MS.into()))
        .ok_or_else(|| {
            Error::policy_validation(format!(
                "process.timeout exceeds the openvmm backend limit of {MAX_TIMEOUT_MS} ms"
            ))
        })
}

/// Returns the workload that the guest agent runs for `process`, after checking it against the
/// limits of the agent's exec request.
///
/// The working directory and the environment travel in fields of their own, which the agent
/// applies just before it starts the program, so no shell runs in front of a `process.argv`
/// program.
fn prepare_exec(process: &ProcessSpec) -> Result<Workload<'_>> {
    if let Some(cwd) = &process.cwd
        && !cwd.starts_with('/')
    {
        return Err(Error::policy_validation(format!(
            "process.cwd {cwd:?} must be an absolute guest path; map host paths with \
             nvx::guest_path"
        )));
    }
    if let Some(cwd) = &process.cwd
        && cwd.len() > MAX_CWD_BYTES
    {
        return Err(Error::policy_validation(format!(
            "process.cwd exceeds the {MAX_CWD_BYTES}-byte limit of the openvmm backend"
        )));
    }
    let environment = match (&process.env, process.inherit_default_env) {
        (None, _) => WorkloadEnvironment::Default,
        (Some(entries), Some(true)) => WorkloadEnvironment::Layered(entries),
        (Some(entries), _) => WorkloadEnvironment::Replaced(entries),
    };
    let workload = Workload {
        argv: workload_argv(process)?,
        timeout_ms: exec_timeout_ms(process)?,
        cwd: process.cwd.as_deref(),
        environment,
    };
    protocol::encode_exec_payload(&workload)
        .map_err(|error| Error::policy_validation(error.into_message()))?;
    Ok(workload)
}

impl Backend for OpenVmmBackend {
    fn name(&self) -> &str {
        BACKEND_KEY
    }

    fn capabilities(&self) -> Capabilities {
        let mut capabilities = Capabilities::new(BACKEND_KEY);
        capabilities.exec.command_line = true;
        capabilities.exec.argv = true;
        capabilities.exec.cancel = true;
        capabilities.exec.env = true;
        capabilities.exec.clear_default_env = true;
        capabilities.exec.max_timeout_ms = Some(MAX_TIMEOUT_MS.into());
        capabilities.exec.max_output_bytes = Some(MAX_OUTPUT_BYTES as u64);
        capabilities.network.egress_allow = true;
        capabilities.network.egress_deny = true;
        capabilities.network.ingress_allow = true;
        capabilities.network.ingress_deny = true;
        capabilities.network.host_loopback_allow = true;
        capabilities.network.host_loopback_deny = true;
        capabilities.network.egress_rules = true;
        capabilities.spec.host_loopback_forwards = true;
        capabilities.filesystem.readonly_paths = true;
        capabilities.filesystem.readwrite_paths = true;
        capabilities.filesystem.denied_paths = true;
        capabilities.exec.cwd = true;
        capabilities
    }

    fn probe(&self) -> Result<()> {
        self.check_host().map(drop)
    }

    fn validate_provision(&self, request: &ProvisionRequest) -> Result<()> {
        self.validate_provision_with(request, &SandboxSpec::new())
    }

    fn validate_provision_with(
        &self,
        request: &ProvisionRequest,
        spec: &SandboxSpec,
    ) -> Result<()> {
        let memory_mib = request
            .microvm
            .provision
            .memory_mib
            .unwrap_or(self.config.memory_mib);
        if memory_mib == 0 {
            return Err(Error::policy_validation(
                "microvm.provision.memoryMib must be positive",
            ));
        }
        let forwarded = !spec.host_loopback_forwards.is_empty();
        let Some(policy) = &request.network else {
            if forwarded {
                return Err(Error::policy_validation(
                    "spec.hostLoopbackForwards require network.ingress.hostLoopback: allow",
                ));
            }
            return Ok(());
        };
        if forwarded && policy.ingress.host_loopback != Some(Access::Allow) {
            return Err(Error::policy_validation(
                "spec.hostLoopbackForwards require network.ingress.hostLoopback: allow",
            ));
        }
        // OpenVMM lets the workload's replies on a forwarded TCP connection through whatever the
        // egress policy, but filters its UDP replies.
        let udp_forwarded = spec
            .host_loopback_forwards
            .iter()
            .any(|forward| forward.protocol == ForwardProtocol::Udp);
        if udp_forwarded && policy.egress.default != Access::Allow {
            return Err(Error::policy_validation(
                "UDP spec.hostLoopbackForwards require network.egress.default: allow for their \
                 replies",
            ));
        }
        let network = network_policy(policy, &spec.host_loopback_forwards);
        network::openvmm_arguments(Some(&network), &self.config.guest_network)?;
        Ok(())
    }

    fn validate_exec(&self, request: &ExecRequest) -> Result<()> {
        prepare_exec(&request.process).map(|_| ())
    }

    fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionResult> {
        self.provision_with(request, &SandboxSpec::new())
    }

    fn provision_with(
        &self,
        request: &ProvisionRequest,
        spec: &SandboxSpec,
    ) -> Result<ProvisionResult> {
        self.validate_provision_with(request, spec)?;
        self.probe()?;
        let memory_mib = request
            .microvm
            .provision
            .memory_mib
            .unwrap_or(self.config.memory_mib);
        let filesystem = match &request.filesystem {
            Some(policy) => export_plan(policy)?,
            None => None,
        };
        let identity = WorkloadIdentity::select(
            self.config.workload_uid,
            self.config.workload_gid,
            self.config.map_host_identity,
            filesystem.is_some(),
        );
        let record = SandboxRecord {
            format: STATE_FORMAT,
            backend: BACKEND_KEY.to_owned(),
            network: request.network.clone(),
            filesystem,
            host_loopback_forwards: spec.host_loopback_forwards.clone(),
            memory_mib,
            workload_uid: identity.uid,
            workload_gid: identity.gid,
            create_workload_account: identity.create_account,
            hostname: self.config.hostname.clone(),
        };
        let spec = self.launch_spec(&record);
        spec.kernel_command_line()?;
        let sandbox_id = SandboxId::generate()?;
        // Windows names each start's control pipe afresh, but every name has the same length.
        instance::check_launch(&self.store.dir(&sandbox_id), &spec)?;
        self.store.create(&sandbox_id, &record)?;
        Ok(ProvisionResult {
            sandbox_id,
            metadata: None,
        })
    }

    fn start(&self, sandbox_id: &SandboxId) -> Result<StartResult> {
        let (mut locked, record) = self.store.lock_and_load(sandbox_id)?;
        match self.find(&mut locked)? {
            Found::Stopped => {}
            Found::Running(_) => {
                return Err(Error::already_started(format!(
                    "sandbox {sandbox_id} is already running"
                )));
            }
            Found::Mismatched(mismatched) => return Err(mismatched.error().into()),
        }
        let runtime = self.check_host()?;
        let started = locked.start(
            &self.launch_spec(&record),
            &runtime,
            self.config.start_timeout,
        )?;
        let mut metadata = Metadata::new();
        let boot_ms = u64::try_from(started.boot.as_millis()).unwrap_or(u64::MAX);
        metadata.insert("bootMilliseconds".to_owned(), boot_ms.into());
        Ok(StartResult {
            metadata: Some(metadata),
        })
    }

    fn exec(
        &self,
        sandbox_id: &SandboxId,
        request: &ExecRequest,
        io: ExecIo,
    ) -> Result<Box<dyn ExecControl>> {
        let workload = prepare_exec(&request.process)?;
        // The lock is held only while the running VM is found, so a long workload cannot delay
        // a stop.
        let (instance, running) = {
            let (mut locked, _) = self.store.lock_and_load(sandbox_id)?;
            match self.find(&mut locked)? {
                Found::Stopped => {
                    return Err(Error::not_started(format!(
                        "sandbox {sandbox_id} is not running"
                    )));
                }
                Found::Running(running) => (locked.instance().clone(), running),
                Found::Mismatched(mismatched) => return Err(mismatched.error().into()),
            }
        };
        let execution = instance
            .exec(
                &running,
                &workload,
                self.config.control_timeout,
                self.config.exec_response_grace,
            )
            .map_err(exec_error)?;
        let shared = Arc::new(ExecShared::default());
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name("aci-edge-sandboxes-openvmm-exec".to_owned())
            .spawn(move || {
                let outcome = pump(execution, io, &worker.cancel_requested);
                worker.finish(outcome);
            })
            .map_err(|error| {
                Error::backend_error("cannot start an execution thread").with_source(error)
            })?;
        Ok(Box::new(OpenVmmExecution { shared }))
    }

    fn stop(&self, sandbox_id: &SandboxId) -> Result<StopResult> {
        let (mut locked, _) = self.store.lock_and_load(sandbox_id)?;
        let stopped = match self.find(&mut locked)? {
            Found::Stopped => {
                return Err(Error::already_stopped(format!(
                    "sandbox {sandbox_id} is not running"
                )));
            }
            Found::Running(running) => locked.stop(running, self.config.stop_timeout)?,
            // Its guest might not speak this build's control protocol, so OpenVMM is terminated
            // through its verified process identity, without a control session.
            Found::Mismatched(mismatched) => {
                locked.terminate(mismatched, self.config.stop_timeout)?
            }
        };
        let mut metadata = Metadata::new();
        metadata.insert("forced".to_owned(), stopped.forced.into());
        if let Some(report) = &stopped.report {
            metadata.extend(report_metadata(report));
        }
        Ok(StopResult {
            metadata: Some(metadata),
        })
    }

    fn deprovision(&self, sandbox_id: &SandboxId) -> Result<DeprovisionResult> {
        let (mut locked, _) = self.store.lock_and_load(sandbox_id)?;
        match self.find(&mut locked)? {
            Found::Stopped => {}
            Found::Running(_) => {
                return Err(Error::already_started(format!(
                    "sandbox {sandbox_id} is running; stop it before deprovisioning"
                )));
            }
            Found::Mismatched(mismatched) => return Err(mismatched.error().into()),
        }
        self.store.remove(sandbox_id, locked)?;
        Ok(DeprovisionResult::default())
    }
}

/// Relays workload events to the output sinks of `io` and returns the terminal outcome. The
/// sinks close when it returns.
fn pump(
    execution: Execution,
    mut io: ExecIo,
    cancel_requested: &AtomicBool,
) -> Result<ExecOutcome> {
    let outcome = execution.pump(
        &mut |chunk: &[u8]| io.stdout.write(chunk),
        &mut |chunk: &[u8]| io.stderr.write(chunk),
        cancel_requested,
    )?;
    Ok(match outcome {
        WorkloadOutcome::Exited(code) => ExecOutcome::Exited(code),
        WorkloadOutcome::Signaled(signal) => ExecOutcome::Signaled(signal),
        WorkloadOutcome::TimedOut => ExecOutcome::TimedOut,
        WorkloadOutcome::OutputLimitExceeded => {
            ExecOutcome::Failed(ExecFailure::OutputLimitExceeded)
        }
        WorkloadOutcome::Failed => ExecOutcome::Failed(ExecFailure::Workload),
        WorkloadOutcome::Cancelled => ExecOutcome::Cancelled,
        WorkloadOutcome::LaunchFailed => ExecOutcome::Failed(ExecFailure::LaunchFailed),
        WorkloadOutcome::WorkingDirectory => ExecOutcome::Failed(ExecFailure::WorkingDirectory),
    })
}

#[derive(Default)]
struct ExecShared {
    completion: Completion,
    cancel_requested: AtomicBool,
}

impl ExecShared {
    fn finish(&self, outcome: Result<ExecOutcome>) {
        self.completion.finish(outcome);
    }
}

struct OpenVmmExecution {
    shared: Arc<ExecShared>,
}

impl ExecControl for OpenVmmExecution {
    fn wait(&self) -> Result<ExecOutcome> {
        self.shared.completion.wait()
    }

    fn cancel(&self) -> Result<()> {
        // The execution thread delivers the request; a finished execution ignores it.
        self.shared.cancel_requested.store(true, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::ErrorCode;
    use crate::model::{EgressPolicy, NetworkPeer, NetworkPort};

    #[test]
    fn command_lines_run_through_the_shell() {
        let request = ExecRequest::command_line("echo hi | wc -c");
        assert_eq!(
            workload_argv(&request.process).unwrap(),
            ["/bin/sh", "-c", "echo hi | wc -c"]
        );
        let long = ExecRequest::command_line("x".repeat(MAX_ARGUMENT_BYTES + 1));
        assert_eq!(
            workload_argv(&long.process).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
    }

    #[test]
    fn working_directories_are_entered_by_the_guest_agent() {
        // No shell runs in front of the workload, so a program keeps its exact environment.
        let request = ExecRequest::argv(["/bin/ls", "-l"])
            .with_cwd("/tmp")
            .with_envs(["FOO=bar"]);
        let workload = prepare_exec(&request.process).unwrap();
        assert_eq!(workload.argv, ["/bin/ls", "-l"]);
        assert_eq!(workload.cwd, Some("/tmp"));
        let request = ExecRequest::command_line("pwd").with_cwd("/mnt/c/work");
        let workload = prepare_exec(&request.process).unwrap();
        assert_eq!(workload.argv, ["/bin/sh", "-c", "pwd"]);
        assert_eq!(workload.cwd, Some("/mnt/c/work"));
        assert_eq!(
            prepare_exec(&ExecRequest::command_line("pwd").process)
                .unwrap()
                .cwd,
            None
        );
        for relative in [r"C:\work", "work"] {
            let request = ExecRequest::command_line("pwd").with_cwd(relative);
            let error = prepare_exec(&request.process).unwrap_err();
            assert_eq!(error.code(), ErrorCode::PolicyValidation);
            assert!(error.message().contains("nvx::guest_path"), "{error}");
        }
        // The whole 4096-byte command line remains available with a working directory.
        let longest = "x".repeat(MAX_ARGUMENT_BYTES);
        let request = ExecRequest::command_line(longest.as_str()).with_cwd("/tmp");
        assert_eq!(prepare_exec(&request.process).unwrap().argv[2], longest);
    }

    #[test]
    fn working_directories_must_be_bounded_absolute_guest_paths() {
        let longest = format!("/{}", "d".repeat(MAX_CWD_BYTES - 1));
        let request = ExecRequest::command_line("pwd").with_cwd(longest.as_str());
        assert_eq!(prepare_exec(&request.process).unwrap().cwd, Some(&*longest));
        for cwd in [
            "work".to_owned(),
            "./work".to_owned(),
            "../work".to_owned(),
            r"C:\work".to_owned(),
            format!("{longest}d"),
        ] {
            let request = ExecRequest::command_line("pwd").with_cwd(cwd.as_str());
            let error = prepare_exec(&request.process).unwrap_err();
            assert_eq!(error.code(), ErrorCode::PolicyValidation, "{cwd:?}");
            assert!(error.message().contains("process.cwd"), "{error}");
        }
    }

    #[test]
    fn environments_select_their_mode_from_inherit_default_env() {
        let entries = vec!["FOO=bar".to_owned()];
        let environment = |request: ExecRequest| {
            let workload = prepare_exec(&request.process).unwrap();
            match workload.environment {
                WorkloadEnvironment::Default => "default",
                WorkloadEnvironment::Replaced(replaced) if replaced == entries => "replaced",
                WorkloadEnvironment::Layered(layered) if layered == entries => "layered",
                other => panic!("unexpected environment {other:?}"),
            }
        };
        let env = || ExecRequest::argv(["/usr/bin/env"]);
        assert_eq!(environment(env()), "default");
        assert_eq!(environment(env().with_inherit_default_env(true)), "default");
        assert_eq!(
            environment(env().with_inherit_default_env(false)),
            "default"
        );
        assert_eq!(environment(env().with_envs(entries.clone())), "replaced");
        assert_eq!(
            environment(
                env()
                    .with_envs(entries.clone())
                    .with_inherit_default_env(false)
            ),
            "replaced"
        );
        assert_eq!(
            environment(
                env()
                    .with_envs(entries.clone())
                    .with_inherit_default_env(true)
            ),
            "layered"
        );
        let empty = env().with_envs(Vec::<String>::new());
        assert_eq!(
            prepare_exec(&empty.process).unwrap().environment,
            WorkloadEnvironment::Replaced(&[])
        );
    }

    #[test]
    fn timeouts_cover_the_mxc_range() {
        let timeout =
            |millis| ExecRequest::command_line("true").with_timeout(Duration::from_millis(millis));
        for millis in [
            1,
            3_600_000,
            3_600_001,
            86_400_000,
            ProcessSpec::MAX_TIMEOUT_MS,
        ] {
            let request = timeout(millis);
            assert_eq!(
                u64::from(exec_timeout_ms(&request.process).unwrap()),
                millis
            );
            assert_eq!(
                u64::from(prepare_exec(&request.process).unwrap().timeout_ms),
                millis
            );
        }
        // Even a fraction of a millisecond above the limit is refused.
        let maximum = Duration::from_millis(ProcessSpec::MAX_TIMEOUT_MS);
        for beyond in [
            maximum + Duration::from_nanos(1),
            maximum + Duration::from_millis(1),
            Duration::MAX,
        ] {
            let request = ExecRequest::command_line("true").with_timeout(beyond);
            assert_eq!(
                exec_timeout_ms(&request.process).unwrap_err().code(),
                ErrorCode::PolicyValidation,
                "{beyond:?}"
            );
        }
        // Zero and an omitted timeout both disable it.
        assert_eq!(exec_timeout_ms(&timeout(0).process).unwrap(), 0);
        let none = ExecRequest::command_line("true");
        assert_eq!(exec_timeout_ms(&none.process).unwrap(), 0);
    }

    #[test]
    fn network_policies_convert_field_for_field() {
        let policy = NetworkPolicy {
            egress: EgressPolicy::new(Access::Deny)
                .with_allow(NetworkRule {
                    to: vec![NetworkPeer {
                        cidr: "10.0.0.0/30".to_owned(),
                        except: vec!["10.0.0.1".to_owned()],
                    }],
                    ports: vec![NetworkPort {
                        protocol: Protocol::Tcp,
                        port: Some(80),
                        end_port: Some(81),
                    }],
                })
                .with_deny(NetworkRule::to("192.0.2.7").on_protocol(Protocol::Icmp)),
            ..NetworkPolicy::deny_all()
        };
        let converted = network_policy(&policy, &[]);
        assert_eq!(converted.egress, network::Access::Deny);
        assert_eq!(converted.ingress, network::Access::Deny);
        assert_eq!(converted.host_loopback, network::HostLoopback::Deny);
        assert!(converted.host_loopback_forwards.is_empty());
        assert_eq!(converted.proxy, None);
        assert_eq!(converted.allow.name, "network.egress.allow");
        assert_eq!(converted.deny.name, "network.egress.deny");
        assert_eq!(
            converted.allow.rules,
            [network::EgressRule {
                to: vec![network::Destination {
                    cidr: "10.0.0.0/30".to_owned(),
                    except: vec!["10.0.0.1".to_owned()],
                }],
                ports: vec![network::PortSelector {
                    protocol: network::Protocol::Tcp,
                    port: Some(80),
                    end_port: Some(81),
                }],
            }]
        );
        assert_eq!(
            converted.deny.rules,
            [network::EgressRule::to("192.0.2.7").on_protocol(network::Protocol::Icmp)]
        );
        // Host-loopback access and the spec's forwards carry over.
        let mut loopback = policy.clone();
        loopback.ingress.host_loopback = Some(Access::Allow);
        let converted = network_policy(
            &loopback,
            &[HostLoopbackForward::new(ForwardProtocol::Udp, 18080, 8080)],
        );
        assert_eq!(converted.host_loopback, network::HostLoopback::Allow);
        assert_eq!(
            converted.host_loopback_forwards,
            [network::LoopbackForward {
                protocol: network::ForwardProtocol::Udp,
                host_port: 18080,
                guest_port: 8080,
            }]
        );
        // Messages name the contract's fields, as the shared lowering of NVX's egress policy
        // files words them.
        let icmp_port = NetworkPolicy {
            egress: EgressPolicy::new(Access::Deny)
                .with_allow(NetworkRule::to("10.0.0.0/8").on_port(Protocol::Icmp, 8)),
            ..NetworkPolicy::deny_all()
        };
        let error = Error::from(
            network::openvmm_arguments(Some(&network_policy(&icmp_port, &[])), "10.0.0.2/24")
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert_eq!(
            error.message(),
            "network.egress.allow[0].ports[0].port is not supported with icmp"
        );
        let scoped = NetworkPolicy {
            egress: EgressPolicy::new(Access::Allow)
                .with_deny(NetworkRule::to("192.0.2.0/24"))
                .with_deny(NetworkRule::to("fe80::1%eth0/64")),
            ..NetworkPolicy::deny_all()
        };
        let error = Error::from(
            network::openvmm_arguments(Some(&network_policy(&scoped, &[])), "10.0.0.2/24")
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert_eq!(
            error.message(),
            "network.egress.deny[1].to[0].cidr must not name an IPv6 scope: \"fe80::1%eth0/64\""
        );
        let mut crowded = NetworkRule::to("192.0.2.0/24");
        crowded.ports = (0..257)
            .map(|index| NetworkPort {
                protocol: Protocol::Tcp,
                port: Some(2 * index + 1),
                end_port: None,
            })
            .collect();
        let crowded = NetworkPolicy {
            egress: EgressPolicy::new(Access::Deny).with_allow(crowded),
            ..NetworkPolicy::deny_all()
        };
        let error = Error::from(
            network::openvmm_arguments(Some(&network_policy(&crowded, &[])), "10.0.0.2/24")
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert_eq!(
            error.message(),
            "network.egress.allow emits at most 256 native rules"
        );
    }

    #[test]
    fn filesystem_messages_name_the_contract_fields() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        let policy = FilesystemPolicy {
            readwrite_paths: vec![missing.clone()],
            ..FilesystemPolicy::default()
        };
        let error = export_plan(&policy).unwrap_err();
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert!(
            error
                .message()
                .starts_with("filesystem.readwritePaths entry"),
            "{error}"
        );
        let work = directory.path().join("work");
        fs::create_dir(&work).unwrap();
        let policy = FilesystemPolicy {
            readwrite_paths: vec![work.clone()],
            denied_paths: vec![work.join("not-yet")],
            ..FilesystemPolicy::default()
        };
        let error = export_plan(&policy).unwrap_err();
        assert!(
            error.message().starts_with("filesystem.deniedPaths entry"),
            "{error}"
        );
        // A read-only path inside a read-write directory names both entries.
        fs::create_dir(work.join("src")).unwrap();
        let policy = FilesystemPolicy {
            readonly_paths: vec![work.join("src")],
            readwrite_paths: vec![work.clone()],
            ..FilesystemPolicy::default()
        };
        let error = export_plan(&policy).unwrap_err();
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert!(
            error.message().starts_with(&format!(
                "filesystem.readonlyPaths entry {} lies inside the filesystem.readwritePaths \
                 entry {},",
                work.join("src").display(),
                work.display()
            )),
            "{error}"
        );
        assert_eq!(export_plan(&FilesystemPolicy::default()).unwrap(), None);
    }

    #[test]
    fn vms_gone_during_an_exec_leave_the_sandbox_not_started() {
        let gone = exec_error(nvx::Error::new(
            nvx::ErrorCode::SandboxExited,
            "sandbox aci-edge-sandboxes:00 stopped unexpectedly",
        ));
        assert_eq!(gone.code(), ErrorCode::NotStarted);
        assert_eq!(gone.nvx_code(), Some(nvx::ErrorCode::SandboxExited));
        assert_eq!(
            gone.message(),
            "sandbox aci-edge-sandboxes:00 stopped unexpectedly"
        );
        for (kind, code) in [
            (nvx::ErrorKind::TimedOut, ErrorCode::BackendError),
            (nvx::ErrorKind::Protocol, ErrorCode::BackendError),
            (nvx::ErrorKind::Unavailable, ErrorCode::BackendUnavailable),
        ] {
            assert_eq!(exec_error(nvx::Error::new(kind, "x")).code(), code);
        }
        // Elsewhere, a VM that exited is a backend failure.
        assert_eq!(
            Error::from(nvx::Error::exited("OpenVMM exited during startup")).code(),
            ErrorCode::BackendError
        );
    }

    #[test]
    fn reports_become_stop_metadata() {
        let report = Report {
            category: Some("success".to_owned()),
            status_code: Some(0),
            teardown_complete: Some(true),
        };
        let metadata = report_metadata(&report);
        assert_eq!(metadata["vmOutcome"], "success");
        assert_eq!(metadata["vmStatusCode"], 0);
        assert_eq!(metadata["teardownComplete"], true);
        assert!(report_metadata(&Report::default()).is_empty());
    }

    #[test]
    fn provisioning_without_artifacts_records_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let Some(hypervisor) = nvx::Hypervisor::platform_default() else {
            return;
        };
        let config = OpenVmmConfig::new(
            directory.path().join("openvmm"),
            directory.path().join("vmlinux"),
            directory.path().join("initrd"),
            hypervisor,
            directory.path().join("state"),
        );
        let backend = OpenVmmBackend::new(config).unwrap();
        let error = backend.provision(&ProvisionRequest::new()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendUnavailable);
        assert!(error.message().contains("not found"), "{error}");
    }
}
