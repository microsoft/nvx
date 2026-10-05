//! Image-backed guest lifecycle over the separately supplied nvxhost library.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use prost::Message;
use sha2_runtime::{Digest, Sha256};

use super::artifacts::absolute;
use super::config::{OpenVmmConfig, validate_unix_socket_path};
use super::platform;
use super::process;
use super::state::{
    BOOT_SOCKET_NAME, LaunchRecord, NATIVE_BACKEND_KEY, NativeArtifactRecord, ProcessIdentity,
    RuntimeRecord, SOCKET_NAME, STATE_FORMAT, SandboxRecord, StateStore, remove_if_present,
};
use super::{OpenVmmBackend, RunState};
use crate::backend::{Backend, ExecControl, ExecIo};
use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::exec::{Completion, ExecOutcome};
use crate::id::SandboxId;
use crate::model::{
    Access, Command, DeprovisionResult, ExecRequest, Metadata, ProvisionRequest, ProvisionResult,
    StartResult, StdinMode, StopResult,
};
use crate::nvxhost::{HostLibrary, LaunchInputs, Session};

const BACKEND_KEY: &str = NATIVE_BACKEND_KEY;
const RUNTIME_ABI: &str = "microvm-abi-v2-edge-ramfs-v1";
const MAX_EXEC_SECONDS: u64 = 3600;
const MAX_OUTPUT_BYTES: usize = 1 << 20;
const SHELL: &str = "/bin/sh";

/// Artifact paths and approved native-library digest for an image-backed guest.
///
/// The image is a caller-prepared GPT disk, not a container image reference. This backend
/// creates no disks, filesystem mappings, or network devices.
#[derive(Debug, Clone)]
pub struct NvxHostConfig {
    /// OpenVMM and guest boot artifacts, hypervisor, state root, and deadlines.
    pub openvmm: OpenVmmConfig,
    /// GPT image attached read-only as the distro block device.
    pub image: PathBuf,
    /// Absolute path to a separately installed nvxhost DLL or shared library.
    pub library: PathBuf,
    /// SHA-256 approved by the caller's independent artifact policy.
    pub library_sha256: [u8; 32],
    /// Requests verbose guest kernel diagnostics on the boot console.
    pub guest_debug: bool,
}

impl NvxHostConfig {
    /// Uses caller-provided artifacts and an independently supplied native-library digest.
    pub fn new(
        openvmm: OpenVmmConfig,
        image: impl Into<PathBuf>,
        library: impl Into<PathBuf>,
        library_sha256: [u8; 32],
    ) -> Self {
        Self {
            openvmm,
            image: image.into(),
            library: library.into(),
            library_sha256,
            guest_debug: false,
        }
    }

    /// Enables verbose guest boot diagnostics without changing the guest lifecycle.
    #[must_use]
    pub fn with_guest_debug(mut self, enabled: bool) -> Self {
        self.guest_debug = enabled;
        self
    }
}

/// Owns sandbox state and OpenVMM processes; the private native library owns only
/// the launch arguments and authenticated guest channel.
#[derive(Debug)]
pub struct NvxHostBackend {
    config: NvxHostConfig,
    base: OpenVmmBackend,
    host: Arc<HostLibrary>,
    console_pumps: Mutex<HashMap<SandboxId, thread::JoinHandle<Result<()>>>>,
}

#[derive(Clone, PartialEq, Message)]
struct GuestInfo {
    #[prost(int32, tag = "10")]
    readiness: i32,
    #[prost(uint64, tag = "11")]
    readiness_generation: u64,
    #[prost(string, tag = "15")]
    agent_build_id: String,
    #[prost(string, tag = "16")]
    runtime_abi: String,
    #[prost(string, tag = "17")]
    startup_error: String,
}

#[derive(Clone, PartialEq, Message)]
struct WaitReadyRequest {
    #[prost(uint64, tag = "1")]
    minimum_generation: u64,
}

#[derive(Clone, PartialEq, Message)]
struct WaitReadyResponse {
    #[prost(int32, tag = "1")]
    readiness: i32,
    #[prost(uint64, tag = "2")]
    generation: u64,
    #[prost(string, tag = "3")]
    startup_error: String,
}

#[derive(Clone, PartialEq, Message)]
struct ExecuteCommandRequest {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, repeated, tag = "2")]
    args: Vec<String>,
    #[prost(int32, tag = "5")]
    timeout_seconds: i32,
}

#[derive(Clone, PartialEq, Message)]
struct ExecuteCommandResponse {
    #[prost(int32, tag = "1")]
    exit_code: i32,
    #[prost(string, tag = "2")]
    stdout: String,
    #[prost(string, tag = "3")]
    stderr: String,
    #[prost(bool, tag = "4")]
    timed_out: bool,
}

