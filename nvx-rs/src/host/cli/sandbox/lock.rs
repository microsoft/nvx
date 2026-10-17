//! The lifecycle lock of a state directory, `_LifecycleLock` of the former `sandbox_lifecycle.py`,
//! with the file primitives that it shares with that Python implementation, so that an older
//! `nvx.py` and `nvx-rs` exclude each other:
//!
//! - `lifecycle.lock` is opened or created without following a link that it names: with
//!   `O_NOFOLLOW` on POSIX, and on Windows with `FILE_FLAG_OPEN_REPARSE_POINT` and every sharing
//!   mode, so that deprovision can delete it while others wait for it. It must be a regular file
//!   with one link, and not a reparse point.
//! - The lock is `flock(LOCK_EX | LOCK_NB)` on POSIX, and on Windows the lock of the file's first
//!   byte, as `msvcrt.locking(fd, LK_NBLCK, 1)` takes it at offset 0. A caller that cannot take
//!   it retries every 25 ms within its timeout.
//! - Deprovision writes `deprovisioned\n` into the lock file before it removes the file, so that
//!   a waiter that then takes the lock of the removed file knows that the directory goes away.
//!
//! [`LifecycleLock::acquire`] and [`LifecycleLock::release`] follow `__enter__` and `release`
//! step by step; see the comments of `_LifecycleLock`, in the history of
//! `scripts/nvx_tools/sandbox_lifecycle.py`, for the protocol.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};

use super::lifecycle::{
    CONFIG_NAME, LOCK_NAME, LOCK_RETRY_INTERVAL, LOCK_TOMBSTONE, prepare_state_directory,
};
use crate::host::cli::deadline::Deadline;
use crate::host::cli::{Exit, Result};
use crate::host::python::fs::{self, os_error_message};
use crate::host::python::path::PurePath;

/// How Python classifies an `OSError`, as far as the lifecycle tells the classes apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OsErrorClass {
    /// `FileNotFoundError`: `ENOENT`.
    NotFound,
    /// `PermissionError`: `EACCES` or `EPERM`.
    PermissionDenied,
    /// Any other `OSError`.
    Other,
}

/// Returns the class of the `OSError` that Python raises for `error`. On Windows, Python maps the
/// Win32 error to an `errno` as `winerror_to_errno` does.
pub(crate) fn os_error_class(error: &io::Error) -> OsErrorClass {
    #[cfg(windows)]
    if let Some(code) = error.raw_os_error() {
        return match code {
            2 | 3 | 15 | 18 | 53 | 67 | 161 | 206 => OsErrorClass::NotFound,
            5 | 16 | 19..=36 | 65 | 82 | 83 | 108 | 132 | 158 | 167 => {
                OsErrorClass::PermissionDenied
            }
            _ => OsErrorClass::Other,
        };
    }
    match error.kind() {
        io::ErrorKind::NotFound => OsErrorClass::NotFound,
        io::ErrorKind::PermissionDenied => OsErrorClass::PermissionDenied,
        _ => OsErrorClass::Other,
    }
}

/// Returns whether `error` is `ELOOP`, with which `O_NOFOLLOW` refuses a symbolic link.
fn is_loop(error: &io::Error) -> bool {
    #[cfg(unix)]
    return error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32);
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

/// Returns the error of a lock file that is not a regular file with one name.
fn not_standalone(path: &PurePath) -> Exit {
    Exit::error(format!(
        "sandbox lifecycle lock is not a standalone regular file: {path}"
    ))
}

/// Returns the message of an `OSError` that opening the lock file at `path` raised: with the
/// file's name from `os.open` on POSIX, and without it from `ctypes.WinError` on Windows.
fn open_error(error: &io::Error, path: &PurePath) -> Exit {
    let path = if cfg!(windows) {
        None
    } else {
        Some(path.as_str())
    };
    Exit::error(os_error_message(error, path))
}

/// Opens or creates `path` without following a link that it names, as `_open_lock_path` does.
fn open_lock_path(path: &PurePath) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options
            .mode(0o600)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        /// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`.
        const SHARE_ALL: u32 = 0x7;
        /// `FILE_ATTRIBUTE_NORMAL`.
        const NORMAL: u32 = 0x80;
        /// `FILE_FLAG_OPEN_REPARSE_POINT`.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options
            .share_mode(SHARE_ALL)
            .attributes(NORMAL)
            .custom_flags(OPEN_REPARSE_POINT);
    }
    options.open(path.as_path())
}

/// Why opening the lock file failed.
enum OpenFailure {
    /// An `OSError` that the caller may handle.
    Os(io::Error),
    /// Any other failure.
    Exit(Exit),
}

