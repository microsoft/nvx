//! A caller-owned VM directory, private to the current user, with fixed names for its files and
//! endpoints. The caller may keep files of its own under other names.
//!
//! [`create_private_dir`] creates a directory private to the current user, or checks an existing
//! one, and [`VmDir::open`] opens only one that is private: mode 0700 on Linux, or an owner-only
//! protected access control list on Windows. Neither changes an existing directory, since
//! restricting its permissions would not revoke access through files that others already opened.
//!
//! The library reserves the names documented below, and every name that starts with `.openvmm`,
//! which OpenVMM gives its temporary files. [`Capability`] holds the credential of a VM's control
//! endpoint, which the library never writes to a file. A caller keeps its own [`SavedVm`] values
//! for [`verify`] and [`reattach`] after a restart, without a library-owned store.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(feature = "cli")]
use std::time::Duration;
use std::time::Instant;

use crate::activation::{
    ActivationRecord, Configuration, ExpectedResources, Export, ExportKind, Image, RestoreSection,
    TransactionId, Upper,
};
use crate::host::config::{
    BlockGeometry, CaptureConfig, ConsoleAttachment, DiskCapture, NetworkConfig, NicConfig,
    RootConfig, UpperConfig, VmConfig, check_argument_nuls, check_endpoint, console_arguments,
    fit_process_command_line, reject_disk_path, runtime_arguments,
};
use crate::host::filesystem;
use crate::host::network;
use crate::host::paths::absolute;
use crate::host::platform;
use crate::host::report::{self, Report};
use crate::host::restore::ReadyListener;
use crate::host::runtime::{Runtime, RuntimeIdentity};
use crate::host::snapshot::{CaptureContext, HostTime, Snapshot};
use crate::host::spawn;
use crate::host::transport::{self, HostConnection};
use crate::host::vm_directory::{CONSOLE, CONTROL, RESTORE_READY, io_error};
pub use crate::host::vm_directory::{VmDir, VmOutputs, create_private_dir};
use crate::host::{
    platform::inherit::Inherited,
    process::{Opened, Process, ProcessRef},
};
use crate::protocol::CAPABILITY_LEN;
use crate::{Error, Result};

/// OpenVMM's snapshot files: a capture's destination and a restore's snapshot view.
const OPENVMM: &str = "openvmm";
/// The sparse placeholder image that a capture VM attaches.
const CAPTURE_IMAGE: &str = "capture-image.raw";
/// The dormant scratch that a capture VM attaches.
const CAPTURE_SCRATCH: &str = "capture-scratch.raw";

/// The credential that the host presents at a VM's control endpoint.
///
/// The library never writes it to a file, and its `Debug` form hides it. A caller that must
/// reconnect after a restart keeps the bytes that [`as_bytes`](Self::as_bytes) returns.
pub struct Capability([u8; CAPABILITY_LEN]);

impl Capability {
    /// Returns the credential's bytes.
    pub fn as_bytes(&self) -> &[u8; CAPABILITY_LEN] {
        &self.0
    }
}

impl fmt::Debug for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Capability").finish_non_exhaustive()
    }
}

/// Returns a fresh launch capability, which is never all zero.
pub(crate) fn generate_capability() -> Result<[u8; CAPABILITY_LEN]> {
    let mut capability = [0u8; CAPABILITY_LEN];
    while capability == [0; CAPABILITY_LEN] {
        getrandom::fill(&mut capability).map_err(|error| {
            Error::other("cannot generate a control capability").with_source(error)
        })?;
    }
    Ok(capability)
}

/// Launch values that the caller saves and reloads to verify or reattach its VM.
///
/// The library never serializes, stores or locates them. The endpoint is the name chosen at
/// launch, not one newly generated from the directory. The `Debug` form hides the capability.
#[derive(Clone)]
pub struct SavedVm {
    /// The caller-owned private VM directory.
    pub vm_dir: VmDir,
    /// The control endpoint chosen at launch.
    pub endpoint: String,
    /// The launch credential's bytes, from [`Capability::as_bytes`].
    pub capability: [u8; CAPABILITY_LEN],
    /// The process's recorded ID, start time and host boot.
    pub process: ProcessRef,
    /// The runtime identity that the VM was launched with.
    pub runtime: RuntimeIdentity,
}

impl fmt::Debug for SavedVm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SavedVm")
            .field("vm_dir", &self.vm_dir)
            .field("endpoint", &self.endpoint)
            .field("capability", &Capability(self.capability))
            .field("process", &self.process)
            .field("runtime", &self.runtime)
            .finish()
    }
}