#[derive(Clone, PartialEq, Message)]
struct ShutdownRequest {
    #[prost(int64, tag = "1")]
    grace_period_milliseconds: i64,
}

#[derive(Clone, PartialEq, Message)]
struct ShutdownResponse {}

#[derive(Clone, PartialEq, Message)]
struct StreamLogsRequest {
    #[prost(string, tag = "1")]
    container_id: String,
    #[prost(uint64, tag = "2")]
    offset: u64,
    #[prost(bool, tag = "3")]
    follow: bool,
    #[prost(uint32, tag = "4")]
    max_chunk_bytes: u32,
}

#[derive(Clone, PartialEq, Message)]
struct LogChunk {
    #[prost(string, tag = "1")]
    container_id: String,
    #[prost(uint64, tag = "2")]
    offset: u64,
    #[prost(bytes, tag = "3")]
    data: Vec<u8>,
    #[prost(uint64, tag = "4")]
    next_offset: u64,
    #[prost(uint64, tag = "5")]
    earliest_retained_offset: u64,
    #[prost(bool, tag = "6")]
    loss: bool,
}

struct ExecJob {
    host: Arc<HostLibrary>,
    runtime: RuntimeRecord,
    capability: [u8; 32],
    config: OpenVmmConfig,
    command: ExecuteCommandRequest,
    timeout: Option<Duration>,
    io: ExecIo,
}

impl NvxHostBackend {
    /// Opens the state root and verifies the explicitly supplied native asset.
    pub fn new(mut config: NvxHostConfig) -> Result<Self> {
        config.openvmm = config.openvmm.normalized()?;
        config.image = absolute(&config.image)?;
        config.library = absolute(&config.library)?;
        let store_root = config.openvmm.state_root.join(BACKEND_KEY);
        validate_unix_socket_path(&store_root, SOCKET_NAME, "control")?;
        validate_unix_socket_path(&store_root, BOOT_SOCKET_NAME, "boot-console")?;
        let host = HostLibrary::load(&config.library, &config.library_sha256)?;
        let store = StateStore::open_for(&store_root, BACKEND_KEY).map_err(|error| {
            Error::backend_unavailable(format!(
                "cannot open native sandbox state root {}",
                store_root.display()
            ))
            .with_source(error)
        })?;
        let base = OpenVmmBackend {
            config: config.openvmm.clone(),
            store,
        };
        Ok(Self {
            config,
            base,
            host,
            console_pumps: Mutex::new(HashMap::new()),
        })
    }

    /// Returns the normalized configuration.
    pub fn config(&self) -> &NvxHostConfig {
        &self.config
    }