/// Opens the lock file of a state directory, creating it if needed, as `_open_lock_file` does:
/// it must be a regular file that no other name links, and not a reparse point.
fn open_lock_file(path: &PurePath) -> std::result::Result<File, OpenFailure> {
    let file = match open_lock_path(path) {
        Ok(file) => file,
        Err(error) if is_loop(&error) => return Err(OpenFailure::Exit(not_standalone(path))),
        Err(error) => return Err(OpenFailure::Os(error)),
    };
    let status = fs::fstat(&file)
        .map_err(|error| OpenFailure::Exit(Exit::error(os_error_message(&error, None))))?;
    if !status.regular || status.links != 1 || status.reparse_point {
        return Err(OpenFailure::Exit(not_standalone(path)));
    }
    Ok(file)
}

/// Takes the exclusive lock of an open lock file unless another caller holds it, and returns
/// whether it took it, as `_try_lock_file` does.
fn try_lock_file(file: &File) -> Result<bool> {
    #[cfg(windows)]
    return Ok(crate::host::platform::try_lock_first_byte(file));
    #[cfg(not(windows))]
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(Exit::error(os_error_message(&error, None)))
        }
    }
}

/// Releases the lock of an open lock file, as `_unlock_file` does.
fn unlock_file(file: &File) -> io::Result<()> {
    #[cfg(windows)]
    return crate::host::platform::unlock_first_byte(file);
    #[cfg(not(windows))]
    file.unlock()
}

/// Returns whether `path` still names the open `file`, as `_names_open_file` does. Windows keeps
/// a removed file in its directory until its last handle closes, and denies access to it
/// meanwhile, which counts as another file.
fn names_open_file(path: &PurePath, file: &File) -> io::Result<bool> {
    let current = match fs::stat(path.as_path()) {
        Ok(current) => current,
        Err(error) => {
            return match os_error_class(&error) {
                OsErrorClass::NotFound => Ok(false),
                OsErrorClass::PermissionDenied if cfg!(windows) => Ok(false),
                _ => Err(error),
            };
        }
    };
    Ok(current.identity() == fs::fstat(file)?.identity)
}

/// Reads up to `size` bytes from the start of the lock file, as `_read_lock_file` does.
fn read_lock_file(mut file: &File, size: usize) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut data = Vec::with_capacity(size);
    let read = (&mut file).take(size as u64).read_to_end(&mut data);
    file.seek(SeekFrom::Start(0))?;
    read.map(|_| data)
}

/// Writes `data` over the start of the lock file, as `_write_lock_file` does.
fn write_lock_file(mut file: &File, data: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = data;
    let written = (|| {
        while !remaining.is_empty() {
            let count = file.write(remaining)?;
            if count == 0 {
                return Err(io::Error::other(
                    "sandbox lifecycle lock write made no progress",
                ));
            }
            remaining = &remaining[count..];
        }
        Ok(())
    })();
    // `msvcrt.locking()` unlocks from the file position, as it locked.
    file.seek(SeekFrom::Start(0))?;
    written
}

/// Returns the error of a transition that still waits for another: Python's `TimeoutError`.
pub(crate) fn transition_in_progress() -> Exit {
    Exit::error("another lifecycle transition of the sandbox is still in progress")
}

/// Returns `os.lstat(path)`'s identity, or `None` if the path cannot be examined, as
/// `_directory_status` does.
pub(crate) fn directory_status(path: &PurePath) -> Option<(u64, u128)> {
    fs::lstat(path.as_path())
        .ok()
        .map(|status| status.identity())
}

/// Returns whether `name` is a removed lock file that a handle still keeps, as `_lingers` does:
/// NFS renames a removed file that is still open to `.nfs*`, and Windows file systems without
/// POSIX deletion keep it under its own name.
pub(crate) fn lingers(name: &str) -> bool {
    name == LOCK_NAME || name.starts_with(".nfs")
}

/// Returns whether `error` is how `rmdir` reports a directory that is not empty.
pub(crate) fn directory_not_empty(error: &io::Error) -> bool {
    #[cfg(windows)]
    let not_empty = matches!(error.raw_os_error(), Some(145 | 80 | 183));
    #[cfg(unix)]
    let not_empty = matches!(
        error.raw_os_error(),
        Some(code) if code == nix::errno::Errno::ENOTEMPTY as i32
            || code == nix::errno::Errno::EEXIST as i32
    );
    #[cfg(not(any(windows, unix)))]
    let not_empty = error.kind() == io::ErrorKind::DirectoryNotEmpty;
    not_empty
}

