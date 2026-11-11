//! The exec lock of a sandbox, which runs one managed exec at a time in it, across processes.
//!
//! An exec holds the OS lock of `exec.lock` in the sandbox's state directory from before it finds
//! the running VM until its workload ends. A release leaves the file, which `deprovision` removes
//! with the store's files, so that no release unlinks a file that another exec has open.

use std::fs::{File, TryLockError};

use super::store::{LockedState, open_lock};
use super::{Exit, Result, check_interrupt};
use crate::ErrorCode;
use crate::host::platform;

/// The name of the exec lock in a sandbox's state directory.
pub(crate) const NAME: &str = "exec.lock";

/// The held exec lock of a sandbox, which dropping it releases.
#[derive(Debug)]
pub(crate) struct ExecLock {
    file: File,
}

impl ExecLock {
    /// Tries the exec lock of the sandbox whose lifecycle lock `state` holds, without waiting, and
    /// returns `None` while another exec holds it. The lock file is created and checked as the
    /// store's own lock file is, and refused once its entry no longer names the locked file.
    pub(crate) fn try_acquire(state: &LockedState) -> Result<Option<Self>> {
        check_interrupt()?;
        let directory = state.directory()?;
        let guard = Self {
            file: open_lock(&state.dir().join(NAME))?,
        };
        match guard.file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => return Err(Exit::from_os_error(error)),
        }
        let held = platform::opened_file_id(&guard.file).map_err(Exit::from_os_error)?;
        if directory.entry_id(NAME).map_err(Exit::from_os_error)? != Some(held) {
            return Err(Exit::error(
                ErrorCode::Io,
                "sandbox exec lock was removed or replaced",
            ));
        }
        Ok(Some(guard))
    }
}

impl Drop for ExecLock {
    fn drop(&mut self) {
        // Unlock explicitly so a concurrently forked child's descriptor cannot extend admission.
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests;