    /// Returns the OpenVMM diagnostic log for this sandbox.
    pub fn log_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.base.log_path(sandbox_id)
    }

    /// Returns the guest boot-console log, including startup errors before ttrpc is ready.
    pub fn console_log_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.base.store.console_path(sandbox_id)
    }

    /// Returns OpenVMM's bounded outcome report path for the latest launch.
    pub fn outcome_report_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.base.store.outcome_path(sandbox_id)
    }

    /// Collects the guest's bounded, non-follow log snapshot through ttrpc.
    pub fn guest_logs(&self, sandbox_id: &SandboxId) -> Result<Vec<u8>> {
        let (runtime, capability) = self.running(sandbox_id)?;
        let deadline = Instant::now() + self.config.openvmm.control_timeout;
        let mut session = self.connect(
            &runtime,
            &capability,
            deadline.saturating_duration_since(Instant::now()),
        )?;
        let mut output = Vec::new();
        let mut offset = 0u64;
        let request = StreamLogsRequest {
            container_id: "guest".to_owned(),
            offset,
            follow: false,
            max_chunk_bytes: 64 * 1024,
        };
        let streamed = session.server_stream(
            "StreamLogs",
            &request.encode_to_vec(),
            deadline.saturating_duration_since(Instant::now()),
            |bytes| {
                let chunk = LogChunk::decode(bytes).map_err(|error| {
                    Error::backend_error("the guest returned an invalid log chunk")
                        .with_source(error)
                })?;
                let expected_next = offset
                    .checked_add(chunk.data.len() as u64)
                    .ok_or_else(|| Error::backend_error("guest log cursor overflowed"))?;
                if chunk.container_id != "guest"
                    || chunk.loss
                    || chunk.offset != offset
                    || chunk.next_offset != expected_next
                    || chunk.earliest_retained_offset > offset
                {
                    return Err(Error::backend_error(
                        "the guest returned a missing or out-of-order log chunk",
                    ));
                }
                if output
                    .len()
                    .checked_add(chunk.data.len())
                    .is_none_or(|size| size > MAX_OUTPUT_BYTES)
                {
                    return Err(Error::backend_error(
                        "guest logs exceed the one-megabyte collection limit",
                    ));
                }
                offset = chunk.next_offset;
                output.extend_from_slice(&chunk.data);
                Ok(())
            },
        );
        finish_session(session, streamed, Some(deadline))?;
        Ok(output)
    }

    fn boot_endpoint(&self, sandbox_id: &SandboxId, control: &str) -> Result<String> {
        if cfg!(windows) {
            let suffix = control
                .strip_prefix("//./pipe/openvmm-microvm-")
                .ok_or_else(|| {
                    Error::backend_error("the recorded control pipe is not canonical")
                })?;
            if suffix.is_empty()
                || !suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                return Err(Error::backend_error(
                    "the recorded control pipe has an invalid name",
                ));
            }
            return Ok(format!("{control}-boot"));
        }
        self.base
            .store
            .boot_socket_path(sandbox_id)
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::backend_unavailable("the boot-console socket path is not UTF-8"))
    }

    fn start_console_pump(&self, sandbox_id: &SandboxId, runtime: &RuntimeRecord) -> Result<()> {
        let mut pumps = self
            .console_pumps
            .lock()
            .map_err(|_| Error::backend_error("the boot-console pump table is unavailable"))?;
        if pumps.contains_key(sandbox_id) {
            return Ok(());
        }
        let endpoint = self.boot_endpoint(sandbox_id, &runtime.endpoint)?;
        let file = self.base.store.open_console_log(sandbox_id)?;
        let runtime = runtime.clone();
        let timeout = self.config.openvmm.start_timeout;
        let pump = thread::Builder::new()
            .name("nvxhost-boot-console".to_owned())
            .spawn(move || pump_console(runtime, endpoint, file, timeout))
            .map_err(|error| {
                Error::backend_error("cannot start the guest boot-console listener")
                    .with_source(error)
            })?;
        pumps.insert(sandbox_id.clone(), pump);
        Ok(())
    }

    fn finish_console_pump(&self, sandbox_id: &SandboxId) -> Result<()> {
        let pump = self
            .console_pumps
            .lock()
            .map_err(|_| Error::backend_error("the boot-console pump table is unavailable"))?
            .remove(sandbox_id);
        if let Some(pump) = pump {
            pump.join()
                .map_err(|_| Error::backend_error("the boot-console listener panicked"))??;
        }
        Ok(())
    }

    fn artifact_record(&self) -> Result<NativeArtifactRecord> {
        Ok(NativeArtifactRecord {
            image: self.config.image.clone(),
            image_sha256: file_digest(&self.config.image)?,
            kernel_sha256: file_digest(&self.config.openvmm.kernel)?,
            initrd_sha256: file_digest(&self.config.openvmm.initrd)?,
            library_sha256: encode_digest(&self.config.library_sha256),
        })
    }

    fn verify_record(&self, record: &SandboxRecord, for_launch: bool) -> Result<()> {
        let recorded = record.native.as_ref().ok_or_else(|| {
            Error::backend_error("the native sandbox has no pinned artifact identity")
        })?;
        if recorded.image != self.config.image
            || recorded.library_sha256 != encode_digest(&self.config.library_sha256)
        {
            return Err(Error::backend_unavailable(
                "the native sandbox's image or host library differs from its provisioned identity",
            ));
        }
        if for_launch && *recorded != self.artifact_record()? {
            return Err(Error::backend_unavailable(
                "the native sandbox's boot artifacts changed since provision",
            ));
        }
        Ok(())
    }

    fn running(&self, sandbox_id: &SandboxId) -> Result<(RuntimeRecord, [u8; 32])> {
        let (_guard, record) = self.base.store.lock_and_load(sandbox_id)?;
        self.verify_record(&record, false)?;
        match self.base.reconcile(sandbox_id)? {
            RunState::Provisioned => Err(Error::not_started(format!(
                "sandbox {sandbox_id} is not running"
            ))),
            RunState::Running(runtime) => {
                let capability = self.base.store.read_capability(sandbox_id)?;
                self.start_console_pump(sandbox_id, &runtime)?;
                Ok((runtime, capability))
            }
        }
    }

    fn connect(
        &self,
        runtime: &RuntimeRecord,
        capability: &[u8; 32],
        timeout: Duration,
    ) -> Result<Session> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::backend_error("control timeout is too long"))?;
        Session::connect_verified(
            Arc::clone(&self.host),
            &runtime.endpoint,
            capability,
            runtime.pid,
            deadline,
            || {
                platform::process_start_time(runtime.pid)
                    .map(|current| current == Some(runtime.start_time))
                    .map_err(|error| {
                        Error::backend_error("cannot verify the running OpenVMM process")
                            .with_source(error)
                    })
            },
        )
    }

    fn wait_ready(&self, session: &mut Session, deadline: Instant) -> Result<String> {
        let remaining = || {
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                Err(Error::backend_error(
                    "the guest did not become ready before the start deadline",
                ))
            } else {
                Ok(timeout)
            }
        };
        let info = GuestInfo::decode(
            session
                .unary("GetGuestInfo", &[], Some(remaining()?))?
                .as_slice(),
        )
        .map_err(|error| {
            Error::backend_error("the guest returned malformed identity data").with_source(error)
        })?;
        if info.runtime_abi != RUNTIME_ABI || info.agent_build_id.is_empty() {
            return Err(Error::backend_unavailable(format!(
                "the guest reported runtime ABI {:?}, expected {RUNTIME_ABI}",
                info.runtime_abi
            )));
        }
        if info.readiness == 3 || info.readiness == 2 {
            return Err(Error::backend_error(format!(
                "the guest cannot become ready: {}",
                info.startup_error
            )));
        }
        let ready = WaitReadyResponse::decode(
            session
                .unary(
                    "WaitReady",
                    &WaitReadyRequest {
                        minimum_generation: info.readiness_generation,
                    }
                    .encode_to_vec(),
                    Some(remaining()?),
                )?
                .as_slice(),
        )
        .map_err(|error| {
            Error::backend_error("the guest returned malformed readiness data").with_source(error)
        })?;
        if ready.readiness != 1 || ready.generation < info.readiness_generation {
            return Err(Error::backend_error(format!(
                "the guest did not reach the required readiness generation: {}",
                ready.startup_error
            )));
        }
        Ok(info.agent_build_id)
    }
}