/// Returns the names of the entries of `directory`.
pub(crate) fn entries(directory: &PurePath) -> io::Result<Vec<String>> {
    std::fs::read_dir(directory.as_path())?
        .map(|entry| {
            let name = entry?.file_name();
            name.into_string()
                .map_err(|_| crate::host::python::not_unicode("a file name"))
        })
        .collect()
}

/// Waits while lingering lock files keep the directory that deprovision removes, as
/// `_wait_for_handoff` does. The wait ends once the path names no directory or another one, or
/// the directory holds anything else; a directory that nothing keeps any longer is removed here,
/// in case deprovision has given up.
fn wait_for_handoff(state_dir: &PurePath, removing: (u64, u128), deadline: Deadline) -> Result {
    loop {
        if directory_status(state_dir) != Some(removing) {
            return Ok(());
        }
        let Ok(names) = entries(state_dir) else {
            return Ok(());
        };
        if !names.iter().all(|name| lingers(name)) {
            return Ok(());
        }
        if names.is_empty() {
            match std::fs::remove_dir(state_dir.as_path()) {
                Ok(()) => return Ok(()),
                Err(error) if !directory_not_empty(&error) => return Ok(()),
                Err(_) => {}
            }
        }
        if !deadline.pause(LOCK_RETRY_INTERVAL)? {
            return Err(transition_in_progress());
        }
    }
}

/// The exclusive lock that serializes the lifecycle transitions of a state directory.
pub(crate) struct LifecycleLock {
    /// The state directory.
    state_dir: PurePath,
    /// Its lock file.
    path: PurePath,
    /// The locked file, until the lock is released.
    file: Option<File>,
}

impl LifecycleLock {
    /// Takes the lock of `state_dir` within `timeout` seconds, as `_LifecycleLock(state_dir,
    /// timeout, provision=provision).__enter__()` does.
    ///
    /// A caller that takes the lock of a lock file that its holder removed meanwhile holds no
    /// lock. Unless it provisions, it retries only while the sandbox is still provisioned, and a
    /// provision that finds the lock file or the directory removed waits until the old directory
    /// is gone, then provisions a new one, unless another provision has done so first.
    pub(crate) fn acquire(state_dir: &PurePath, timeout: f64, provision: bool) -> Result<Self> {
        if !(timeout > 0.0 && timeout < f64::INFINITY) {
            return Err(Exit::error(
                "lifecycle timeout must be finite and greater than 0",
            ));
        }
        let path = state_dir.join(LOCK_NAME);
        let deadline = Deadline::after(timeout);
        loop {
            let file = match open_lock_file(&path) {
                Ok(file) => file,
                Err(OpenFailure::Exit(exit)) => return Err(exit),
                Err(OpenFailure::Os(error)) => match os_error_class(&error) {
                    // Deprovision removed the state directory meanwhile, which only a provision
                    // creates again.
                    OsErrorClass::NotFound => {
                        if !provision {
                            return Err(Exit::error(format!(
                                "sandbox state path is not a plain directory: {state_dir}"
                            )));
                        }
                        if deadline.passed() {
                            return Err(transition_in_progress());
                        }
                        prepare_state_directory(state_dir.as_str(), true)?;
                        continue;
                    }
                    // Without POSIX deletion, Windows denies opening a removed lock file until
                    // its last handle closes.
                    OsErrorClass::PermissionDenied
                        if provision && cfg!(windows) && deadline.pause(LOCK_RETRY_INTERVAL)? =>
                    {
                        continue;
                    }
                    _ => return Err(open_error(&error, &path)),
                },
            };
            while !try_lock_file(&file)? {
                if !deadline.pause(LOCK_RETRY_INTERVAL)? {
                    return Err(transition_in_progress());
                }
            }
            if names_open_file(&path, &file)
                .map_err(|error| Exit::error(os_error_message(&error, Some(path.as_str()))))?
            {
                return Ok(Self {
                    state_dir: state_dir.clone(),
                    path,
                    file: Some(file),
                });
            }
            let tombstone = read_lock_file(&file, LOCK_TOMBSTONE.len())
                .map_err(|error| Exit::error(os_error_message(&error, None)))?;
            // Deprovision leaves the mark only if lingering lock files, such as the one that this
            // handle may keep, kept it from removing the directory, which the path then still
            // names.
            let removing = if provision && tombstone == LOCK_TOMBSTONE {
                directory_status(state_dir)
            } else {
                None
            };
            drop(file);
            if !provision {
                if !fs::lexists(state_dir.join(CONFIG_NAME).as_path()) {
                    return Err(Exit::error("sandbox is not provisioned"));
                }
            } else if let Some(removing) = removing {
                wait_for_handoff(state_dir, removing, deadline)?;
            }
            if deadline.passed() {
                return Err(transition_in_progress());
            }
        }
    }

