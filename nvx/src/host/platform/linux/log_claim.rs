//! The claim of a launch's log: the lock on the log that its VMM inherits with it, and the checks
//! of whether a process of the launch still holds the log.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use super::identity::opened_file_identity;
use super::process::process_start_time;
use crate::host::sys::Errno;

/// Claims a launch's fresh log, which `file` holds open.
///
/// The claim is an exclusive `flock` lock. It belongs to the open file description, which the
/// VMM shares through the standard output and error that it inherits, and only the last close of
/// that description releases it, so it outlives the caller while the VMM runs.
pub(crate) fn claim_log(file: &File) -> io::Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(io::Error::other("another process holds the log")),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Returns whether a process still holds the claimed log at `path`, which must be the file
/// `identity`: whether its `flock` claim is held through another open file description.
///
/// The open follows no link and does not wait, as it would for a FIFO, so that only the claimed
/// file is examined.
pub(crate) fn log_claim_held(path: &Path, identity: (u64, u64)) -> io::Result<bool> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_NONBLOCK).bits())
        .open(path)?;
    if opened_file_identity(&file)? != identity {
        return Err(io::Error::other(format!(
            "{} is not the claimed log",
            path.display()
        )));
    }
    // A lock taken here goes when the file closes, at the end of this function.
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Returns whether the live process `pid` that started at `start_time`, which serves the control
/// endpoint of a launch whose log is the file `log`, is that launch's VMM: whether it holds the
/// log as its standard output.
///
/// The endpoint is the same socket path for every launch of a sandbox, so the log, which every
/// launch creates afresh, tells its VMM apart. The start time is read again after the
/// descriptor, so the answer cannot describe another process that reused the ID meanwhile.
pub(crate) fn serves_launch(pid: u32, start_time: u64, log: (u64, u64)) -> io::Result<bool> {
    if process_start_time(pid)? != Some(start_time) {
        return Ok(false);
    }
    let held = match fs::metadata(format!("/proc/{pid}/fd/1")) {
        Ok(metadata) => (metadata.dev(), metadata.ino()) == log,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::ESRCH as i32) =>
        {
            false
        }
        Err(error) => return Err(error),
    };
    Ok(held && process_start_time(pid)? == Some(start_time))
}