impl Backend for NvxHostBackend {
    fn name(&self) -> &str {
        BACKEND_KEY
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn probe(&self) -> Result<()> {
        self.base.probe()?;
        if !self.config.image.is_file() {
            return Err(Error::backend_unavailable(format!(
                "NVX image not found: {}",
                self.config.image.display()
            )));
        }
        Ok(())
    }

    fn validate_provision(&self, request: &ProvisionRequest) -> Result<()> {
        let memory = request
            .microvm
            .provision
            .memory_mib
            .unwrap_or(self.config.openvmm.memory_mib);
        if memory == 0 || memory > i32::MAX as u32 {
            return Err(Error::policy_validation(
                "microvm.provision.memoryMib must fit a positive signed 32-bit integer",
            ));
        }
        if request
            .filesystem
            .as_ref()
            .is_some_and(|policy| !policy.is_empty())
            || request.network.as_ref().is_some_and(|policy| {
                policy.egress.default != Access::Deny
                    || policy.ingress.default != Access::Deny
                    || policy.ingress.host_loopback == Some(Access::Allow)
                    || !policy.egress.allow.is_empty()
                    || !policy.egress.deny.is_empty()
            })
        {
            return Err(Error::policy_validation(
                "the nvxhost backend currently supports no host filesystem or guest network",
            ));
        }
        Ok(())
    }

    fn validate_exec(&self, request: &ExecRequest) -> Result<()> {
        prepare_exec(request).map(|_| ())
    }

    fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionResult> {
        self.validate_provision(request)?;
        self.probe()?;
        let record = SandboxRecord {
            format: STATE_FORMAT,
            backend: BACKEND_KEY.to_owned(),
            network: None,
            filesystem: None,
            native: Some(self.artifact_record()?),
            memory_mib: request
                .microvm
                .provision
                .memory_mib
                .unwrap_or(self.config.openvmm.memory_mib),
            workload_uid: 65534,
            workload_gid: 65534,
            create_workload_account: false,
            hostname: self.config.openvmm.hostname.clone(),
        };
        let sandbox_id = SandboxId::generate()?;
        self.base.store.create(&sandbox_id, &record)?;
        Ok(ProvisionResult {
            sandbox_id,
            metadata: None,
        })
    }

    fn start(&self, sandbox_id: &SandboxId) -> Result<StartResult> {
        let (_guard, record) = self.base.store.lock_and_load(sandbox_id)?;
        if let RunState::Running(_) = self.base.reconcile(sandbox_id)? {
            return Err(Error::already_started(format!(
                "sandbox {sandbox_id} is already running"
            )));
        }
        self.verify_record(&record, true)?;
        self.probe()?;

        let mut capability = [0u8; 32];
        while capability == [0; 32] {
            getrandom::fill(&mut capability).map_err(|error| {
                Error::backend_error("cannot generate a broker capability").with_source(error)
            })?;
        }
        let socket = self.base.store.socket_path(sandbox_id);
        let boot_socket = self.base.store.boot_socket_path(sandbox_id);
        remove_if_present(&socket)?;
        remove_if_present(&boot_socket)?;
        let endpoint = platform::control_endpoint(&socket).map_err(|error| {
            Error::backend_error("cannot choose a control endpoint").with_source(error)
        })?;
        let boot = self.boot_endpoint(sandbox_id, &endpoint)?;
        let mut arguments = self.host.launch_arguments(&LaunchInputs {
            kernel: &self.config.openvmm.kernel,
            initrd: &self.config.openvmm.initrd,
            image: &self.config.image,
            control: &endpoint,
            boot: &boot,
            hypervisor: self.config.openvmm.hypervisor.as_str(),
            memory_mb: record.memory_mib,
            guest_debug: self.config.guest_debug,
        })?;
        let report = self.base.store.outcome_path(sandbox_id);
        remove_if_present(&report)?;
        arguments.push("--microvm-report".into());
        arguments.push(report.into_os_string());

        self.base.store.write_capability(sandbox_id, &capability)?;
        let log = self.base.store.create_log(sandbox_id)?;
        self.base.store.write_launch(
            sandbox_id,
            &LaunchRecord {
                format: STATE_FORMAT,
                endpoint: endpoint.clone(),
                process: None,
            },
        )?;
        let started = Instant::now();
        let child = match process::spawn(
            &self.config.openvmm,
            &arguments,
            &capability,
            log,
            &self.base.store.dir(sandbox_id),
        ) {
            Ok(child) => child,
            Err(error) => {
                self.base.store.clear_runtime(sandbox_id)?;
                return Err(Error::backend_error("cannot launch OpenVMM").with_source(error));
            }
        };
        let pid = child.id();
        let start_time = match platform::process_start_time(pid) {
            Ok(Some(value)) => value,
            other => {
                if process::kill_child(child) {
                    self.base.store.clear_runtime(sandbox_id)?;
                }
                return Err(self.base.start_failure(
                    sandbox_id,
                    &format!("cannot identify OpenVMM process {pid}: {other:?}"),
                ));
            }
        };
        let runtime = RuntimeRecord {
            format: STATE_FORMAT,
            pid,
            start_time,
            endpoint: endpoint.clone(),
        };
        if let Err(error) = self
            .base
            .store
            .write_launch(
                sandbox_id,
                &LaunchRecord {
                    format: STATE_FORMAT,
                    endpoint,
                    process: Some(ProcessIdentity { pid, start_time }),
                },
            )
            .and_then(|()| self.base.store.write_runtime(sandbox_id, &runtime))
        {
            if process::kill_child(child) {
                self.base.store.clear_runtime(sandbox_id)?;
            }
            return Err(error);
        }
        process::detach_child(child);
        self.base.store.remove_launch(sandbox_id);
        if let Err(error) = self.start_console_pump(sandbox_id, &runtime) {
            return Err(self
                .base
                .abort_start(sandbox_id, &runtime, &error.to_string()));
        }
        let ready = (|| {
            let deadline = started
                .checked_add(self.config.openvmm.start_timeout)
                .ok_or_else(|| Error::backend_error("start timeout is too long"))?;
            let mut session = self.connect(
                &runtime,
                &capability,
                deadline.saturating_duration_since(Instant::now()),
            )?;
            let result = self.wait_ready(&mut session, deadline);
            finish_session(session, result, Some(deadline))
        })();
        let agent = match ready {
            Ok(agent) => agent,
            Err(error) => {
                let failure = self
                    .base
                    .abort_start(sandbox_id, &runtime, &error.to_string());
                match platform::process_start_time(runtime.pid) {
                    Ok(Some(current)) if current == runtime.start_time => return Err(failure),
                    Ok(_) => {}
                    Err(check) => {
                        return Err(Error::backend_error(format!(
                            "{failure}; cannot verify boot-console cleanup: {check}"
                        )));
                    }
                }
                return match self.finish_console_pump(sandbox_id) {
                    Ok(()) => Err(failure),
                    Err(console) => Err(Error::backend_error(format!(
                        "{failure}; boot-console listener also failed: {console}"
                    ))),
                };
            }
        };
        let mut metadata = Metadata::new();
        let boot_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        metadata.insert("bootMilliseconds".to_owned(), boot_ms.into());
        metadata.insert("guestBuildId".to_owned(), agent.into());
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
        if request.stdin == StdinMode::Piped || io.stdin.is_some() {
            return Err(Error::policy_validation(
                "the nvxhost backend does not support piped stdin",
            ));
        }
        let (message, timeout) = prepare_exec(request)?;
        let (runtime, capability) = self.running(sandbox_id)?;
        let shared = Arc::new(Completion::default());
        let result = Arc::clone(&shared);
        let job = ExecJob {
            host: Arc::clone(&self.host),
            runtime,
            capability,
            config: self.config.openvmm.clone(),
            command: message,
            timeout,
            io,
        };
        thread::Builder::new()
            .name("nvxhost-exec".to_owned())
            .spawn(move || {
                let outcome = execute(job);
                result.finish(outcome);
            })
            .map_err(|error| {
                Error::backend_error("cannot start an execution thread").with_source(error)
            })?;
        Ok(Box::new(NvxHostExecution { shared }))
    }

    fn stop(&self, sandbox_id: &SandboxId) -> Result<StopResult> {
        let (_guard, record) = self.base.store.lock_and_load(sandbox_id)?;
        self.verify_record(&record, false)?;
        let RunState::Running(runtime) = self.base.reconcile(sandbox_id)? else {
            return Err(Error::already_stopped(format!(
                "sandbox {sandbox_id} is not running"
            )));
        };
        let deadline = Instant::now() + self.config.openvmm.stop_timeout;
        let graceful = (|| {
            let capability = self.base.store.read_capability(sandbox_id)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut session = self.connect(&runtime, &capability, remaining)?;
            let grace_period_milliseconds = i64::try_from(remaining.as_millis())
                .map_err(|_| Error::backend_error("guest shutdown grace period is too long"))?;
            let response = session.unary(
                "Shutdown",
                &ShutdownRequest {
                    grace_period_milliseconds,
                }
                .encode_to_vec(),
                Some(remaining),
            );
            let response = finish_session(session, response, Some(deadline))?;
            ShutdownResponse::decode(response.as_slice()).map_err(|error| {
                Error::backend_error("the guest returned an invalid shutdown response")
                    .with_source(error)
            })?;
            if process::wait_for_exit(runtime.pid, runtime.start_time, deadline) {
                Ok(())
            } else {
                Err(Error::backend_error("guest shutdown timed out"))
            }
        })();
        let needs_force = graceful.is_err();
        let forced = needs_force
            && platform::process_start_time(runtime.pid).map_err(|error| {
                Error::backend_error("cannot check OpenVMM before forced stop").with_source(error)
            })? == Some(runtime.start_time);
        if forced
            && !process::kill(runtime.pid, runtime.start_time).map_err(|error| {
                Error::backend_error(format!("cannot terminate OpenVMM process {}", runtime.pid))
                    .with_source(error)
            })?
        {
            return Err(Error::backend_error(format!(
                "OpenVMM process {} did not terminate",
                runtime.pid
            )));
        }
        let mut metadata = Metadata::new();
        metadata.insert("forced".to_owned(), forced.into());
        if let Err(error) = graceful {
            metadata.insert("gracefulError".to_owned(), error.to_string().into());
        }
        let console = self.finish_console_pump(sandbox_id);
        self.base.store.clear_runtime(sandbox_id)?;
        console?;
        Ok(StopResult {
            metadata: Some(metadata),
        })
    }

    fn deprovision(&self, sandbox_id: &SandboxId) -> Result<DeprovisionResult> {
        let (guard, record) = self.base.store.lock_and_load(sandbox_id)?;
        self.verify_record(&record, false)?;
        if let RunState::Running(_) = self.base.reconcile(sandbox_id)? {
            return Err(Error::already_started(format!(
                "sandbox {sandbox_id} is running; stop it before deprovisioning"
            )));
        }
        self.finish_console_pump(sandbox_id)?;
        self.base.store.remove(sandbox_id)?;
        drop(guard);
        self.base.store.remove_lock(sandbox_id);
        Ok(DeprovisionResult::default())
    }
}

fn capabilities() -> Capabilities {
    let mut capabilities = Capabilities::new(BACKEND_KEY);
    capabilities.exec.command_line = true;
    capabilities.exec.argv = true;
    capabilities.exec.max_timeout_ms = Some(MAX_EXEC_SECONDS * 1_000);
    capabilities.exec.max_output_bytes = Some(MAX_OUTPUT_BYTES as u64);
    capabilities.network.egress_deny = true;
    capabilities.network.ingress_deny = true;
    capabilities.network.host_loopback_deny = true;
    capabilities
}

fn encode_digest(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn file_digest(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|error| {
        Error::backend_unavailable(format!("cannot open {}", path.display())).with_source(error)
    })?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(|error| {
        Error::backend_unavailable(format!("cannot hash {}", path.display())).with_source(error)
    })?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn finish_session<T>(session: Session, result: Result<T>, deadline: Option<Instant>) -> Result<T> {
    match (result, session.close(deadline)) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(close)) => {
            eprintln!("nvxhost could not close a failed guest session: {close}");
            Err(error)
        }
    }
}