/// Checks the saved runtime and process, returning the verified held process.
///
/// Opens no endpoint and reads no runtime artifact. A different runtime is
/// [`ErrorKind::RuntimeMismatch`](crate::ErrorKind::RuntimeMismatch), whose message names the
/// differing identity parts. An exited recorded process is [`ErrorKind::Exited`](crate::ErrorKind::Exited),
/// and an indeterminate process check is
/// [`ErrorKind::Io`](crate::ErrorKind::Io), retaining its original I/O source.
pub fn verify(saved: &SavedVm, runtime: &RuntimeIdentity) -> Result<Process> {
    if saved.runtime != *runtime {
        let parts: Vec<&str> = saved
            .runtime
            .differences(runtime)
            .iter()
            .map(|part| part.name())
            .collect();
        return Err(Error::new(
            crate::ErrorKind::RuntimeMismatch,
            format!(
                "the saved VM runtime does not match the current runtime, which differs in {}",
                parts.join(", ")
            ),
        ));
    }
    match Process::open(&saved.process)
        .map_err(|error| Error::io("cannot verify the saved VM process").with_source(error))?
    {
        Opened::Running(process) => Ok(process),
        Opened::Exited(_) => {
            let message = "the saved VM process exited";
            Err(Error::exited(message)
                .with_source(io::Error::new(io::ErrorKind::NotFound, message)))
        }
    }
}

/// A verified held VM process and its authenticated control connection.
pub struct Reattached {
    /// The process that matched the saved identity.
    pub process: Process,
    /// The connection attached with the saved launch capability.
    pub connection: HostConnection,
}

/// Verifies the saved VM, peer-verifies its endpoint and authenticates with its saved capability.
///
/// Uses `runtime`'s identity without reading its artifacts. Failures retain the kinds and sources
/// of [`verify`], [`transport::connect_verified`] and [`HostConnection::attach`], and a failure
/// neither signals the VM nor changes the caller's saved values.
pub fn reattach(
    saved: &SavedVm,
    runtime: &Runtime,
    deadline: std::time::Instant,
) -> Result<Reattached> {
    reattach_with_transport(saved, runtime, deadline, transport::connect_verified)
}

/// Lets the CLI observe its verified transport before the single authenticated attachment.
pub(crate) fn reattach_with_transport(
    saved: &SavedVm,
    runtime: &Runtime,
    deadline: std::time::Instant,
    connect: impl FnOnce(&str, &ProcessRef, std::time::Instant) -> Result<Box<dyn transport::Transport>>,
) -> Result<Reattached> {
    let process = verify(saved, &runtime.identity)?;
    let transport = connect(&saved.endpoint, &saved.process, deadline)?;
    let connection = HostConnection::attach(transport, &saved.capability, deadline)?;
    Ok(Reattached {
        process,
        connection,
    })
}

/// An owned descriptor on Unix.
#[cfg(unix)]
type Owned = std::os::fd::OwnedFd;
/// An owned handle on Windows.
#[cfg(windows)]
type Owned = std::os::windows::io::OwnedHandle;
/// An owned file on hosts without descriptors or handles.
#[cfg(not(any(unix, windows)))]
type Owned = File;

/// An already opened resource that a VMM inherits, owned until dropped or transferred.
///
/// Conversions move the same descriptor or handle; they neither duplicate nor reopen it.
/// Borrowed raw descriptors and handles cannot transfer ownership into this type.
#[derive(Debug)]
pub struct InheritedHandle {
    /// The owned platform resource.
    handle: Owned,
}

impl From<File> for InheritedHandle {
    fn from(file: File) -> Self {
        Self {
            handle: file.into(),
        }
    }
}

#[cfg(unix)]
impl From<std::os::fd::OwnedFd> for InheritedHandle {
    fn from(handle: std::os::fd::OwnedFd) -> Self {
        Self { handle }
    }
}

#[cfg(unix)]
impl From<InheritedHandle> for std::os::fd::OwnedFd {
    fn from(resource: InheritedHandle) -> Self {
        resource.handle
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for InheritedHandle {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.handle)
    }
}

#[cfg(windows)]
impl From<std::os::windows::io::OwnedHandle> for InheritedHandle {
    fn from(handle: std::os::windows::io::OwnedHandle) -> Self {
        Self { handle }
    }
}