    /// Removes the held lock file, marking it so that waiters know why, as `remove` does. If the
    /// removal fails, the mark goes again, as deprovision then keeps the directory.
    pub(crate) fn remove(&mut self) -> Result {
        let file = self.file.as_ref().expect("the lock is held");
        let removed = write_lock_file(file, LOCK_TOMBSTONE)
            .map_err(|error| Exit::error(os_error_message(&error, None)))
            .and_then(|()| {
                std::fs::remove_file(self.path.as_path()).map_err(|error| {
                    Exit::error(os_error_message(&error, Some(self.path.as_str())))
                })
            });
        if removed.is_err() {
            self.retract();
        }
        removed
    }

    /// Clears the mark of the removed lock file, so that waiters retry at once, as `retract`
    /// does.
    pub(crate) fn retract(&mut self) {
        if let Some(file) = &self.file {
            let _ = write_lock_file(file, &[0; LOCK_TOMBSTONE.len()]);
        }
    }

    /// Releases the lock, as `release` does: the lock file of a directory that holds no
    /// provisioned sandbox goes with it.
    pub(crate) fn release(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        if !fs::lexists(self.state_dir.join(CONFIG_NAME).as_path())
            && names_open_file(&self.path, &file).unwrap_or(false)
        {
            let _ = std::fs::remove_file(self.path.as_path());
        }
        // Closing the file releases the lock even if the unlock fails.
        let _ = unlock_file(&file);
    }
}

impl Drop for LifecycleLock {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_dir(directory: &tempfile::TempDir) -> PurePath {
        PurePath::native(directory.path().to_str().unwrap())
    }

    #[test]
    fn the_lock_excludes_other_holders_until_released() {
        let directory = tempfile::tempdir().unwrap();
        let state = state_dir(&directory);
        std::fs::write(state.join(CONFIG_NAME).as_path(), "{}").unwrap();
        let mut held = LifecycleLock::acquire(&state, 10.0, false).unwrap();
        let started = std::time::Instant::now();
        let error = LifecycleLock::acquire(&state, 0.1, false).err().unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(100));
        assert_eq!(
            error.message(),
            Some("another lifecycle transition of the sandbox is still in progress")
        );
        held.release();
        drop(LifecycleLock::acquire(&state, 10.0, false).unwrap());
        // A provisioned directory keeps its lock file.
        assert!(state.join(LOCK_NAME).as_path().is_file());
    }

    #[test]
    fn released_locks_leave_no_file_in_unprovisioned_directories() {
        let directory = tempfile::tempdir().unwrap();
        let state = state_dir(&directory);
        drop(LifecycleLock::acquire(&state, 10.0, true).unwrap());
        assert!(!state.join(LOCK_NAME).as_path().exists());
        let error = LifecycleLock::acquire(&state.join("missing"), 1.0, false)
            .err()
            .unwrap();
        assert_eq!(
            error.message().unwrap(),
            format!(
                "sandbox state path is not a plain directory: {}",
                state.join("missing")
            )
        );
    }

    #[test]
    fn linked_lock_files_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let state = state_dir(&directory);
        let other = state.join("other");
        std::fs::write(other.as_path(), "").unwrap();
        if std::fs::hard_link(other.as_path(), state.join(LOCK_NAME).as_path()).is_err() {
            return;
        }
        let error = LifecycleLock::acquire(&state, 1.0, true).err().unwrap();
        assert_eq!(
            error.message().unwrap(),
            format!(
                "sandbox lifecycle lock is not a standalone regular file: {}",
                state.join(LOCK_NAME)
            )
        );
    }

    #[test]
    fn deprovision_marks_the_removed_lock_file() {
        let directory = tempfile::tempdir().unwrap();
        let state = state_dir(&directory);
        let mut lock = LifecycleLock::acquire(&state, 10.0, true).unwrap();
        let file = lock.file.as_ref().unwrap().try_clone().unwrap();
        lock.remove().unwrap();
        assert_eq!(read_lock_file(&file, 64).unwrap(), LOCK_TOMBSTONE);
        lock.retract();
        assert_eq!(
            read_lock_file(&file, 64).unwrap(),
            vec![0; LOCK_TOMBSTONE.len()]
        );
        assert!(!state.join(LOCK_NAME).as_path().exists());
    }
}
