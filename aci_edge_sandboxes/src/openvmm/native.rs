//! Image-backed guest lifecycle over the separately supplied nvxhost library.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use prost::Message;

use super::artifacts::absolute;
use super::config::{OpenVmmConfig, validate_unix_socket_path};
use super::filesystem;
use super::images::{
    ImageDigest, ImageId, ImageRecord, ImageStore, RegisteredImage, VerifiedFile, encode_hex,
};
use super::platform::{self, Transport};
use super::process;
use super::state::{
    BOOT_SOCKET_NAME, IMAGES_NAME, LaunchRecord, NATIVE_BACKEND_KEY, NativeArtifactRecord,
    ProcessIdentity, RuntimeRecord, SOCKET_NAME, STATE_FORMAT, SandboxRecord, StateStore,
    remove_if_present,
};
use super::{OpenVmmBackend, RunState};
use crate::backend::{Backend, ExecControl, ExecIo};
use crate::capabilities::Capabilities;
use crate::error::{Error, ErrorCode, Result};
use crate::exec::{Completion, ExecOutcome};
use crate::id::SandboxId;
use crate::model::{
    Command, DeprovisionResult, ExecRequest, FilesystemPolicy, Metadata, NetworkPolicy,
    ProvisionRequest, ProvisionResult, StartResult, StdinMode, StopResult,
};
use crate::nvxhost::{HostLibrary, LaunchInputs, Session};

const BACKEND_KEY: &str = NATIVE_BACKEND_KEY;
const RUNTIME_ABI: &str = "microvm-abi-v2-edge-ramfs-v2";
const MAX_EXEC_SECONDS: u64 = 3600;
const MAX_OUTPUT_BYTES: usize = 1 << 20;
const SHELL: &str = "/bin/sh";
const CONSOLE_POLL: Duration = Duration::from_millis(250);

/// Artifact paths and approved native-library digest for an image-backed guest.
///
/// The image is a caller-prepared GPT disk, not a container image reference. This backend
/// creates no disks; it maps host paths and attaches a network device as each sandbox's policy
/// requests. Construct it with [`NvxHostConfig::new`] and its builder methods, which keep
/// callers compatible as options are added.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NvxHostConfig {
    /// OpenVMM and guest boot artifacts, hypervisor, state root, and deadlines.
    pub openvmm: OpenVmmConfig,
    /// GPT image attached read-only as the distro block device.
    ///
    /// The backend registers the image when it is created, and the sandboxes that it provisions
    /// refer to the image by its [`ImageId`].
    pub image: PathBuf,
    /// How the backend establishes the content digest of `image` when it registers it.
    pub image_digest: ImageDigest,
    /// Absolute path to a separately installed nvxhost DLL or shared library.
    pub library: PathBuf,
    /// SHA-256 approved by the caller's independent artifact policy.
    pub library_sha256: [u8; 32],
    /// SHA-256 digests of the OpenVMM runtime files approved by the caller's policy, if any.
    ///
    /// The backend hashes the runtime files once, when it is created, and requires these digests
    /// when they are set.
    pub runtime_sha256: Option<RuntimeDigests>,
    /// Hashes the image and the runtime files again before every start.
    ///
    /// Otherwise a start compares only the files' seals with those taken when they were hashed.
    /// This diagnostic reads every file in full, which adds the hashing time to each start.
    pub content_verification: bool,
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
            image_digest: ImageDigest::Compute,
            library: library.into(),
            library_sha256,
            runtime_sha256: None,
            content_verification: false,
            guest_debug: false,
        }
    }

    /// Sets how the backend establishes the digest of the image that it registers.
    #[must_use]
    pub fn with_image_digest(mut self, digest: ImageDigest) -> Self {
        self.image_digest = digest;
        self
    }

    /// Requires the OpenVMM runtime files to have these approved digests.
    #[must_use]
    pub fn with_runtime_digests(mut self, digests: RuntimeDigests) -> Self {
        self.runtime_sha256 = Some(digests);
        self
    }

    /// Hashes the image and the runtime files again before every start, as a diagnostic.
    #[must_use]
    pub fn with_content_verification(mut self, enabled: bool) -> Self {
        self.content_verification = enabled;
        self
    }

    /// Enables verbose guest boot diagnostics without changing the guest lifecycle.
    #[must_use]
    pub fn with_guest_debug(mut self, enabled: bool) -> Self {
        self.guest_debug = enabled;
        self
    }
}

