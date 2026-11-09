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

use crate::host::config::{ConsoleAttachment, RootConfig, UpperConfig, VmConfig};
use crate::host::filesystem;
use crate::host::paths::{absolute, normalized};
use crate::host::platform;
#[cfg(not(windows))]
use crate::host::platform::{check_store_dir as check_dir, create_store_dir as create_dir};
#[cfg(windows)]
use crate::host::platform::{check_vm_dir as check_dir, create_vm_dir as create_dir};
use crate::host::runtime::Runtime;
use crate::host::snapshot::Snapshot;
use crate::host::spawn::{self, VmmCommand};
use crate::host::{platform::inherit::Inherited, process::Process};
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

/// A checked cold launch, with its capability, chosen outputs, and already opened log.
///
/// Creation starts no process and binds no listener. The caller keeps the VM directory and
/// runtime inputs stable, and may journal the capability and outputs before a later spawn.
/// Dropping a successful preparation closes its files but leaves its output paths to the caller.
#[derive(Debug)]
pub struct Prepared {
    /// The executable checked during creation.
    program: PathBuf,
    /// The private VM directory in which the child starts.
    working_dir: PathBuf,
    /// Whether the child leaves the caller's Windows job.
    windows_breakaway: bool,
    /// The control credential, never written to disk.
    capability: Capability,
    /// The launch's fixed file paths and chosen endpoints.
    outputs: VmOutputs,
    /// The newly created log, kept open rather than reopened by name.
    log: File,
    /// The complete arguments, checked against the actual runtime and output paths.
    arguments: Vec<OsString>,
    /// Already opened resources added by the caller, retained until the preparation is dropped.
    inherit: Vec<File>,
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
    pub fn inherit(&mut self, file: File) {
        self.inherit.push(file);
    }

    /// Returns the same opened log that creation made, for example to take a liveness claim.
    pub fn log(&self) -> &File {
        &self.log
    }

    /// Returns the complete, checked OpenVMM arguments without starting a process.
    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

/// Creates an inspectable cold launch in `dir`, without starting a process or binding endpoints.
///
/// A cold launch needs [`VmConfig::boot`]. Restoring `from` a [`Snapshot`] is not supported yet.
/// The caller keeps the directory and runtime inputs stable during creation and a later spawn.
/// Caller-relative runtime and block-image paths become absolute during creation, without
/// resolving links or `..` components; output paths come from `dir`.
///
/// The log is created anew, privately, and kept open in [`Prepared`]. Other outputs are reserved
/// names that OpenVMM creates when it runs: existing entries at those names are refused, not
/// replaced. A failure closes and removes only the log that this invocation created, preserving
/// every pre-existing entry and the caller's inputs. The capability is never written to disk.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a restore, a missing
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
    if !cfg!(windows) {
        for endpoint in std::iter::once(&outputs.endpoint).chain(outputs.console.as_ref()) {
            if endpoint.len() >= 108 || endpoint.contains([',', '=']) {
                return Err(Error::unsupported(format!(
                    "the VM socket {endpoint} must be shorter than 108 bytes and hold no ',' or '='",
                )));
            }
        }
        let canonical = filesystem::canonicalize(dir.path())
            .map_err(|error| io_error("resolve", dir.path(), error))?;
        for name in std::iter::once(CONTROL).chain(outputs.console.as_ref().map(|_| CONSOLE)) {
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
            &runtime,
            &outputs.endpoint,
            &outputs.outcome,
            outputs.console.as_deref(),
        )?;
        Ok((capability, arguments))
    })();
    match checked {
        Ok((capability, arguments)) => Ok(Prepared {
            program: runtime.openvmm.clone(),
            working_dir: dir.path().to_owned(),
            windows_breakaway: runtime.windows_breakaway,
            capability,
            outputs,
            log,
            arguments,
            inherit: Vec::new(),
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
}

/// Starts a prepared cold launch and returns its verified, detached process.
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
/// retaining their source; an exit-confirmation failure is reported with the original error.
pub fn spawn(prepared: Prepared) -> Result<Launched> {
    spawn_verified(prepared, spawn::Launched::process)
}

/// Keeps the child owned until its process identity is verified, or its exit is confirmed.
fn spawn_verified(
    prepared: Prepared,
    verify: impl FnOnce(&spawn::Launched) -> io::Result<Process>,
) -> Result<Launched> {
    let Prepared {
        program,
        working_dir,
        windows_breakaway,
        capability,
        outputs,
        log,
        arguments,
        inherit,
    } = prepared;
    let command = VmmCommand {
        program,
        arguments,
        working_dir,
        breakaway_from_job: windows_breakaway,
        inherit: inherit.iter().map(Inherited::new).collect(),
    };
    let child = spawn::spawn_vm(&command, capability.as_bytes(), log)
        .map_err(|error| Error::io("cannot spawn the VM").with_source(error))?;
    let process = match verify(&child) {
        Ok(process) => process,
        Err(error) => {
            let cleanup = child.kill_and_confirm_exit();
            let message = match cleanup {
                Ok(()) => "cannot verify the spawned VM".to_owned(),
                Err(cleanup) => {
                    format!("cannot verify the spawned VM; cannot confirm its exit: {cleanup}")
                }
            };
            return Err(Error::io(message).with_source(error));
        }
    };
    child.detach();
    Ok(Launched {
        process,
        outputs,
        capability,
    })
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
