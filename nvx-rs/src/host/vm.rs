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
//! endpoint, which the library never writes to a file.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::host::config::{ConsoleAttachment, VmConfig};
use crate::host::paths::{absolute, normalized};
use crate::host::platform;
#[cfg(not(windows))]
use crate::host::platform::{check_store_dir as check_dir, create_store_dir as create_dir};
#[cfg(windows)]
use crate::host::platform::{check_vm_dir as check_dir, create_vm_dir as create_dir};
use crate::host::runtime::Runtime;
use crate::host::snapshot::Snapshot;
use crate::protocol::CAPABILITY_LEN;
use crate::{Error, Result};

/// OpenVMM's log of a launch.
const LOG: &str = "openvmm.log";
/// OpenVMM's outcome report of a launch.
const OUTCOME: &str = "outcome.json";
/// The control socket on Linux.
const CONTROL: &str = "control.sock";
/// The guest's boot-console socket on Linux.
const CONSOLE: &str = "console.sock";
/// The restore-ready socket on Linux.
const RESTORE_READY: &str = "restore-ready.sock";
/// OpenVMM's snapshot files: a capture's destination and a restore's snapshot view.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL2.12b and SL2.13 use it (#1102)")
)]
const OPENVMM: &str = "openvmm";
/// A finished capture's key.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL2.12b and SL2.14 use it (#1102)")
)]
const KEY: &str = "key.bin";
/// A finished capture's informational snapshot description.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL2.12b and SL2.14 use it (#1102)")
)]
const DESCRIPTION: &str = "snapshot.json";
/// The sparse placeholder image that a capture VM attaches.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL2.13's capture creates it (#1102)")
)]
const CAPTURE_IMAGE: &str = "capture-image.raw";
/// The dormant scratch that a capture VM attaches.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL2.13's capture creates it (#1102)")
)]
const CAPTURE_SCRATCH: &str = "capture-scratch.raw";
/// The prefix OpenVMM requires of its Windows console pipes.
const OPENVMM_PIPE: &str = "openvmm-microvm";
/// The prefix of a Windows restore-ready pipe.
const RESTORE_READY_PIPE: &str = "nvx-restore-ready";

/// The directory of a VM, which its caller creates and owns, private to the current user.
///
/// The library reserves the fixed names of its launch and snapshot files and endpoints, and
/// every name that starts with `.openvmm`; the caller may add files under other names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmDir {
    /// The directory's absolute path.
    path: PathBuf,
}

impl VmDir {
    /// Opens an existing directory private to the current user, as [`create_private_dir`] makes
    /// it. A relative path names a directory from the current directory. Changes nothing.
    ///
    /// Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) if the directory is not private or
    /// cannot be read, as when it does not exist.
    pub fn open(path: &Path) -> Result<Self> {
        // A trailing separator or `/.` would make the OS follow a final symbolic link before
        // applying the platform's no-follow check. Keep `..` components unresolved.
        let path = absolute(&normalized(path)).map_err(|error| io_error("open", path, error))?;
        let problem = check_dir(&path).map_err(|error| io_error("open", &path, error))?;
        check_privacy(&path, problem)?;
        Ok(Self { path })
    }

    /// Returns the directory as an absolute path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a launch's outputs, with a boot-console endpoint if requested. Creates nothing.
    pub(crate) fn outputs(&self, console: bool) -> Result<VmOutputs> {
        Ok(VmOutputs {
            log: self.path.join(LOG),
            outcome: self.path.join(OUTCOME),
            endpoint: self.endpoint(CONTROL, OPENVMM_PIPE)?,
            console: console
                .then(|| self.endpoint(CONSOLE, OPENVMM_PIPE))
                .transpose()?,
        })
    }