#[cfg(windows)]
impl From<InheritedHandle> for std::os::windows::io::OwnedHandle {
    fn from(resource: InheritedHandle) -> Self {
        resource.handle
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsHandle for InheritedHandle {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        std::os::windows::io::AsHandle::as_handle(&self.handle)
    }
}

/// A complete launch handed to the caller's launcher, with all its resources owned.
///
/// The launcher supplies the capability on stdin through a pipe closed to writers before
/// spawning, and uses the opened log for stdout and stderr. It allows only the standard streams
/// and `inherit`, under their original descriptor or handle numbers, into the child.
///
/// Follow [`spawn`](fn@spawn)'s launch contract: detach on Linux, clear only the child's owner
/// umask bits, and keep allowed descriptors close-on-exec in the caller; on Windows, use an
/// explicit handle list, restore temporary inheritability, start without a console in the
/// log-named job, refuse a taken or untrusted job name, and honor `breakaway_from_job`. Windows
/// callers coordinate their other spawns with the inheritance window. If setup fails after
/// spawning, terminate and reap only that child before returning an error. The caller journals
/// recovery data and keeps the VM directory and runtime inputs stable; this value starts no
/// process. A restore retains its already-bound ready listener in `ready`, outside the child
/// inheritance allowlist.
#[derive(Debug)]
pub struct VmmCommand {
    /// The checked OpenVMM executable.
    pub program: PathBuf,
    /// The complete, checked arguments, without the executable.
    pub arguments: Vec<OsString>,
    /// The VM directory in which OpenVMM runs.
    pub working_dir: PathBuf,
    /// The control credential, never written to disk by the library.
    pub capability: Capability,
    /// The already chosen output files and endpoints.
    pub outputs: VmOutputs,
    /// Owned resources allowed into the child besides its standard streams.
    pub inherit: Vec<InheritedHandle>,
    /// The same private log that preparation opened.
    pub log: File,
    /// Whether Windows leaves the caller's job, which must permit breakaway.
    pub breakaway_from_job: bool,
    /// The same restore-ready listener bound during creation, absent for a cold launch.
    pub ready: Option<ReadyListener>,
}

/// A checked launch, with its capability, chosen outputs, and already opened log.
///
/// Creation starts no process; a restore binds its ready listener. The caller keeps the VM
/// directory and runtime inputs stable, and may journal the capability and outputs before a
/// later spawn.
/// Dropping a successful preparation closes its files but leaves its output paths to the caller.
#[derive(Debug)]
pub struct Prepared {
    /// The executable checked during creation.
    program: PathBuf,
    /// The private VM directory in which the child starts.
    working_dir: PathBuf,
    /// Whether the child leaves the caller's Windows job.
    windows_breakaway: bool,
    /// Whether this CLI-owned child keeps the caller's session, group, and console.
    foreground: bool,
    /// The control credential, never written to disk.
    capability: Capability,
    /// The launch's fixed file paths and chosen endpoints.
    outputs: VmOutputs,
    /// The newly created log, kept open rather than reopened by name.
    log: File,
    /// The complete arguments, checked against the actual runtime and output paths.
    arguments: Vec<OsString>,
    /// The checked launch configuration, including this launch's network policy.
    configuration: VmConfig,
    /// The effective NIC, whose presence and address a snapshot fixes for a restore.
    nic: Option<NicConfig>,
    /// Already opened resources added by the caller, retained until the preparation is dropped.
    inherit: Vec<InheritedHandle>,
    /// The checked context of a capture, absent for an ordinary cold launch.
    capture_context: Option<CaptureContext>,
    /// The restore's listener, retained outside the child's inheritance allowlist.
    ready: Option<ReadyListener>,
}

impl Prepared {
    /// Returns the effective NIC, or none when the launch attaches no NIC.
    ///
    /// A cold launch resolves an omitted address to
    /// [`DEFAULT_GUEST_NETWORK`](crate::host::config::DEFAULT_GUEST_NETWORK). A restore's
    /// resolved NIC retains its captured presence and address, independently of its policy.
    pub fn nic(&self) -> Option<NicConfig> {
        self.nic
    }

    /// Builds a layered root's activation record from this launch's checked, owned settings,
    /// preserving `tx` as its transaction. Reads no host path and opens no snapshot.
    ///
    /// The effective NIC supplies the network's presence and IPv4 and derived IPv6 gateways;
    /// this launch's policy supplies its DNS, publication and proxy, rather than a capture's.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a blockless
    /// root, which activation records do not represent, memory that cannot fit in the record's
    /// byte count, or settings that [`Configuration::check`] refuses.
    pub fn activation_record(&self, tx: TransactionId) -> Result<ActivationRecord> {
        let RootConfig::Layered(root) = &self.configuration.root else {
            return Err(Error::unsupported(
                "an activation record needs a layered root",
            ));
        };
        let image = match root.format.format {
            crate::protocol::root::ImageFormat::Erofs => Image::Erofs {
                uuid: root.format.identity,
            },
            crate::protocol::root::ImageFormat::Gpt => Image::Gpt {
                disk_guid: root.format.identity,
                first_partition_bound: root.gpt_first_partition_bound.unwrap_or(u64::MAX),
            },
        };
        let upper = match root.upper {
            UpperConfig::Ram { max_memory_percent } => Upper::Tmpfs { max_memory_percent },
            UpperConfig::Scratch { journal, .. } => Upper::Scratch { journal },
        };
        let network = match self.nic {
            None => None,
            Some(nic) => {
                let policy =
                    self.configuration.network.as_ref().ok_or_else(|| {
                        Error::unsupported("a prepared NIC needs a network policy")
                    })?;
                let gateway = crate::net::guest_gateway(&nic.guest_network.to_string())
                    .ok_or_else(|| Error::unsupported("the prepared NIC has no gateway"))?;
                // OpenVMM embeds every microVM NIC's IPv4 identity in fd00::/96.
                let ipv6_gateway =
                    std::net::Ipv6Addr::from((0xfd00_u128 << 112) | u128::from(u32::from(gateway)));
                let dns = network::dns_server(&policy.policy, gateway, Some(ipv6_gateway))?;
                Some(policy.policy.activation_network(gateway, dns))
            }
        };
        let (export, mappings) = self
            .configuration
            .filesystem
            .as_ref()
            .map_or((None, 0), |plan| {
                (
                    Some(Export {
                        kind: ExportKind::Aggregate,
                    }),
                    plan.binds.len(),
                )
            });
        let memory = self
            .configuration
            .memory_mib
            .checked_mul(1 << 20)
            .ok_or_else(|| Error::unsupported("VM memory does not fit the record's byte count"))?;
        ActivationRecord::new(
            Some(RestoreSection { transaction: tx }),
            Configuration {
                image,
                upper,
                overlay: root.overlay,
                export,
                mappings,
                network,
                view: self.configuration.view.clone(),
                resources: ExpectedResources {
                    cpus: self.configuration.processors,
                    memory,
                },
            },
        )
        .map_err(|error| {
            Error::unsupported(format!(
                "no activation record holds the prepared settings: {error}"
            ))
            .with_source(error)
        })
    }