/// SHA-256 digests of the OpenVMM executable, guest kernel, and guest initramfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeDigests {
    /// Digest of the OpenVMM executable.
    pub openvmm: [u8; 32],
    /// Digest of the guest kernel.
    pub kernel: [u8; 32],
    /// Digest of the guest initramfs.
    pub initrd: [u8; 32],
}

/// Owns sandbox state and OpenVMM processes; the private native library owns only
/// the launch arguments and authenticated guest channel.
#[derive(Debug)]
pub struct NvxHostBackend {
    config: NvxHostConfig,
    base: OpenVmmBackend,
    host: Arc<HostLibrary>,
    images: ImageStore,
    image: ImageId,
    runtime: RuntimeFiles,
    console_pumps: Mutex<HashMap<SandboxId, ConsolePump>>,
}

/// The OpenVMM runtime files, hashed once, when the backend is created.
#[derive(Debug)]
struct RuntimeFiles {
    openvmm: VerifiedFile,
    kernel: VerifiedFile,
    initrd: VerifiedFile,
}

impl RuntimeFiles {
    fn verify(config: &OpenVmmConfig, approved: Option<RuntimeDigests>) -> Result<Self> {
        Ok(Self {
            openvmm: VerifiedFile::verify(
                &config.openvmm,
                "OpenVMM executable",
                approved.map(|digests| digests.openvmm),
            )?,
            kernel: VerifiedFile::verify(
                &config.kernel,
                "guest kernel",
                approved.map(|digests| digests.kernel),
            )?,
            initrd: VerifiedFile::verify(
                &config.initrd,
                "guest initramfs",
                approved.map(|digests| digests.initrd),
            )?,
        })
    }

    fn digests(&self) -> RuntimeDigests {
        RuntimeDigests {
            openvmm: *self.openvmm.sha256(),
            kernel: *self.kernel.sha256(),
            initrd: *self.initrd.sha256(),
        }
    }

    /// Opens every file, keeping writers out on Windows while the handles live, and checks that
    /// none changed since it was hashed. With `rehash`, it also hashes every file again.
    fn open_checked(&self, rehash: bool) -> Result<Vec<File>> {
        [&self.openvmm, &self.kernel, &self.initrd]
            .into_iter()
            .map(|verified| {
                let mut file = verified.open_checked()?;
                if rehash {
                    verified.verify_content(&mut file)?;
                }
                Ok(file)
            })
            .collect()
    }
}