    /// Returns a restore-ready endpoint without listening on it.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "SL2.17's ReadyListener calls it (#1099)")
    )]
    pub(crate) fn restore_ready_endpoint(&self) -> Result<String> {
        self.endpoint(RESTORE_READY, RESTORE_READY_PIPE)
    }

    /// Returns the Linux socket `name` or a fresh Windows pipe with the prefix `pipe`.
    fn endpoint(&self, name: &str, pipe: &str) -> Result<String> {
        if cfg!(windows) {
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes)
                .map_err(|error| Error::io("cannot draw the name of a pipe").with_source(error))?;
            let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            return Ok(format!("//./pipe/{pipe}-{suffix}"));
        }
        self.path
            .join(name)
            .into_os_string()
            .into_string()
            .map_err(|_| {
                Error::unsupported(format!(
                    "the VM directory {} is not valid Unicode, which OpenVMM's endpoints must be",
                    self.path.display()
                ))
            })
    }
}

/// Creates a VM directory private to the current user, or checks an existing one without
/// changing it. The parent must exist.
///
/// On Linux, a new directory has mode 0700, whatever the umask and its parent's setgid bit. On
/// Windows, it belongs to the user, and its protected access control list allows the user alone
/// full access, which the files and directories created in it inherit.
///
/// Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) if an existing directory is not private or
/// the directory cannot be created or read.
pub fn create_private_dir(path: &Path) -> Result<()> {
    let path = normalized(path);
    let problem = create_dir(&path).map_err(|error| io_error("create", &path, error))?;
    check_privacy(&path, problem)
}

/// A launch's output files and endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmOutputs {
    /// OpenVMM's log, `openvmm.log` in the VM directory.
    pub log: PathBuf,
    /// OpenVMM's outcome report, `outcome.json` in the VM directory.
    pub outcome: PathBuf,
    /// The control endpoint: `control.sock` in the VM directory on Linux, or on Windows a fresh
    /// named pipe, `//./pipe/openvmm-microvm-` and 32 random lowercase hexadecimal digits.
    pub endpoint: String,
    /// The boot-console endpoint, if requested: `console.sock` in the VM directory on Linux, or
    /// on Windows another fresh `//./pipe/openvmm-microvm-` pipe.
    pub console: Option<String>,
}

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

/// A complete cold launch handed to the caller's launcher, with all its resources owned.
///
/// The launcher supplies the capability on stdin through a pipe closed to writers before
/// spawning, and uses the opened log for stdout and stderr. It allows only the standard streams
/// and `inherit`, under their original descriptor or handle numbers, into the child.
///
/// Preserve the launch contract of [`crate::host::supervisor::spawn`]: detach on Linux, clear
/// only the child's owner umask bits, and keep allowed descriptors close-on-exec in the caller;
/// on Windows, use an explicit handle list, restore temporary inheritability, start without a
/// console in the log-named job, and honor `breakaway_from_job`. Windows callers coordinate their
/// other spawns with the inheritance window. If setup fails after spawning, terminate and reap
/// only that child before returning an error. The caller journals recovery data and keeps the
/// VM directory and runtime inputs stable; this value starts no process or listener.
#[derive(Debug)]
pub struct VmmCommand {
    /// The checked OpenVMM executable, as the runtime supplied it.
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
}

/// A checked cold launch, with its capability, chosen outputs, and already opened log.
///
/// Creation starts no process and binds no listener. The caller keeps the VM directory and
/// runtime inputs stable, and may journal the capability and outputs before a later spawn.
/// Dropping a successful preparation closes its files but leaves its output paths to the caller.
#[derive(Debug)]
pub struct Prepared {
    /// The control credential, never written to disk.
    capability: Capability,
    /// The launch's fixed file paths and chosen endpoints.
    outputs: VmOutputs,
    /// The newly created log, kept open rather than reopened by name.
    log: File,
    /// The complete arguments, checked against the actual runtime and output paths.
    arguments: Vec<OsString>,
    /// Already opened resources added by the caller, retained until the preparation is dropped.
    inherit: Vec<InheritedHandle>,
    /// The executable supplied by the checked runtime.
    program: PathBuf,
    /// The checked VM working directory.
    working_dir: PathBuf,
    /// The runtime's Windows job-breakaway setting.
    breakaway_from_job: bool,
}