    /// Keeps this CLI launch in its caller's session, process group, and console.
    #[cfg(feature = "cli")]
    pub(crate) fn for_foreground(mut self) -> Self {
        self.foreground = true;
        self
    }

    /// Returns the credential to journal before a spawn or use when attaching a session.
    pub fn capability(&self) -> &Capability {
        &self.capability
    }

    /// Returns the launch's log, outcome, control, and optional boot-console names.
    pub fn outputs(&self) -> &VmOutputs {
        &self.outputs
    }

    /// Adds an already opened resource to the inheritance allowlist, transferring ownership.
    ///
    /// The resource is not reopened, and remains open for the lifetime of this preparation.
    pub fn inherit(&mut self, resource: impl Into<InheritedHandle>) {
        self.inherit.push(resource.into());
    }

    /// Returns the same opened log that creation made, for example to take a liveness claim.
    pub fn log(&self) -> &File {
        &self.log
    }

    /// Returns the complete, checked OpenVMM arguments without starting a process.
    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    /// Returns the checked capture context, if this launch prepares a blockless capture.
    ///
    /// The caller chooses whether and how to persist it; creation writes no context file.
    pub fn capture_context(&self) -> Option<&CaptureContext> {
        self.capture_context.as_ref()
    }

    /// Transfers the complete checked launch to the caller's launcher.
    ///
    /// Moves the same opened log and inherited resources, without cloning, reopening, rendering
    /// arguments again or rediscovering paths. Moves a restore's same bound listener, without
    /// putting it in the child's inheritance allowlist. Starts no process or new listener. The
    /// caller follows [`VmmCommand`]'s launch contract and may retain its capability and outputs
    /// for recovery. Read [`nic`](Self::nic) or [`capture_context`](Self::capture_context), or
    /// build an [`activation_record`](Self::activation_record), before consuming the preparation.
    pub fn into_command(self) -> VmmCommand {
        VmmCommand {
            program: self.program,
            arguments: self.arguments,
            working_dir: self.working_dir,
            capability: self.capability,
            outputs: self.outputs,
            inherit: self.inherit,
            log: self.log,
            breakaway_from_job: self.windows_breakaway,
            ready: self.ready,
        }
    }
}

/// Creates an inspectable cold launch or snapshot restore in `dir`, without starting a process.
///
/// A cold launch needs [`VmConfig::boot`]; a restore refuses it and calls [`Snapshot::accepts`]
/// with this host's actual time. A restore hard-links only the snapshot's OpenVMM files into the
/// new, fixed `openvmm/` view, requiring the same filesystem, and binds its ready listener before
/// any spawn. Both launch paths retain that same listener. Nothing is copied or restored from a
/// snapshot-parent fallback. The successful view and all output paths remain caller-owned.
/// The caller keeps the directory and runtime inputs stable during creation and a later spawn.
/// Caller-relative runtime and block-image paths become absolute during creation, without
/// resolving links or `..` components; output paths come from `dir`.
///
/// The log is created anew, privately, and kept open in [`Prepared`]. Other outputs are reserved
/// names that OpenVMM creates when it runs: existing entries at those names are refused, not
/// replaced. A failure closes and removes only files that this invocation created, preserving
/// every pre-existing entry and the caller's inputs. The capability is never written to disk.
/// With `boot.capture`, the root must be blockless and no export is accepted. Creation also
/// prepares sparse image and scratch files, verifies their actual geometry, and retains the
/// checked capture context. OpenVMM captures to `openvmm/` under this VM directory, with a
/// configured NIC under deny-all. No key, description or context file is written.
///
/// On Linux the caller holds its per-snapshot lock from this call, including adding the hard
/// links, through a peer-verified restore-ready event or the VM's verified exit, and around
/// removing `openvmm/`. NVX acquires no lock; Windows needs none. A restore whose caller dies
/// before OpenVMM connects the restore-ready endpoint cannot complete: OpenVMM checks the
/// snapshot's files before that one connection, which it does not retry, and the dead caller's
/// endpoint no longer listens. After the connection OpenVMM no longer checks those files, so
/// the caller needs no durable record of its in-flight restores; it keeps the VM's process as it
/// does for any launch.
///
/// Fails as [`Snapshot::accepts`] does, or with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for Boot on a restore, a missing
/// boot, a refused configuration, unrepresentable Linux socket names, or a complete Windows
/// command line beyond the portable limit.
/// The final check includes the actual runtime and output paths, with no reserve, on every host.
/// Filesystem failures have kind [`ErrorKind::Io`](crate::ErrorKind::Io), retaining their source;
/// a rollback failure is reported along with the original error.
pub fn create(
    runtime: &Runtime,
    config: &VmConfig,
    from: Option<&Snapshot>,
    dir: &VmDir,
) -> Result<Prepared> {
    if let Some(snapshot) = from {
        if config.boot.is_some() {
            return Err(Error::unsupported(
                "restoring a Snapshot does not take Boot",
            ));
        }
        snapshot.accepts(config, runtime, &HostTime::now()?)?;
    } else if config.boot.is_none() {
        return Err(Error::unsupported("creating a cold VM needs Boot"));
    }
    let capture = config.boot.as_ref().and_then(|boot| boot.capture.as_ref());
    if capture.is_some() {
        config.kernel_command_line()?;
        runtime.identity.check()?;
    }
    VmDir::open(dir.path())?;
    platform::check_working_dir(dir.path())?;
    let outputs = dir.outputs(config.console == ConsoleAttachment::Listener)?;
    let ready_endpoint = if from.is_some() && !cfg!(windows) {
        Some(dir.restore_ready_endpoint()?)
    } else {
        None
    };
    if !cfg!(windows) {
        for endpoint in std::iter::once(&outputs.endpoint)
            .chain(outputs.console.as_ref())
            .chain(ready_endpoint.as_ref())
        {
            if endpoint.len() >= 108 || endpoint.contains([',', '=']) {
                return Err(Error::unsupported(format!(
                    "the VM socket {endpoint} must be shorter than 108 bytes and hold no ',' or '='",
                )));
            }
        }
        let canonical = filesystem::canonicalize(dir.path())
            .map_err(|error| io_error("resolve", dir.path(), error))?;
        for name in std::iter::once(CONTROL)
            .chain(outputs.console.as_ref().map(|_| CONSOLE))
            .chain(ready_endpoint.as_ref().map(|_| RESTORE_READY))
        {
            let endpoint = canonical.join(name);
            if endpoint.to_str().is_none() || endpoint.as_os_str().len() >= 108 {
                return Err(Error::unsupported(format!(
                    "the canonical VM socket {} must be valid Unicode and shorter than 108 bytes",
                    endpoint.display(),
                )));
            }
        }
    }
    let resolve = |path: &Path| -> Result<PathBuf> {
        // Keep invalid spellings for the renderer's existing required-path checks.
        if path.as_os_str().is_empty() || path.as_os_str().to_string_lossy().contains('\0') {
            return Ok(path.to_owned());
        }
        absolute(path).map_err(|error| {
            Error::io(format!("cannot resolve VM input {}", path.display())).with_source(error)
        })
    };
    let runtime = Runtime {
        openvmm: resolve(&runtime.openvmm)?,
        kernel: resolve(&runtime.kernel)?,
        initramfs: resolve(&runtime.initramfs)?,
        ..runtime.clone()
    };
    let mut config = config.clone();
    if let RootConfig::Layered(root) = &mut config.root {
        root.image = resolve(&root.image)?;
        if let UpperConfig::Scratch { path, .. } = &mut root.upper {
            *path = resolve(path)?;
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let log = options.open(&outputs.log).map_err(|error| {
        Error::io(format!(
            "cannot create the VM log {}",
            outputs.log.display()
        ))
        .with_source(error)
    })?;
    let mut created = Vec::new();
    let view = dir.path().join(OPENVMM);
    let mut created_view = false;
    let mut ready = None;
    let checked = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            log.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| {
                    Error::io("cannot make the new VM log private").with_source(error)
                })?;
        }
        if let Some(problem) = platform::private_file_problem(&log)
            .map_err(|error| Error::io("cannot check the new VM log").with_source(error))?
        {
            return Err(Error::io(format!("the new VM log {problem}")));
        }
        unused_output(&outputs.outcome)?;
        if !cfg!(windows) {
            unused_output(Path::new(&outputs.endpoint))?;
            if let Some(console) = &outputs.console {
                unused_output(Path::new(console))?;
            }
        }
        let capability = Capability(generate_capability()?);
        if let Some(snapshot) = from {
            unused_output(&view)?;
            if let Some(endpoint) = &ready_endpoint {
                unused_output(Path::new(endpoint))?;
            }
            // The private directory stays caller-stable; setup can fail after creating it.
            created_view = true;
            create_private_dir(&view)?;
            snapshot.link_restore_view(&view, &mut created)?;
            // Binding can leave its new socket if later listener setup fails.
            if let Some(endpoint) = &ready_endpoint {
                created.push(PathBuf::from(endpoint));
            }
            let listener = ReadyListener::bind(dir)?;
            let arguments = restore_arguments(
                &runtime,
                &config,
                snapshot,
                dir,
                &outputs,
                listener.endpoint(),
            )?;
            ready = Some(listener);
            return Ok((capability, arguments, snapshot.nic(), None));
        }
        let mut arguments = config.openvmm_arguments(
            &runtime,
            &outputs.endpoint,
            &outputs.outcome,
            outputs.console.as_deref(),
        )?;
        let nic = config
            .network
            .as_ref()
            .map(NetworkConfig::cold_nic)
            .transpose()?;
        let capture_context = if let Some(capture) = capture {
            let image_path = dir.path().join(CAPTURE_IMAGE);
            let scratch_path = dir.path().join(CAPTURE_SCRATCH);
            let destination = dir.path().join(OPENVMM);
            for path in [&image_path, &scratch_path, &destination] {
                unused_output(path)?;
            }
            for path in [&image_path, &scratch_path] {
                if path.to_str().is_none() {
                    return Err(Error::unsupported(
                        "capture block paths must be Unicode for OpenVMM",
                    ));
                }
                reject_disk_path(path, &path.display().to_string())?;
            }
            arguments.extend([
                "--microvm-sandbox-block".into(),
                format!("distro:file:{},ro", image_path.display()).into(),
                "--microvm-sandbox-block".into(),
                format!("scratch:file:{}", scratch_path.display()).into(),
                "--snapshot-destination".into(),
                destination.into_os_string(),
            ]);
            fit_process_command_line(runtime.openvmm.as_os_str(), &arguments, 0)?;
            let DiskCapture::Fresh { image, scratch } = capture.disks;
            let image = capture_disk(&image_path, image, &mut created)?;
            let scratch = capture_disk(&scratch_path, scratch, &mut created)?;
            Some(CaptureContext {
                capture: CaptureConfig {
                    disks: DiskCapture::Fresh { image, scratch },
                    ..capture.clone()
                },
                nic,
                console: config.console,
                runtime: runtime.identity.clone(),
                hypervisor: runtime.hypervisor,
                cpu_profile: runtime.cpu_profile.clone(),
            })
        } else {
            None
        };
        Ok((capability, arguments, nic, capture_context))
    })();
    match checked {
        Ok((capability, arguments, nic, capture_context)) => Ok(Prepared {
            program: runtime.openvmm.clone(),
            working_dir: dir.path().to_owned(),
            windows_breakaway: runtime.windows_breakaway,
            foreground: false,
            capability,
            outputs,
            log,
            arguments,
            configuration: config,
            nic,
            inherit: Vec::new(),
            capture_context,
            ready,
        }),
        Err(mut error) => {
            drop(log);
            drop(ready);
            for path in created.iter().rev() {
                if let Err(cleanup) = fs::remove_file(path)
                    && cleanup.kind() != io::ErrorKind::NotFound
                {
                    error = Error::new(
                        error.kind(),
                        format!(
                            "{error}; cannot remove created VM entry {}: {cleanup}",
                            path.display()
                        ),
                    )
                    .with_source(error);
                }
            }
            if created_view
                && let Err(cleanup) = fs::remove_dir(&view)
                && cleanup.kind() != io::ErrorKind::NotFound
            {
                error = Error::new(
                    error.kind(),
                    format!(
                        "{error}; cannot remove created restore view {}: {cleanup}",
                        view.display()
                    ),
                )
                .with_source(error);
            }
            match fs::remove_file(&outputs.log) {
                Ok(()) => Err(error),
                Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(error),
                Err(cleanup) => Err(Error::new(
                    error.kind(),
                    format!(
                        "{error}; cannot remove the created VM log {}: {cleanup}",
                        outputs.log.display()
                    ),
                )
                .with_source(error)),
            }
        }
    }
}

