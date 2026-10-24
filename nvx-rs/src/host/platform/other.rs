use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::Transport;
use crate::host::supervisor::Inherited;
use crate::hypervisor::Hypervisor;

#[cfg(all(feature = "cli", unix))]
pub(crate) use crate::host::sys::{interrupted, watch_interrupts, write_interruptibly};

/// Whether this host can run NVX microVMs.
pub const SUPPORTED: bool = false;

/// Does nothing: this host has no console interrupts to watch.
#[cfg(all(feature = "cli", not(unix)))]
pub(crate) fn watch_interrupts() -> io::Result<()> {
    Ok(())
}

/// Reports no interrupt: this host has no console interrupts to watch.
#[cfg(all(feature = "cli", not(unix)))]
pub(crate) fn interrupted() -> bool {
    false
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the openvmm backend supports Linux and Windows hosts only",
    )
}

/// Reports every process as gone: this host runs no OpenVMM processes.
pub fn process_start_time(_pid: u32) -> io::Result<Option<u64>> {
    Ok(None)
}

/// A verified process, which this host never holds.
pub(crate) enum ProcessHandle {}

impl ProcessHandle {
    /// Never called: no handle exists.
    pub(crate) fn has_exit_event(&self) -> bool {
        match *self {}
    }

    /// Never called: no handle exists.
    pub(crate) fn wait(&self, _deadline: Instant) -> io::Result<Option<bool>> {
        match *self {}
    }

    /// Never called: no handle exists.
    pub(crate) fn kill(&self, _pid: u32) -> io::Result<()> {
        match *self {}
    }
}

/// Reports every process as gone, as [`process_start_time`] does.
pub(crate) fn open_process(_pid: u32, _start_time: u64) -> io::Result<Option<ProcessHandle>> {
    Ok(None)
}

/// Fails: this host cannot verify processes.
pub(crate) fn pin_child(_pid: u32) -> io::Result<(ProcessHandle, u64)> {
    Err(unsupported())
}

/// Fails: this host has no boot identity that NVX reads.
pub(crate) fn boot_id() -> io::Result<[u8; 16]> {
    Err(unsupported())
}

/// Fails: this host has no host identity that NVX reads.
pub(crate) fn host_id() -> io::Result<[u8; 16]> {
    Err(unsupported())
}

/// Fails: this host has no monotonic clock that NVX reads.
pub(crate) fn monotonic_ns() -> io::Result<u64> {
    Err(unsupported())
}

/// Fails: this host cannot export host paths.
pub fn file_identity(_path: &Path) -> io::Result<(u64, u64)> {
    Err(unsupported())
}

/// Returns whether the file at `path` is deleted: whether no file has that name, which a removal
/// takes at once.
pub(crate) fn is_deleted(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

pub(crate) fn opened_file_identity(_file: &File) -> io::Result<(u64, u64)> {
    Err(unsupported())
}

pub(crate) fn claim_log(_file: &File) -> io::Result<()> {
    Err(unsupported())
}

pub(crate) fn log_claim_held(_path: &Path, _identity: (u64, u64)) -> io::Result<bool> {
    Err(unsupported())
}

pub(crate) fn serves_launch(_pid: u32, _start_time: u64, _log: (u64, u64)) -> io::Result<bool> {
    Err(unsupported())
}

pub(crate) fn create_store_dir(_path: &Path) -> io::Result<Option<String>> {
    Err(unsupported())
}

pub(crate) fn check_store_dir(_path: &Path) -> io::Result<Option<String>> {
    Err(unsupported())
}

/// Fails: this host cannot check who may access a directory.
pub(crate) fn create_owner_only_dir(_path: &Path) -> io::Result<Option<String>> {
    Err(unsupported())
}

/// Fails: this host cannot check who may access a directory.
pub(crate) fn check_owner_only_dir(_path: &Path) -> io::Result<Option<String>> {
    Err(unsupported())
}

pub(crate) fn private_file_problem(_file: &File) -> io::Result<Option<String>> {
    Err(unsupported())
}

/// Leaves the command as it is, unless it should inherit descriptors: this host cannot restrict
/// what the command inherits, and fails with [`io::ErrorKind::Unsupported`] instead.
pub(crate) fn detach(_command: &mut Command, inherit: &[Inherited<'_>]) -> io::Result<()> {
    if inherit.is_empty() {
        Ok(())
    } else {
        Err(unsupported())
    }
}

/// Leaves the command as it is, unless it should inherit descriptors, as [`detach`] does.
#[cfg(feature = "cli")]
pub(crate) fn inherit_only(command: &mut Command, inherit: &[Inherited<'_>]) -> io::Result<()> {
    detach(command, inherit)
}

pub(crate) fn probe_hypervisor(_hypervisor: Hypervisor) -> Result<(), String> {
    Err(unsupported().to_string())
}

/// Creates `path` if needed.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

pub(crate) fn control_endpoint(_socket_path: &Path) -> io::Result<String> {
    Err(unsupported())
}

pub(crate) fn connect_endpoint(
    _endpoint: &str,
    _expected_pid: u32,
    _timeout: Duration,
) -> io::Result<Box<dyn Transport>> {
    Err(unsupported())
}

pub(crate) fn endpoint_server_pid(_endpoint: &str) -> io::Result<Option<u32>> {
    Ok(None)
}