fn pump_console(
    runtime: RuntimeRecord,
    endpoint: String,
    mut output: File,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| Error::backend_error("boot-console timeout is too long"))?;
    let mut console = loop {
        if Instant::now() >= deadline {
            return Err(Error::backend_error(
                "OpenVMM did not open its guest boot-console endpoint",
            ));
        }
        match platform::connect_endpoint(&endpoint, runtime.pid, Duration::from_millis(250)) {
            Ok(console) => break console,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                if platform::process_start_time(runtime.pid).map_err(|error| {
                    Error::backend_error("cannot verify OpenVMM while connecting the console")
                        .with_source(error)
                })? != Some(runtime.start_time)
                {
                    return Err(Error::backend_error(
                        "OpenVMM exited before the guest boot console could connect",
                    ));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                return Err(
                    Error::backend_error("cannot connect the guest boot console")
                        .with_source(error),
                );
            }
        }
    };
    let mut buffer = [0u8; 4096];
    loop {
        match console.read(&mut buffer, Some(Duration::from_millis(250))) {
            Ok(0) => break,
            Ok(size) => output.write_all(&buffer[..size]).map_err(|error| {
                Error::backend_error("cannot write the guest boot-console log").with_source(error)
            })?,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                if platform::process_start_time(runtime.pid).map_err(|error| {
                    Error::backend_error("cannot verify OpenVMM while reading the console")
                        .with_source(error)
                })? != Some(runtime.start_time)
                {
                    break;
                }
            }
            Err(error) => {
                return Err(
                    Error::backend_error("cannot read the guest boot console").with_source(error)
                );
            }
        }
    }
    output.sync_all().map_err(|error| {
        Error::backend_error("cannot flush the guest boot-console log").with_source(error)
    })
}