impl Prepared {
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

    /// Transfers the complete checked launch to the caller's launcher.
    ///
    /// Moves the same opened log and inherited resources, without cloning, reopening, rendering
    /// arguments again or rediscovering paths. Starts no process or listener. The caller follows
    /// [`VmmCommand`]'s launch contract and may retain its capability and outputs for recovery.
    pub fn into_command(self) -> VmmCommand {
        VmmCommand {
            program: self.program,
            arguments: self.arguments,
            working_dir: self.working_dir,
            capability: self.capability,
            outputs: self.outputs,
            inherit: self.inherit,
            log: self.log,
            breakaway_from_job: self.breakaway_from_job,
        }
    }
}

/// Creates an inspectable cold launch in `dir`, without starting a process or binding endpoints.
///
/// A cold launch needs [`VmConfig::boot`]. Restoring `from` a [`Snapshot`] is not supported yet.
/// The caller keeps the directory and runtime inputs stable during creation and a later spawn.
/// Runtime and configuration paths are used as supplied; output paths come from `dir`.
///
/// The log is created anew, privately, and kept open in [`Prepared`]. Other outputs are reserved
/// names that OpenVMM creates when it runs: existing entries at those names are refused, not
/// replaced. A failure closes and removes only the log that this invocation created, preserving
/// every pre-existing entry and the caller's inputs. The capability is never written to disk.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a restore, a missing
/// boot, a refused configuration, or a complete Windows command line beyond the portable limit.
/// The final check includes the actual runtime and output paths, with no reserve, on every host.
/// Filesystem failures have kind [`ErrorKind::Io`](crate::ErrorKind::Io), retaining their source;
/// a rollback failure is reported along with the original error.
pub fn create(
    runtime: &Runtime,
    config: &VmConfig,
    from: Option<&Snapshot>,
    dir: &VmDir,
) -> Result<Prepared> {
    if from.is_some() {
        return Err(Error::unsupported(
            "creating a VM from a Snapshot is not supported yet",
        ));
    }
    if config.boot.is_none() {
        return Err(Error::unsupported("creating a cold VM needs Boot"));
    }
    VmDir::open(dir.path())?;
    platform::check_working_dir(dir.path())?;
    let outputs = dir.outputs(config.console == ConsoleAttachment::Listener)?;
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
        let capability = Capability(crate::host::instance::generate_capability()?);
        let arguments = config.openvmm_arguments(
            runtime,
            &outputs.endpoint,
            &outputs.outcome,
            outputs.console.as_deref(),
        )?;
        Ok((capability, arguments))
    })();
    match checked {
        Ok((capability, arguments)) => Ok(Prepared {
            capability,
            outputs,
            log,
            arguments,
            inherit: Vec::new(),
            program: runtime.openvmm.clone(),
            working_dir: dir.path.clone(),
            breakaway_from_job: runtime.windows_breakaway,
        }),
        Err(error) => {
            drop(log);
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

/// Refuses an existing output entry without opening, replacing, or removing it.
fn unused_output(path: &Path) -> Result<()> {
    let error = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => error,
        Ok(_) => io::Error::new(io::ErrorKind::AlreadyExists, "the output already exists"),
    };
    Err(Error::io(format!("cannot reserve VM output {}", path.display())).with_source(error))
}

/// Retains the host's I/O failure, or classifies an unsupported host as unavailable.
fn io_error(action: &str, path: &Path, error: io::Error) -> Error {
    let message = format!("cannot {action} the VM directory {}", path.display());
    if error.kind() == io::ErrorKind::Unsupported {
        Error::unavailable(message).with_source(error)
    } else {
        Error::io(message).with_source(error)
    }
}

/// Reports why an existing VM directory is not private without changing its permissions.
fn check_privacy(path: &Path, problem: Option<String>) -> Result<()> {
    problem.map_or(Ok(()), |problem| {
        Err(Error::io(format!(
            "the VM directory {} {problem}",
            path.display()
        )))
    })
}

#[cfg(test)]
mod tests;
