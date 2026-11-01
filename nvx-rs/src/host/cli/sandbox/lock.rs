//! The lifecycle lock of a state directory: the store's
//! [lifecycle lock](crate::host::store#lifecycle-locks) of the sandbox, whose caller keeps its
//! configuration record, `config.json`, in the directory.
//!
//! - Provision, start, stop, and deprovision hold the lock for their whole transition, and exec
//!   while it finds the running VM. A caller that cannot take the lock retries every 25 ms
//!   within its timeout, and Ctrl-C ends the wait.
//! - The store takes the lock of the file that the path names when the lock is granted. A
//!   provision whose directory deprovision removed meanwhile creates it again; any other
//!   transition then fails, as for a path that names no directory.
//! - Deprovision [removes](crate::host::store#removal) the sandbox through the store, which
//!   removes the lock file before the directory. A transition that waited for it then finds the
//!   directory gone, or takes the lock of a new lock file in the directory and finds no sandbox
//!   there. Every transition that leaves the directory without a provisioned sandbox removes its
//!   lock file as it releases the lock, so that the directory empties.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::error_with_cause;
use super::lifecycle::{CONFIG_NAME, LOCK_RETRY_INTERVAL, create_state_directory};
use crate::host::cli::deadline::Deadline;
use crate::host::cli::{Exit, Result, io_error};
use crate::host::paths;
use crate::host::store::{CallerFiles, LockedStore, RemovalError, SandboxStore};
use crate::{ErrorCode, ErrorKind};

/// Returns the kind of the I/O error that caused the store's `error`, if one did.
fn cause_kind(error: &crate::Error) -> Option<io::ErrorKind> {
    std::error::Error::source(error)?
        .downcast_ref::<io::Error>()
        .map(io::Error::kind)
}

/// Returns the error of a transition that still waits for another: Python's `TimeoutError`.
pub(crate) fn transition_in_progress() -> Exit {
    Exit::error(
        ErrorCode::SandboxBusy,
        "another lifecycle transition of the sandbox is still in progress",
    )
}

/// Returns the files that the CLI keeps in a state directory beside the store's: its
/// configuration record, whose presence means that the sandbox is provisioned.
fn caller_files() -> CallerFiles {
    CallerFiles::new(CONFIG_NAME).expect("the configuration record has a plain file name")
}

/// The exclusive lock that serializes the lifecycle transitions of a state directory: the
/// lifecycle lock of its sandbox's store, through which the transition changes the store's
/// records. Dropping it releases the lock.
pub(crate) struct LifecycleLock {
    /// The state directory.
    state_dir: PathBuf,
    /// The sandbox's locked store.
    locked: LockedStore,
}