fn prepare_exec(request: &ExecRequest) -> Result<(ExecuteCommandRequest, Option<Duration>)> {
    if request.stdin == StdinMode::Piped
        || request.process.cwd.is_some()
        || request.process.env.is_some()
    {
        return Err(Error::policy_validation(
            "the nvxhost backend supports no piped stdin, custom working directory, or environment",
        ));
    }
    let argv = match &request.process.command {
        Command::CommandLine(line) => vec![SHELL.to_owned(), "-c".to_owned(), line.clone()],
        Command::Argv(argv) => argv.clone(),
    };
    if argv.is_empty()
        || argv.iter().any(|argument| argument.contains('\0'))
        || argv.iter().map(String::len).sum::<usize>() > 4096
    {
        return Err(Error::policy_validation(
            "the guest command must contain non-NUL arguments totaling at most 4096 bytes",
        ));
    }
    let timeout = request.process.timeout.filter(|timeout| !timeout.is_zero());
    if let Some(timeout) = timeout {
        if timeout > Duration::from_secs(MAX_EXEC_SECONDS) {
            return Err(Error::policy_validation(
                "the guest command timeout exceeds one hour",
            ));
        }
        if timeout.subsec_nanos() != 0 {
            return Err(Error::policy_validation(
                "the nvxhost backend requires whole-second precision for positive command timeouts",
            ));
        }
    }
    let seconds = timeout.map_or(0, |duration| duration.as_secs() as i32);
    Ok((
        ExecuteCommandRequest {
            command: argv[0].clone(),
            args: argv[1..].to_vec(),
            timeout_seconds: seconds,
        },
        timeout,
    ))
}