/// Renders a restore without cold-boot inputs or a store/lease dependency.
fn restore_arguments(
    runtime: &Runtime,
    config: &VmConfig,
    snapshot: &Snapshot,
    dir: &VmDir,
    outputs: &VmOutputs,
    ready: &str,
) -> Result<Vec<OsString>> {
    check_endpoint(&outputs.endpoint, "endpoint")?;
    check_endpoint(ready, "restore-ready endpoint")?;
    for (name, path) in [
        ("Runtime.openvmm", runtime.openvmm.as_path()),
        ("report", outputs.outcome.as_path()),
    ] {
        if path.as_os_str().is_empty() || path.as_os_str().to_string_lossy().contains('\0') {
            return Err(Error::unsupported(format!(
                "{name} must name a path without a NUL"
            )));
        }
    }
    for endpoint in std::iter::once(&outputs.endpoint).chain(outputs.console.as_ref()) {
        if endpoint == ready
            || (runtime.hypervisor == crate::Hypervisor::Whp
                && endpoint.eq_ignore_ascii_case(ready))
        {
            return Err(Error::unsupported(
                "restore-ready and console endpoints must be distinct",
            ));
        }
    }
    let console = config.console_backend(runtime, &outputs.endpoint, outputs.console.as_deref())?;
    let key = snapshot.key();
    let mut arguments = config.restore_device_arguments(key.processors(), snapshot.nic())?;
    arguments.extend([
        "--single-process".into(),
        "--hypervisor".into(),
        key.hypervisor().as_str().into(),
        "--restore-snapshot".into(),
        dir.path().join(OPENVMM).into_os_string(),
    ]);
    if key.boot_processors() != 0 {
        arguments.extend([
            "--restore-processors".into(),
            config.processors.to_string().into(),
        ]);
    }
    arguments.extend([
        "--restore-memory".into(),
        format!("{}M", config.memory_mib).into(),
        "--restore-ready-path".into(),
        ready.into(),
    ]);
    arguments.extend(console_arguments(
        &console,
        &outputs.endpoint,
        &outputs.outcome,
    ));
    arguments.extend(runtime_arguments(&Runtime {
        cpu_profile: Some(key.cpu_profile()),
        ..runtime.clone()
    })?);
    check_argument_nuls(&arguments)?;
    fit_process_command_line(runtime.openvmm.as_os_str(), &arguments, 0)?;
    Ok(arguments)
}