impl LifecycleLock {
    /// Takes the lock of `state_dir` within `timeout` seconds.
    ///
    /// The directory must be private to the current user, as the store requires of its
    /// directory. A provision whose directory deprovision removed meanwhile creates it again; any
    /// other transition then fails, as for a path that names no directory.
    pub(crate) fn acquire(state_dir: &Path, timeout: f64, provision: bool) -> Result<Self> {
        if !(timeout > 0.0 && timeout < f64::INFINITY) {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                "lifecycle timeout must be finite and greater than 0",
            ));
        }
        let deadline = Deadline::after(timeout);
        let files = caller_files();
        loop {
            // The store tries the lock once for a deadline that has passed, so that this wait
            // checks for Ctrl-C between tries.
            let now = Some(Instant::now());
            let taken = if provision {
                SandboxStore::lock_for_provision(state_dir, &files, now)
            } else {
                SandboxStore::lock_existing(state_dir, &files, now)
            };
            let error = match taken {
                Ok(locked) => {
                    return Ok(Self {
                        state_dir: state_dir.to_path_buf(),
                        locked,
                    });
                }
                Err(error) => error,
            };
            match (error.kind(), cause_kind(&error)) {
                (ErrorKind::TimedOut, _) => {}
                // Deprovision removed the state directory meanwhile, which only a provision
                // creates again.
                (_, Some(io::ErrorKind::NotFound)) if !provision => {
                    return Err(Exit::error(
                        ErrorCode::SandboxNotProvisioned,
                        format!(
                            "sandbox state path is not a plain directory: {}",
                            state_dir.display()
                        ),
                    ));
                }
                // The store creates the state directory again itself, once its parent exists.
                (_, Some(io::ErrorKind::NotFound)) => {
                    if deadline.passed() {
                        return Err(transition_in_progress());
                    }
                    create_state_directory(state_dir)?;
                    continue;
                }
                // Any other failure ends the transition at once. The store waits itself for a
                // removed lock file or state directory that Windows keeps until its last handle
                // closes, so a denied access is one for good, such as a read-only lock file's.
                _ => return Err(error_with_cause(error)),
            }
            if !deadline.pause(LOCK_RETRY_INTERVAL)? {
                return Err(transition_in_progress());
            }
        }
    }

    /// Returns the sandbox's locked store.
    pub(crate) fn locked(&mut self) -> &mut LockedStore {
        &mut self.locked
    }

    /// Removes the sandbox and its state directory through the store, as deprovision does once no
    /// OpenVMM of the sandbox can run, waiting up to `timeout` seconds for open files, such as the
    /// lock files of other transitions, that keep the emptied directory, and releases the lock.
    pub(crate) fn remove(self, timeout: f64) -> Result {
        let Self { state_dir, locked } = self;
        let dir = locked.dir().to_owned();
        let failed = |error| removal_error(error, &state_dir, &dir);
        let Some(mut lingering) = locked.remove().map_err(failed)? else {
            return Ok(());
        };
        // A removed file that a handle still keeps, such as a lock file, stays in the directory,
        // as do the lock files of transitions that wait meanwhile. Each such transition finds no
        // sandbox, and removes its lock file as it releases the lock, so the directory empties
        // after the removal released its own, once the other handles close. The removal is tried
        // once at a time, so that this wait checks for Ctrl-C between tries.
        let deadline = Deadline::after(timeout);
        loop {
            match lingering.wait(Some(Instant::now())) {
                Ok(()) => return Ok(()),
                Err(RemovalError::TimedOut(error)) if !deadline.passed() => {
                    if !deadline.pause(LOCK_RETRY_INTERVAL)? {
                        return Err(failed(RemovalError::TimedOut(error)));
                    }
                }
                Err(error) => return Err(failed(error)),
            }
        }
    }
}

/// Returns the error of `nvx.py`'s deprovision for the store's removal of the sandbox in
/// `state_dir`, whose store names it `dir`, that `error` ended, with the code that the store's
/// conversion gives it.
fn removal_error(error: RemovalError, state_dir: &Path, dir: &Path) -> Exit {
    match error {
        // The CLI always declares its configuration record.
        error @ RemovalError::Undeclared => Exit::error(ErrorCode::Unsupported, error.to_string()),
        RemovalError::Launched => Exit::error(
            ErrorCode::SandboxRunning,
            "sandbox must be stopped before deprovision",
        ),
        RemovalError::Foreign(names) => Exit::error(
            ErrorCode::ForeignFiles,
            format!(
                "sandbox state directory contains files not owned by NVX: {}",
                names.join(", ")
            ),
        ),
        RemovalError::ProvisionedAgain => Exit::error(
            ErrorCode::SandboxProvisioned,
            "sandbox was provisioned again while deprovision removed it",
        ),
        RemovalError::TimedOut(error) => io_error(ErrorCode::TimedOut, &error, state_dir),
        RemovalError::Io { path, error } => {
            // The store names the directory, or one of its entries, by its own path.
            let shown = match path.strip_prefix(dir) {
                Ok(name) => state_dir.join(name),
                Err(_) => path,
            };
            io_error(ErrorCode::Io, &error, &paths::normalized(&shown))
        }
    }
}

#[cfg(test)]
mod tests;
