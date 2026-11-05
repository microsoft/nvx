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

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::host::paths::{absolute, normalized};
#[cfg(not(windows))]
use crate::host::platform::{check_store_dir as check_dir, create_store_dir as create_dir};
#[cfg(windows)]
use crate::host::platform::{check_vm_dir as check_dir, create_vm_dir as create_dir};
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "SL2.8's vm::create calls it (#1099)")
    )]
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