/// A detached VM owned by its caller.
///
/// Dropping this value closes the process handle, not the VM. Its outputs remain caller-owned,
/// and its capability is the same credential returned by the preparation.
#[derive(Debug)]
pub struct Launched {
    /// The verified process, held independently of the child handle used during spawn.
    pub process: Process,
    /// The preparation's output files and endpoints.
    pub outputs: VmOutputs,
    /// The control credential, never written to disk.
    pub capability: Capability,
    /// The preparation's same restore-ready listener, absent for a cold launch.
    pub ready: Option<ReadyListener>,
    /// Retains only a foreground caller's child, for its status.
    #[cfg(feature = "cli")]
    child: ForegroundChild,
}

/// Reaps a foreground child on release without adding `Drop` to the public launch value.
#[cfg(feature = "cli")]
#[derive(Debug)]
struct ForegroundChild(Option<spawn::Launched>);

#[cfg(feature = "cli")]
impl Drop for ForegroundChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.take() {
            child.detach();
        }
    }
}

#[cfg(feature = "cli")]
impl Launched {
    /// Polls the owned child independently of its optional outcome report.
    pub(crate) fn wait_status(&mut self, timeout: Duration) -> io::Result<Option<i32>> {
        Ok(self
            .child_mut()?
            .wait_timeout(timeout)?
            .map(|status| status as i32))
    }