/// Copies the boot console of one OpenVMM launch into the sandbox's console log.
#[derive(Debug)]
struct ConsolePump {
    pid: u32,
    start_time: u64,
    thread: thread::JoinHandle<Result<()>>,
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
    /// Opens the state root, verifies the explicitly supplied native asset, hashes the OpenVMM
    /// runtime files, and registers the configured image.
    ///
    /// Creating a backend reads every runtime file in full. It reads the image only if no
    /// registration that the file still matches exists and `image_digest` is not
    /// [`ImageDigest::Trusted`].
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
        let images_dir = store_root.join(IMAGES_NAME);
        let images = ImageStore::open(&images_dir).map_err(|error| {
            Error::backend_unavailable(format!(
                "cannot open the image registry {}",
                images_dir.display()
            ))
            .with_source(error)
        })?;
        let runtime = RuntimeFiles::verify(&config.openvmm, config.runtime_sha256)?;
        let image = images.register(&config.image, config.image_digest)?.id;
        let base = OpenVmmBackend {
            config: config.openvmm.clone(),
            store,
        };
        Ok(Self {
            config,
            base,
            host,
            images,
            image,
            runtime,
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

    /// Returns the ID of the configured image, which the sandboxes that this backend provisions
    /// boot.
    pub fn image_id(&self) -> ImageId {
        self.image
    }

    /// Returns the digests of the OpenVMM runtime files, computed when the backend was created.
    pub fn runtime_digests(&self) -> RuntimeDigests {
        self.runtime.digests()
    }

    /// Registers the image at `path` in the state root's image registry.
    ///
    /// A registration that the file still matches is reused without reading the file. Otherwise
    /// the image is hashed, or recorded with an [`ImageDigest::Trusted`] digest, and its
    /// registration replaces any earlier one of the same content. Registering a changed image
    /// again lets the sandboxes that boot it start again if its content is unchanged.
    pub fn register_image(
        &self,
        path: impl AsRef<Path>,
        digest: ImageDigest,
    ) -> Result<RegisteredImage> {
        let path = absolute(path.as_ref())?;
        Ok(self.images.register(&path, digest)?.describe(true))
    }

    /// Lists the registered images and whether each file still matches its registration.
    pub fn images(&self) -> Result<Vec<RegisteredImage>> {
        Ok(self
            .images
            .list()?
            .into_iter()
            .map(|record| {
                let intact = self.images.open_checked(&record, false).is_ok();
                record.describe(intact)
            })
            .collect())
    }

    /// Hashes a registered image and checks that it still has the content that its ID names.
    ///
    /// This diagnostic reads the whole image, whereas provisioning and starting compare only the
    /// image's seal with its registration.
    pub fn verify_image(&self, id: &ImageId) -> Result<()> {
        let record = self
            .images
            .get(id)?
            .ok_or_else(|| Error::backend_unavailable(format!("image {id} is not registered")))?;
        self.images.verify(&record)
    }

    /// Removes the registration of an image, returning whether it existed. The file stays.
    ///
    /// Fails with [`ErrorCode::PolicyValidation`] while a provisioned sandbox refers to the image.
    /// Removing the registration of the configured image makes provisioning fail until the image
    /// is registered again.
    pub fn unregister_image(&self, id: &ImageId) -> Result<bool> {
        let _registry = self.images.lock()?;
        let image = id.to_string();
        for sandbox_id in self.base.store.sandbox_ids()? {
            let record = match self.base.store.load(&sandbox_id) {
                Ok(record) => record,
                // The sandbox was deprovisioned after the listing.
                Err(error) if error.code() == ErrorCode::StaleId => continue,
                Err(error) => return Err(error),
            };
            if record.native.is_some_and(|native| native.image == image) {
                return Err(Error::policy_validation(format!(
                    "image {id} is used by sandbox {sandbox_id}; deprovision the sandbox first"
                )));
            }
        }
        self.images.remove(id)
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
        if let Some(pump) = pumps.get(sandbox_id)
            && pump.pid == runtime.pid
            && pump.start_time == runtime.start_time
        {
            return Ok(());
        }
        // A listener of an earlier launch ends promptly because its OpenVMM has exited; its
        // result only describes that launch.
        if let Some(stale) = pumps.remove(sandbox_id) {
            let _ = stale.thread.join();
        }
        let endpoint = self.boot_endpoint(sandbox_id, &runtime.endpoint)?;
        let file = self.base.store.open_console_log(sandbox_id)?;
        let runtime = runtime.clone();
        let (pid, start_time) = (runtime.pid, runtime.start_time);
        let timeout = self.config.openvmm.start_timeout;
        let thread = thread::Builder::new()
            .name("nvxhost-boot-console".to_owned())
            .spawn(move || pump_console(runtime, endpoint, file, timeout))
            .map_err(|error| {
                Error::backend_error("cannot start the guest boot-console listener")
                    .with_source(error)
            })?;
        pumps.insert(
            sandbox_id.clone(),
            ConsolePump {
                pid,
                start_time,
                thread,
            },
        );
        Ok(())
    }

    fn finish_console_pump(&self, sandbox_id: &SandboxId) -> Result<()> {
        let pump = self
            .console_pumps
            .lock()
            .map_err(|_| Error::backend_error("the boot-console pump table is unavailable"))?
            .remove(sandbox_id);
        if let Some(pump) = pump {
            pump.thread
                .join()
                .map_err(|_| Error::backend_error("the boot-console listener panicked"))??;
        }
        Ok(())
    }

    fn artifact_record(&self, devices: Option<serde_json::Value>) -> NativeArtifactRecord {
        let digests = self.runtime.digests();
        NativeArtifactRecord {
            image: self.image.to_string(),
            openvmm_sha256: encode_hex(&digests.openvmm),
            kernel_sha256: encode_hex(&digests.kernel),
            initrd_sha256: encode_hex(&digests.initrd),
            library_sha256: encode_hex(&self.config.library_sha256),
            devices,
        }
    }

    /// Returns a sandbox's artifact identity after checking that it uses this backend's host
    /// library.
    fn native_record<'a>(&self, record: &'a SandboxRecord) -> Result<&'a NativeArtifactRecord> {
        let recorded = record.native.as_ref().ok_or_else(|| {
            Error::backend_error("the native sandbox has no pinned artifact identity")
        })?;
        if recorded.library_sha256 != encode_hex(&self.config.library_sha256) {
            return Err(Error::backend_unavailable(
                "the native sandbox was provisioned with a different host library",
            ));
        }
        Ok(recorded)
    }