fn execute(job: ExecJob) -> Result<ExecOutcome> {
    let ExecJob {
        host,
        runtime,
        capability,
        config,
        command,
        timeout,
        mut io,
    } = job;
    let mut session = Session::connect_verified(
        host,
        &runtime.endpoint,
        &capability,
        runtime.pid,
        Instant::now() + config.control_timeout,
        || {
            platform::process_start_time(runtime.pid)
                .map(|current| current == Some(runtime.start_time))
                .map_err(|error| {
                    Error::backend_error("cannot verify the running OpenVMM process")
                        .with_source(error)
                })
        },
    )?;
    let deadline = timeout.map(|timeout| Instant::now() + timeout + config.exec_response_grace);
    let response = session.unary(
        "ExecuteCommand",
        &command.encode_to_vec(),
        deadline.map(|deadline| deadline.saturating_duration_since(Instant::now())),
    );
    let response = finish_session(session, response, deadline)?;
    let reply = ExecuteCommandResponse::decode(response.as_slice()).map_err(|error| {
        Error::backend_error("the guest returned an invalid execution result").with_source(error)
    })?;
    let _ = io.stdout.write(reply.stdout.as_bytes());
    let _ = io.stderr.write(reply.stderr.as_bytes());
    drop(io);
    if reply.timed_out {
        Ok(ExecOutcome::TimedOut)
    } else {
        Ok(ExecOutcome::Exited(reply.exit_code))
    }
}