    /// Returns the owned child, whose status [`Self::wait_status`] polls.
    fn child_mut(&mut self) -> io::Result<&mut spawn::Launched> {
        self.child
            .0
            .as_mut()
            .ok_or_else(|| io::Error::other("the VM has no child owned by this caller"))
    }
}

/// Starts a prepared launch and returns its verified, detached process and any ready listener.
///
/// The capability reaches OpenVMM only on standard input, followed by end-of-file. The opened
/// log becomes standard output and error without reopening its path. Only the preparation's
/// owned inheritance allowlist and standard streams reach the child.
///
/// On Linux the child starts in a new session, with the caller's umask without its owner's bits;
/// the caller's umask is unchanged. On Windows it starts without a console, in its log-named
/// job, outside the caller's job when the runtime requests breakaway. A taken or untrusted log
/// job name is refused. Other processes started during its inheritance window never join that
/// job, but may inherit its handles: Windows callers coordinate their own other spawns, for
/// example with their own lock around this function and their other process starts. The
/// library's inheritance lock remains internal to this spawn.
///
/// The caller keeps the directory and runtime inputs stable through creation and spawn.
/// A failure after child creation terminates only that child and waits for its confirmed exit.
/// Output paths are not removed. Host failures have kind [`ErrorKind::Io`](crate::ErrorKind::Io),
/// retaining their source. An unconfirmed exit has kind
/// [`ErrorKind::StillRunning`](crate::ErrorKind::StillRunning), with the original error.
pub fn spawn(prepared: Prepared) -> Result<Launched> {
    spawn_verified(prepared, spawn::Launched::process)
}

/// A confirmed VM exit, with the outcome-report read kept separate from exit proof.
#[derive(Debug)]
pub struct Stopped {
    /// Exit was confirmed before any forced termination was attempted.
    pub self_exited: bool,
    /// Forced termination through the verified process was attempted.
    pub forced: bool,
    /// The outcome summary, missing report, or report-read error after confirmed exit.
    pub report: Result<Option<Report>>,
}