    /// Checks the configured image and the runtime files against their seals.
    fn check_configured_artifacts(&self) -> Result<()> {
        let image = self.images.get(&self.image)?.ok_or_else(|| {
            Error::backend_unavailable(format!(
                "image {} is no longer registered; register it again",
                self.image
            ))
        })?;
        self.images.open_checked(&image, false)?;
        self.runtime.open_checked(false)?;
        Ok(())
    }

    /// Checks a sandbox's artifacts before it launches, without hashing them unless content
    /// verification is enabled, and returns its image registration.
    ///
    /// The returned handles keep writers out of the files on Windows until they are dropped, after
    /// OpenVMM has opened the files.
    fn launch_artifacts(&self, record: &SandboxRecord) -> Result<(ImageRecord, Vec<File>)> {
        let recorded = self.native_record(record)?;
        let digests = self.runtime.digests();
        if recorded.openvmm_sha256 != encode_hex(&digests.openvmm)
            || recorded.kernel_sha256 != encode_hex(&digests.kernel)
            || recorded.initrd_sha256 != encode_hex(&digests.initrd)
        {
            return Err(Error::backend_unavailable(
                "the native sandbox was provisioned with different OpenVMM runtime files",
            ));
        }
        let id = ImageId::parse(&recorded.image).map_err(|_| {
            Error::backend_error(format!(
                "the native sandbox records a malformed image ID {:?}",
                recorded.image
            ))
        })?;
        let image = self.images.get(&id)?.ok_or_else(|| {
            Error::backend_unavailable(format!(
                "image {id} of the native sandbox is not registered; register it again"
            ))
        })?;
        let rehash = self.config.content_verification;
        let mut held = self.runtime.open_checked(rehash)?;
        let mut file = self.images.open_checked(&image, true)?;
        if rehash {
            image.verify_content(&mut file)?;
        }
        held.push(file);
        Ok((image, held))
    }

    fn running(&self, sandbox_id: &SandboxId) -> Result<(RuntimeRecord, [u8; 32])> {
        let (_guard, record) = self.base.store.lock_and_load(sandbox_id)?;
        self.native_record(&record)?;
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
        self.check_configured_artifacts()
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
        if let Some(policy) = device_policy(request)? {
            self.host
                .plan_sandbox(&policy, &self.config.openvmm.guest_network, true)?;
        }
        // The state root holds every sandbox's record, including the plan that decides what the
        // next start exports, so no workload may see it.
        let state_root = &self.config.openvmm.state_root;
        if let Some(path) = request
            .filesystem
            .as_ref()
            .and_then(|policy| filesystem::exposes(state_root, policy))
        {
            return Err(Error::policy_validation(format!(
                "the mapped path {} would show workloads the sandbox state in {}; map paths \
                 outside the state root, or deny the state root inside the mapped path",
                path.display(),
                state_root.display()
            )));
        }
        Ok(())
    }

    fn validate_exec(&self, request: &ExecRequest) -> Result<()> {
        prepare_exec(request).map(|_| ())
    }

    fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionResult> {
        self.validate_provision(request)?;
        self.base.probe()?;
        let devices = match device_policy(request)? {
            Some(policy) => {
                let plan = self
                    .host
                    .plan_sandbox(&policy, &self.config.openvmm.guest_network, false)?
                    .ok_or_else(|| Error::backend_error("nvxhost returned no sandbox plan"))?;
                Some(serde_json::from_str(&plan).map_err(|error| {
                    Error::backend_error("nvxhost returned a malformed sandbox plan")
                        .with_source(error)
                })?)
            }
            None => None,
        };
        // Unregistering an image waits for this lock, so the image stays registered until the
        // sandbox that refers to it is recorded.
        let _registry = self.images.lock()?;
        self.check_configured_artifacts()?;
        let record = SandboxRecord {
            format: STATE_FORMAT,
            backend: BACKEND_KEY.to_owned(),
            network: None,
            filesystem: None,
            native: Some(self.artifact_record(devices)),
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
        // A listener of a launch that exited on its own ends promptly. Retire it now so that the
        // new launch's listener connects before the guest writes its first boot output.
        let _ = self.finish_console_pump(sandbox_id);
        // `_held` keeps writers out of the checked files on Windows until OpenVMM has opened them.
        let (image, _held) = self.launch_artifacts(&record)?;
        self.base.probe()?;

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
        let plan = record
            .native
            .as_ref()
            .and_then(|native| native.devices.as_ref())
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                Error::backend_error("cannot encode the sandbox plan").with_source(error)
            })?;
        let mut arguments = self.host.launch_arguments(&LaunchInputs {
            kernel: &self.config.openvmm.kernel,
            initrd: &self.config.openvmm.initrd,
            image: &image.path,
            control: &endpoint,
            boot: &boot,
            hypervisor: self.config.openvmm.hypervisor.as_str(),
            memory_mb: record.memory_mib,
            guest_debug: self.config.guest_debug,
            plan: plan.as_deref(),
        })?;
        let report = self.base.store.outcome_path(sandbox_id);
        remove_if_present(&report)?;
        arguments.push("--microvm-report".into());
        arguments.push(report.into_os_string());

        self.base.store.write_capability(sandbox_id, &capability)?;
        let log = self.base.store.create_log(sandbox_id)?;
        // OpenVMM inherits the log and this claim, so if this process dies before it records
        // OpenVMM's identity, recovery can still tell whether anything of the launch runs.
        platform::claim_launch_log(&log).map_err(|error| {
            Error::backend_error("cannot claim the OpenVMM log").with_source(error)
        })?;
        self.base.store.write_launch(
            sandbox_id,
            &LaunchRecord {
                format: STATE_FORMAT,
                endpoint: endpoint.clone(),
                process: None,
                log_claimed: true,
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
                    log_claimed: true,
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
        self.native_record(&record)?;
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
        // The guest has stopped; a boot-console capture failure is only a diagnostic.
        if let Err(error) = self.finish_console_pump(sandbox_id) {
            metadata.insert("consoleError".to_owned(), error.to_string().into());
        }
        self.base.store.clear_runtime(sandbox_id)?;
        Ok(StopResult {
            metadata: Some(metadata),
        })
    }

    fn deprovision(&self, sandbox_id: &SandboxId) -> Result<DeprovisionResult> {
        let (guard, record) = self.base.store.lock_and_load(sandbox_id)?;
        self.native_record(&record)?;
        if let RunState::Running(_) = self.base.reconcile(sandbox_id)? {
            return Err(Error::already_started(format!(
                "sandbox {sandbox_id} is running; stop it before deprovisioning"
            )));
        }
        // A listener remains only if the guest exited without a stop through this backend.
        let console = self.finish_console_pump(sandbox_id);
        self.base.store.remove(sandbox_id)?;
        drop(guard);
        self.base.store.remove_lock(sandbox_id);
        Ok(DeprovisionResult {
            metadata: console.err().map(|error| {
                Metadata::from_iter([("consoleError".to_owned(), error.to_string().into())])
            }),
        })
    }
}

fn capabilities() -> Capabilities {
    let mut capabilities = Capabilities::new(BACKEND_KEY);
    capabilities.exec.command_line = true;
    capabilities.exec.argv = true;
    capabilities.exec.max_timeout_ms = Some(MAX_EXEC_SECONDS * 1_000);
    capabilities.exec.max_output_bytes = Some(MAX_OUTPUT_BYTES as u64);
    capabilities.network.egress_allow = true;
    capabilities.network.egress_deny = true;
    capabilities.network.ingress_deny = true;
    capabilities.network.host_loopback_deny = true;
    capabilities.network.egress_rules = true;
    capabilities.filesystem.readonly_paths = true;
    capabilities.filesystem.readwrite_paths = true;
    capabilities.filesystem.denied_paths = true;
    capabilities
}

/// The JSON of `request`'s `filesystem` and `network` sections, which nvxhost plans, or `None`
/// when the request has neither.
fn device_policy(request: &ProvisionRequest) -> Result<Option<String>> {
    #[derive(serde::Serialize)]
    struct Policy<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        filesystem: Option<&'a FilesystemPolicy>,
        #[serde(skip_serializing_if = "Option::is_none")]
        network: Option<&'a NetworkPolicy>,
    }
    if request.filesystem.is_none() && request.network.is_none() {
        return Ok(None);
    }
    serde_json::to_string(&Policy {
        filesystem: request.filesystem.as_ref(),
        network: request.network.as_ref(),
    })
    .map(Some)
    .map_err(|error| {
        Error::policy_validation("filesystem paths must be valid UTF-8").with_source(error)
    })
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
    output: File,
    timeout: Duration,
) -> Result<()> {
    copy_console(
        || platform::connect_endpoint(&endpoint, runtime.pid, CONSOLE_POLL),
        |activity| {
            let current = platform::process_start_time(runtime.pid).map_err(|error| {
                Error::backend_error(format!(
                    "cannot verify OpenVMM while {activity} the console"
                ))
                .with_source(error)
            })?;
            Ok(current != Some(runtime.start_time))
        },
        output,
        timeout,
    )
}