struct NvxHostExecution {
    shared: Arc<Completion>,
}

impl ExecControl for NvxHostExecution {
    fn wait(&self) -> Result<ExecOutcome> {
        self.shared.wait()
    }

    fn cancel(&self) -> Result<()> {
        Err(Error::unsupported(
            "the nvxhost backend does not support canceling an active guest command",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    #[test]
    fn simple_exec_has_no_container_metadata_and_respects_timeout() {
        let (request, timeout) = prepare_exec(
            &ExecRequest::command_line("printf READY").with_timeout(Duration::from_secs(2)),
        )
        .unwrap();
        assert_eq!(request.command, SHELL);
        assert_eq!(request.args, ["-c", "printf READY"]);
        assert_eq!(request.timeout_seconds, 2);
        assert_eq!(timeout, Some(Duration::from_secs(2)));
        assert!(request.encode_to_vec().len() < 100);
        assert!(capabilities().exec.command_line);
        assert!(!capabilities().exec.cancel);
        assert!(!capabilities().filesystem.readonly_paths);
    }

    #[test]
    fn fractional_second_exec_timeouts_are_rejected_but_zero_is_unbounded() {
        let (unbounded, timeout) =
            prepare_exec(&ExecRequest::command_line("true").with_timeout(Duration::ZERO)).unwrap();
        assert_eq!(unbounded.timeout_seconds, 0);
        assert_eq!(timeout, None);
        for timeout in [
            Duration::from_nanos(1),
            Duration::from_millis(1),
            Duration::from_millis(999),
            Duration::from_millis(1001),
        ] {
            let error =
                prepare_exec(&ExecRequest::command_line("true").with_timeout(timeout)).unwrap_err();
            assert_eq!(error.code(), ErrorCode::PolicyValidation);
            assert!(error.message().contains("whole-second precision"));
        }
        assert_eq!(
            prepare_exec(
                &ExecRequest::command_line("true")
                    .with_timeout(Duration::from_secs(MAX_EXEC_SECONDS + 1)),
            )
            .unwrap_err()
            .code(),
            ErrorCode::PolicyValidation,
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_socket_paths_are_checked_before_loading_the_library() {
        let suffix = PathBuf::from("0".repeat(32)).join(SOCKET_NAME);
        let target_root_len = 107 - 1 - suffix.as_os_str().len();
        let state_root = PathBuf::from("/").join("x".repeat(target_root_len - 1));
        validate_unix_socket_path(&state_root, SOCKET_NAME, "control").unwrap();
        let native_root = state_root.join(BACKEND_KEY);
        assert!(validate_unix_socket_path(&native_root, BOOT_SOCKET_NAME, "boot-console").is_err());
        let config = OpenVmmConfig::new(
            "/tmp/openvmm",
            "/tmp/vmlinux",
            "/tmp/initramfs",
            super::super::Hypervisor::Kvm,
            &state_root,
        );
        let error = NvxHostBackend::new(NvxHostConfig::new(
            config,
            "/tmp/image.vhd",
            "/tmp/libnvxhost.so",
            [0; 32],
        ))
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendUnavailable);
        assert!(error.message().contains("Unix control socket path"));
        assert!(!native_root.exists());
    }

    #[test]
    fn unsupported_exec_options_fail_before_guest_work() {
        let request = ExecRequest::command_line("echo READY").with_cwd("/tmp");
        assert_eq!(
            prepare_exec(&request).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
        let request = ExecRequest::command_line("echo READY").with_env("K=V");
        assert_eq!(
            prepare_exec(&request).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
    }
}