/// Stops a VM, returning only after its verified process has exited.
///
/// Unless exit is already confirmed, calls `graceful` with `grace`, bounded by `deadline`,
/// then waits until that grace deadline. The callback must respect the supplied deadline;
/// its boolean request result is not exit proof. If exit is still unconfirmed, attempts
/// forced termination and waits until `deadline`. A failed termination is harmless only
/// when that final wait confirms exit.
///
/// An unconfirmed exit fails with [`ErrorKind::StillRunning`](crate::ErrorKind::StillRunning),
/// retaining host errors. The caller keeps the process in that case.
/// Report-read errors have kind [`ErrorKind::Io`](crate::ErrorKind::Io)
/// inside [`Stopped::report`], never hiding confirmed exit.
pub fn stop(
    process: &Process,
    outputs: &VmOutputs,
    graceful: impl FnOnce(Instant) -> bool,
    grace: Instant,
    deadline: Instant,
) -> Result<Stopped> {
    let failure = |kill_error: Option<&io::Error>| {
        let mut message = format!(
            "cannot confirm exit of VM process {}",
            process.reference().pid
        );
        if let Some(error) = kill_error {
            message.push_str(&format!("; forced termination failed: {error}"));
        }
        Error::new(crate::ErrorKind::StillRunning, message)
    };
    let wait = |until| {
        process
            .wait(until)
            .map_err(|error| failure(None).with_source(error))
    };
    let self_exited = if wait(Instant::now())?.is_some() {
        true
    } else {
        let grace = grace.min(deadline);
        graceful(grace);
        wait(grace)?.is_some()
    };
    if !self_exited {
        let kill_error = process.kill().err();
        let exited = process
            .wait(deadline)
            .map_err(|error| failure(kill_error.as_ref()).with_source(error))?;
        if exited.is_none() {
            let error = failure(kill_error.as_ref());
            return Err(match kill_error {
                Some(source) => error.with_source(source),
                None => error,
            });
        }
    }
    Ok(Stopped {
        self_exited,
        forced: !self_exited,
        report: report::read(&outputs.outcome),
    })
}

/// Keeps the child owned until its process identity is verified, or its exit is confirmed.
fn spawn_verified(
    prepared: Prepared,
    verify: impl FnOnce(&spawn::Launched) -> io::Result<Process>,
) -> Result<Launched> {
    spawn_verified_with_cleanup(prepared, verify, spawn::Launched::kill_and_confirm_exit)
}

/// Keeps cleanup proof distinct from a request to terminate the owned child.
fn spawn_verified_with_cleanup(
    prepared: Prepared,
    verify: impl FnOnce(&spawn::Launched) -> io::Result<Process>,
    cleanup: impl FnOnce(spawn::Launched) -> io::Result<()>,
) -> Result<Launched> {
    let Prepared {
        program,
        working_dir,
        windows_breakaway,
        foreground,
        capability,
        outputs,
        log,
        arguments,
        inherit,
        ready,
        ..
    } = prepared;
    let command = spawn::VmmCommand {
        program,
        arguments,
        working_dir,
        breakaway_from_job: windows_breakaway,
        inherit: inherit.iter().map(Inherited::new).collect(),
    };
    let child = spawn::spawn_vm_with_mode(&command, capability.as_bytes(), log, foreground)
        .map_err(|error| {
            let unconfirmed = error
                .get_ref()
                .and_then(|source| source.downcast_ref::<Error>())
                .is_some_and(|source| source.kind() == crate::ErrorKind::StillRunning);
            Error::new(
                if unconfirmed {
                    crate::ErrorKind::StillRunning
                } else {
                    crate::ErrorKind::Io
                },
                "cannot spawn the VM",
            )
            .with_source(error)
        })?;
    let process = match verify(&child) {
        Ok(process) => process,
        Err(error) => {
            let (kind, message) = match cleanup(child) {
                Ok(()) => (
                    crate::ErrorKind::Io,
                    "cannot verify the spawned VM".to_owned(),
                ),
                Err(cleanup) => (
                    crate::ErrorKind::StillRunning,
                    format!("cannot verify the spawned VM; cannot confirm its exit: {cleanup}"),
                ),
            };
            return Err(Error::new(kind, message).with_source(error));
        }
    };
    #[cfg(feature = "cli")]
    let child = if foreground {
        Some(child)
    } else {
        child.detach();
        None
    };
    #[cfg(not(feature = "cli"))]
    child.detach();
    Ok(Launched {
        process,
        outputs,
        capability,
        ready,
        #[cfg(feature = "cli")]
        child: ForegroundChild(child),
    })
}

/// Creates one private sparse capture disk and verifies the geometry OpenVMM presents.
fn capture_disk(
    path: &Path,
    expected: BlockGeometry,
    created: &mut Vec<PathBuf>,
) -> Result<BlockGeometry> {
    let failed = |error| {
        Error::io(format!("cannot create capture disk {}", path.display())).with_source(error)
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path).map_err(failed)?;
    created.push(path.to_owned());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(failed)?;
    }
    if let Some(problem) = platform::private_file_problem(&file).map_err(failed)? {
        return Err(Error::io(format!(
            "the capture disk {} {problem}",
            path.display()
        )));
    }
    platform::make_sparse(&file).map_err(failed)?;
    file.set_len(expected.length).map_err(failed)?;
    let actual = crate::host::sandbox::block_geometry(&file, path, "capture disk")?;
    if actual != expected {
        return Err(Error::unsupported(format!(
            "capture disk {} has actual geometry {actual:?}, not requested {expected:?}",
            path.display()
        )));
    }
    Ok(actual)
}

/// Refuses an existing output entry without opening, replacing, or removing it.
fn unused_output(path: &Path) -> Result<()> {
    let error = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => error,
        Ok(_) => io::Error::new(io::ErrorKind::AlreadyExists, "the output already exists"),
    };
    Err(Error::io(format!("cannot reserve VM output {}", path.display())).with_source(error))
}

#[cfg(test)]
mod tests;