/// Copies the guest boot console into `output` until OpenVMM exits.
///
/// OpenVMM serves one console client at a time, so the listener of another backend instance or
/// process may hold the console. This listener then waits, past `timeout`, to take over, and
/// ends without error if OpenVMM exits first.
fn copy_console(
    mut connect: impl FnMut() -> io::Result<Box<dyn Transport>>,
    exited: impl Fn(&str) -> Result<bool>,
    mut output: File,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| Error::backend_error("boot-console timeout is too long"))?;
    let mut held_elsewhere = false;
    let mut console = loop {
        match connect() {
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
                held_elsewhere |= error.kind() == io::ErrorKind::WouldBlock;
                if exited("connecting")? {
                    if held_elsewhere {
                        return Ok(());
                    }
                    return Err(Error::backend_error(
                        "OpenVMM exited before the guest boot console could connect",
                    ));
                }
                if !held_elsewhere && Instant::now() >= deadline {
                    return Err(Error::backend_error(
                        "OpenVMM did not open its guest boot-console endpoint",
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
        match console.read(&mut buffer, Some(CONSOLE_POLL)) {
            Ok(0) => break,
            Ok(size) => output.write_all(&buffer[..size]).map_err(|error| {
                Error::backend_error("cannot write the guest boot-console log").with_source(error)
            })?,
            // A client still queued in a socket's listen backlog is reset, rather than reaching
            // end of stream, when OpenVMM exits.
            Err(error) => {
                if exited("reading")? {
                    break;
                }
                if error.kind() != io::ErrorKind::TimedOut {
                    return Err(Error::backend_error("cannot read the guest boot console")
                        .with_source(error));
                }
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
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::io::{Read, Seek, SeekFrom};

    use super::*;
    use crate::ErrorCode;

    /// Replays scripted boot-console reads.
    struct Replay(VecDeque<io::Result<&'static [u8]>>);

    impl Transport for Replay {
        fn read(&mut self, buffer: &mut [u8], _timeout: Option<Duration>) -> io::Result<usize> {
            let chunk = self
                .0
                .pop_front()
                .expect("the console read past its script")?;
            buffer[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }

        fn write_all(&mut self, _data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
            unreachable!("the console listener never writes")
        }
    }

    fn replay<const N: usize>(
        reads: [io::Result<&'static [u8]>; N],
    ) -> io::Result<Box<dyn Transport>> {
        Ok(Box::new(Replay(reads.into())))
    }

    fn busy() -> io::Result<Box<dyn Transport>> {
        Err(io::ErrorKind::WouldBlock.into())
    }

    fn read_log(mut log: File) -> String {
        let mut text = String::new();
        log.seek(SeekFrom::Start(0)).unwrap();
        log.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn a_console_held_elsewhere_is_awaited_past_the_deadline_until_openvmm_exits() {
        let log = tempfile::tempfile().unwrap();
        let (attempts, checks) = (Cell::new(0), Cell::new(0));
        copy_console(
            || {
                attempts.set(attempts.get() + 1);
                busy()
            },
            |_| {
                checks.set(checks.get() + 1);
                Ok(checks.get() > 4)
            },
            log.try_clone().unwrap(),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(attempts.get(), 5);
        assert_eq!(read_log(log), "");
    }

    #[test]
    fn a_released_console_is_taken_over_and_copied_to_its_end() {
        let log = tempfile::tempfile().unwrap();
        let mut attempts = VecDeque::from([
            busy(),
            replay([
                Ok(b"late ".as_slice()),
                Err(io::ErrorKind::TimedOut.into()),
                Ok(b"output".as_slice()),
                Ok(b"".as_slice()),
            ]),
        ]);
        copy_console(
            || attempts.pop_front().unwrap(),
            |_| Ok(false),
            log.try_clone().unwrap(),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(read_log(log), "late output");
    }

    #[test]
    fn a_console_that_never_opens_fails_at_its_deadline_or_when_openvmm_exits() {
        for (exited, expected) in [
            (false, "did not open its guest boot-console endpoint"),
            (true, "exited before the guest boot console could connect"),
        ] {
            let error = copy_console(
                || Err(io::ErrorKind::NotFound.into()),
                |_| Ok(exited),
                tempfile::tempfile().unwrap(),
                Duration::ZERO,
            )
            .unwrap_err();
            assert!(error.message().contains(expected), "{error}");
        }
    }

    #[test]
    fn a_console_reset_ends_the_log_only_after_openvmm_exits() {
        let log = tempfile::tempfile().unwrap();
        let reset = || {
            replay([
                Ok(b"boot".as_slice()),
                Err(io::ErrorKind::ConnectionReset.into()),
            ])
        };
        copy_console(
            reset,
            |_| Ok(true),
            log.try_clone().unwrap(),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(read_log(log), "boot");

        let error = copy_console(
            reset,
            |_| Ok(false),
            tempfile::tempfile().unwrap(),
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(
            error
                .message()
                .contains("cannot read the guest boot console"),
            "{error}"
        );
    }

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
        assert!(!capabilities().exec.cwd);
    }

    #[test]
    fn host_paths_and_network_follow_the_policy_the_library_plans() {
        let capabilities = capabilities();
        assert!(capabilities.filesystem.readonly_paths);
        assert!(capabilities.filesystem.readwrite_paths);
        assert!(capabilities.filesystem.denied_paths);
        assert!(capabilities.network.egress_allow && capabilities.network.egress_rules);
        assert!(!capabilities.network.ingress_allow && !capabilities.network.host_loopback_allow);

        assert_eq!(device_policy(&ProvisionRequest::default()).unwrap(), None);
        // The JSON is nvxhost's planning input; its own tests parse this exact text.
        let request = ProvisionRequest {
            filesystem: Some(FilesystemPolicy {
                readonly_paths: vec!["/work/src".into()],
                readwrite_paths: vec!["/work/out".into()],
                denied_paths: vec!["/work/src/secret".into()],
            }),
            network: Some(NetworkPolicy {
                egress: crate::model::EgressPolicy::new(crate::model::Access::Deny)
                    .with_allow(crate::model::NetworkRule {
                        to: vec![crate::model::NetworkPeer {
                            cidr: "192.0.2.0/24".into(),
                            except: vec!["192.0.2.128/25".into()],
                        }],
                        ports: vec![crate::model::NetworkPort {
                            protocol: crate::model::Protocol::Tcp,
                            port: Some(443),
                            end_port: Some(444),
                        }],
                    })
                    .with_deny(crate::model::NetworkRule::to("192.0.2.7")),
                ingress: crate::model::IngressPolicy {
                    default: crate::model::Access::Deny,
                    host_loopback: Some(crate::model::Access::Deny),
                },
            }),
            ..ProvisionRequest::default()
        };
        assert_eq!(
            device_policy(&request).unwrap().unwrap(),
            r#"{"filesystem":{"readonlyPaths":["/work/src"],"readwritePaths":["/work/out"],"deniedPaths":["/work/src/secret"]},"network":{"egress":{"default":"deny","allow":[{"to":[{"cidr":"192.0.2.0/24","except":["192.0.2.128/25"]}],"ports":[{"protocol":"tcp","port":443,"endPort":444}]}],"deny":[{"to":[{"cidr":"192.0.2.7"}]}]},"ingress":{"default":"deny","hostLoopback":"deny"}}}"#
        );
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

    fn host_hypervisor() -> super::super::Hypervisor {
        if cfg!(windows) {
            super::super::Hypervisor::Whp
        } else {
            super::super::Hypervisor::Kvm
        }
    }

    #[test]
    fn a_launch_log_claim_lasts_until_the_launcher_and_openvmm_have_exited() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("openvmm.log");
        assert!(
            !platform::launch_log_released(&path).unwrap(),
            "a missing log proves nothing"
        );
        let log = File::create(&path).unwrap();
        platform::claim_launch_log(&log).unwrap();
        assert!(!platform::launch_log_released(&path).unwrap());

        // A long-running child stands in for OpenVMM, which inherits the log as its output.
        #[cfg(windows)]
        let (program, arguments) = (
            PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\PING.EXE"),
            ["-n", "60", "127.0.0.1"].map(std::ffi::OsString::from),
        );
        #[cfg(not(windows))]
        let (program, arguments) = (PathBuf::from("sleep"), [std::ffi::OsString::from("60")]);
        let config = OpenVmmConfig::new(
            program,
            "vmlinux",
            "initramfs.cpio.gz",
            host_hypervisor(),
            directory.path(),
        );
        let launched =
            process::spawn(&config, &arguments, &[1; 32], log, directory.path()).unwrap();
        assert!(
            !platform::launch_log_released(&path).unwrap(),
            "the child's inherited copy no longer holds the claim"
        );
        assert!(process::kill_child(launched));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !platform::launch_log_released(&path).unwrap() {
            assert!(Instant::now() < deadline, "the claim outlived its holders");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn claimed_launch_markers_are_cleared_only_once_nothing_holds_the_log() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = OpenVmmConfig::new(
            "openvmm",
            "vmlinux",
            "initramfs.cpio.gz",
            host_hypervisor(),
            directory.path(),
        );
        config.start_timeout = Duration::from_millis(200);
        let store = StateStore::open_for(&directory.path().join(BACKEND_KEY), BACKEND_KEY).unwrap();
        let base = OpenVmmBackend { config, store };
        let sandbox_id = SandboxId::generate().unwrap();
        let record = SandboxRecord {
            format: STATE_FORMAT,
            backend: BACKEND_KEY.to_owned(),
            network: None,
            filesystem: None,
            native: None,
            memory_mib: 256,
            workload_uid: 65534,
            workload_gid: 65534,
            create_workload_account: false,
            hostname: "nvx-sandbox".to_owned(),
        };
        base.store.create(&sandbox_id, &record).unwrap();
        let endpoint = platform::control_endpoint(&base.store.socket_path(&sandbox_id)).unwrap();
        let marker = |log_claimed| LaunchRecord {
            format: STATE_FORMAT,
            endpoint: endpoint.clone(),
            process: None,
            log_claimed,
        };
        let retained = |expected: &str| {
            let Err(error) = base.reconcile(&sandbox_id) else {
                panic!("an unproven launch marker was cleared");
            };
            assert_eq!(error.code(), ErrorCode::BackendError, "{error}");
            assert!(error.message().contains(expected), "{error}");
            assert!(base.store.launch(&sandbox_id).unwrap().is_some());
        };

        // A claim whose log is missing proves nothing.
        base.store.write_launch(&sandbox_id, &marker(true)).unwrap();
        retained("is missing or still held");

        // Without a claim, an absent endpoint never proves that the launch exited.
        drop(base.store.create_log(&sandbox_id).unwrap());
        base.store
            .write_launch(&sandbox_id, &marker(false))
            .unwrap();
        retained("cannot verify that OpenVMM exited");

        // A held claim is awaited for the start timeout, and the marker then retained.
        let log = base.store.create_log(&sandbox_id).unwrap();
        platform::claim_launch_log(&log).unwrap();
        base.store.write_launch(&sandbox_id, &marker(true)).unwrap();
        let waited = Instant::now();
        retained("is missing or still held");
        assert!(waited.elapsed() >= Duration::from_millis(200));

        // A released claim proves that nothing of the launch runs.
        drop(log);
        assert!(matches!(
            base.reconcile(&sandbox_id).unwrap(),
            RunState::Provisioned
        ));
        assert!(base.store.launch(&sandbox_id).unwrap().is_none());
    }
}
