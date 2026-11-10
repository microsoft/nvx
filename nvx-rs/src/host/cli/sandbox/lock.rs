//! The CLI store's recoverable lifecycle lock and removal of its owned files.

use super::lifecycle::{CONFIG_NAME, LOCK_RETRY_INTERVAL, create_state_directory};
use crate::ErrorCode;
use crate::host::cli::deadline::Deadline;
use crate::host::cli::store::{LockedState, StateDir, StateFile};
use crate::host::cli::{Exit, Result, io_error};
use crate::host::platform;
use std::io;
use std::path::{Path, PathBuf};

/// Returns the error of a transition that still waits for another: Python's `TimeoutError`.
pub(crate) fn transition_in_progress() -> Exit {
    Exit::error(
        ErrorCode::SandboxBusy,
        "another lifecycle transition of the sandbox is still in progress",
    )
}

/// The exclusive lock that serializes the lifecycle transitions of a state directory: the
/// lifecycle lock of its sandbox's store, through which the transition changes the store's
/// records. Dropping it releases the lock.
pub(crate) struct LifecycleLock {
    /// The state directory.
    state_dir: PathBuf,
    /// The sandbox's locked store.
    locked: LockedState,
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
        loop {
            let taken = if provision {
                StateDir::create(state_dir)
            } else {
                StateDir::open(state_dir)
            }
            .and_then(|state| state.lock(deadline));
            let error = match taken {
                Ok(locked) => {
                    return Ok(Self {
                        state_dir: state_dir.to_path_buf(),
                        locked,
                    });
                }
                Err(error) => error,
            };
            match error.code() {
                Some(ErrorCode::NotFound) if !provision && !state_dir.is_dir() => {
                    return Err(Exit::error(
                        ErrorCode::SandboxNotProvisioned,
                        format!(
                            "sandbox state path is not a plain directory: {}",
                            state_dir.display()
                        ),
                    ));
                }
                Some(ErrorCode::NotFound) => {
                    if deadline.passed() {
                        return Err(transition_in_progress());
                    }
                    if provision {
                        create_state_directory(state_dir)?;
                    }
                    continue;
                }
                _ if cfg!(windows)
                    && (platform::is_deleted(state_dir)
                        || platform::is_deleted(&state_dir.join(StateFile::Lock.name()))) => {}
                _ => return Err(error),
            }
            if !deadline.pause(LOCK_RETRY_INTERVAL)? {
                return Err(transition_in_progress());
            }
        }
    }

    /// Returns the sandbox's locked store.
    pub(crate) fn locked(&mut self) -> &mut LockedState {
        &mut self.locked
    }

    /// Called only after exit proof, retaining the previous launch's log and outcome.
    pub(crate) fn clear_runtime(&self) -> Result {
        self.remove_files(&[
            StateFile::Capability,
            StateFile::Socket,
            StateFile::Journal,
            StateFile::Runtime,
        ])
    }

    pub(crate) fn clear_outputs(&self) -> Result {
        self.remove_files(&[StateFile::Socket, StateFile::Log, StateFile::Outcome])
    }

    fn remove_files(&self, names: &[StateFile]) -> Result {
        let directory = self.locked.directory()?;
        for name in names {
            directory.remove_file(name.name()).map_err(|error| {
                io_error(ErrorCode::Io, &error, &self.state_dir.join(name.name()))
            })?;
        }
        for entry in std::fs::read_dir(&self.state_dir).map_err(Exit::from_os_error)? {
            let name = entry.map_err(Exit::from_os_error)?.file_name();
            let name = name.to_str().ok_or_else(|| {
                Exit::error(ErrorCode::Io, "a sandbox file name is not valid Unicode")
            })?;
            if StateFile::owns_temporary(name) {
                directory.remove_file(name).map_err(Exit::from_os_error)?;
            }
        }
        directory.sync().map_err(Exit::from_os_error)
    }

    /// Removes the sandbox and its state directory through the store, as deprovision does once no
    /// OpenVMM of the sandbox can run, waiting up to `timeout` seconds for open files, such as the
    /// lock files of other transitions, that keep the emptied directory, and releases the lock.
    pub(crate) fn remove(self, timeout: f64) -> Result {
        let directory = self.locked.directory()?;
        for name in [StateFile::Journal, StateFile::Runtime] {
            if directory
                .entry_id(name.name())
                .map_err(Exit::from_os_error)?
                .is_some()
            {
                return Err(Exit::error(
                    ErrorCode::SandboxRunning,
                    "sandbox must be stopped before deprovision",
                ));
            }
        }
        let state_dir = self.state_dir.clone();
        let identity = platform::file_id(&state_dir).map_err(Exit::from_os_error)?;
        let names: Vec<_> = StateFile::ALL
            .into_iter()
            .filter(|name| !matches!(name, StateFile::Lock | StateFile::Config))
            .chain([StateFile::Config])
            .collect();
        self.remove_files(&names)?;
        let foreign = remaining(&state_dir)?;
        if !foreign.is_empty() {
            return Err(foreign_files(foreign));
        }
        directory
            .remove_file(StateFile::Lock.name())
            .map_err(Exit::from_os_error)?;
        directory.sync().map_err(Exit::from_os_error)?;
        drop(self);
        let deadline = Deadline::after(timeout);
        loop {
            match platform::file_id(&state_dir) {
                Ok(current) if current == identity => {}
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(Exit::from_os_error(error)),
            }
            match std::fs::remove_dir(&state_dir) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => {
                    if state_dir.join(CONFIG_NAME).exists() {
                        return Err(Exit::error(
                            ErrorCode::SandboxProvisioned,
                            "sandbox was provisioned again while deprovision removed it",
                        ));
                    }
                    let foreign = remaining(&state_dir)?;
                    if !foreign.is_empty() {
                        return Err(foreign_files(foreign));
                    }
                    if !deadline.pause(LOCK_RETRY_INTERVAL)? {
                        return Err(io_error(ErrorCode::TimedOut, &error, &state_dir));
                    }
                }
            }
        }
    }
}

impl Drop for LifecycleLock {
    fn drop(&mut self) {
        if [StateFile::Config, StateFile::Journal, StateFile::Runtime]
            .into_iter()
            .all(|name| {
                matches!(std::fs::symlink_metadata(self.state_dir.join(name.name())),
                Err(error) if error.kind() == io::ErrorKind::NotFound)
            })
        {
            if let Ok(directory) = self.locked.directory() {
                let _ = directory.remove_file(StateFile::Lock.name());
            }
        }
    }
}

fn remaining(path: &Path) -> Result<Vec<String>> {
    std::fs::read_dir(path)
        .map_err(Exit::from_os_error)?
        .filter_map(|entry| {
            let name = match entry {
                Ok(entry) => entry.file_name(),
                Err(error) => return Some(Err(Exit::from_os_error(error))),
            };
            let name = match name.into_string() {
                Ok(name) => name,
                Err(_) => {
                    return Some(Err(Exit::error(
                        ErrorCode::Io,
                        "a sandbox file name is not valid Unicode",
                    )));
                }
            };
            (name != StateFile::Lock.name() && !name.starts_with(".nfs")).then_some(Ok(name))
        })
        .collect()
}

fn foreign_files(names: Vec<String>) -> Exit {
    Exit::error(
        ErrorCode::ForeignFiles,
        format!(
            "sandbox state directory contains files not owned by NVX: {}",
            names.join(", ")
        ),
    )
}

#[cfg(test)]
mod tests;
