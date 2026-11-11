//! The lock of a state directory, which serializes the lifecycle transitions of its sandbox.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::ops::Deref;
use std::path::Path;

use super::{StateDir, StateFile, failed};
use crate::ErrorCode;
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL};
use crate::host::cli::{Exit, Result};
use crate::host::{filesystem, platform};

/// Mode of the lock file on Unix: readable and writable by the current user only.
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// A state directory whose lock this holds, through which a lifecycle transition of its sandbox
/// changes its files. Dropping it releases the lock.
#[derive(Debug)]
pub(crate) struct LockedState {
    /// The state directory.
    state: StateDir,
    /// The locked file, whose lock lasts until this is dropped.
    lock: File,
}

impl Deref for LockedState {
    type Target = StateDir;

    fn deref(&self) -> &StateDir {
        &self.state
    }
}

impl Drop for LockedState {
    /// Releases the lock before its file closes: on Linux, a process that another thread is
    /// spawning holds a copy of every descriptor from its fork until its exec, and the lock lasts
    /// while any copy of its open file remains, so closing the file alone could keep the lock held
    /// until that process executes its program.
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

impl LockedState {
    pub(crate) fn directory(&self) -> Result<platform::Directory> {
        let directory = platform::Directory::open(self.dir()).map_err(Exit::from_os_error)?;
        let held = platform::opened_file_id(&self.lock).map_err(Exit::from_os_error)?;
        if directory
            .entry_id(StateFile::Lock.name())
            .map_err(Exit::from_os_error)?
            != Some(held)
        {
            return Err(Exit::error(
                ErrorCode::Io,
                "the directory that the path names does not hold the sandbox's lock",
            ));
        }
        Ok(directory)
    }
}

impl StateDir {
    /// Takes the lock of the directory, trying it again every 25 ms until `deadline`.
    ///
    /// Fails with [`ErrorCode::SandboxBusy`] once the deadline has passed, after one try at least,
    /// and with [`Exit::Interrupted`] if Ctrl-C arrives meanwhile. The lock file is created if
    /// needed, readable only by the current user. A lock file that is a link, or on Windows any
    /// other reparse point, or that is not a regular file, is refused at once, since a lock taken
    /// through a link would hold the file that it names, and so is one that another user may
    /// access, who could hold its lock. A lock file that another transition removed or replaced
    /// after this opened it locks nothing, so the lock is taken again: at once the first time,
    /// and after a pause, until the deadline, when that happens again right after.
    ///
    /// Once held, the lock recovers an interrupted launch before returning. Recovery can end a
    /// verified VM, but retains indeterminate records and reports the cause of its refusal.
    pub(crate) fn lock(&self, deadline: Deadline) -> Result<LockedState> {
        let path = self.path(StateFile::Lock);
        let mut retried_at_once = false;
        loop {
            let file = open_lock(&path)?;
            loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(TryLockError::WouldBlock) => {
                        if !deadline.pause(RETRY_INTERVAL)? {
                            return Err(busy());
                        }
                    }
                    Err(TryLockError::Error(error)) => return Err(lock_failed(&path)(error)),
                }
            }
            if names_locked_file(&path, &file)? {
                let mut locked = LockedState {
                    state: self.clone(),
                    lock: file,
                };
                locked
                    .recover(deadline.instant())
                    .map_err(crate::host::cli::sandbox::error_with_cause)?;
                return Ok(locked);
            }
            if retried_at_once && !deadline.pause(RETRY_INTERVAL)? {
                return Err(busy());
            }
            retried_at_once = !retried_at_once;
        }
    }
}

/// Returns the error of a transition that still waits for another past its deadline.
fn busy() -> Exit {
    Exit::error(
        ErrorCode::SandboxBusy,
        "another lifecycle transition of the sandbox is still in progress",
    )
}

/// Builds an error mapper for a failure to lock the lock file at `path`.
fn lock_failed(path: &Path) -> impl FnOnce(io::Error) -> Exit {
    failed(format!("cannot lock {}", path.display()))
}

/// Returns the error of a lock file that is not a regular file.
fn not_regular() -> io::Error {
    io::Error::other("is not a regular file")
}

/// Opens the lock file at `path`, creating it readable only by the current user if it does not
/// exist, and refuses it unless it is a regular file, not a link, that no one else may access.
///
/// On Unix, a lock and the checks of the locked file need no read access, so the file is opened
/// only for writing, and the open does not wait, as it would for a FIFO without a reader. The
/// umask may have cleared even the current user's permission to write it, so a lock file of the
/// current user that the user can open for writing no more is given its mode again.
pub(crate) fn open_lock(path: &Path) -> Result<File> {
    let open = || {
        let mut options = OpenOptions::new();
        options
            .read(cfg!(windows))
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            use nix::fcntl::OFlag;

            options
                .mode(PRIVATE_FILE_MODE)
                .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;

            /// `FILE_FLAG_OPEN_REPARSE_POINT`, which opens a link as itself.
            const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(OPEN_REPARSE_POINT);
        }
        options.open(path)
    };
    #[cfg(target_os = "linux")]
    let opened = match open() {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            platform::restore_private_file_mode(
                path,
                nix::sys::stat::Mode::from_bits_truncate(PRIVATE_FILE_MODE),
            )
            .map_err(lock_failed(path))?;
            open()
        }
        opened => opened,
    };
    #[cfg(not(target_os = "linux"))]
    let opened = open();
    #[cfg(unix)]
    {
        use nix::errno::Errno;

        // `O_NOFOLLOW` refuses a link, and an open that does not wait refuses a FIFO without a
        // reader, or a socket.
        if let Err(error) = &opened
            && [Errno::ELOOP, Errno::ENXIO]
                .into_iter()
                .any(|errno| error.raw_os_error() == Some(errno as i32))
        {
            return Err(lock_failed(path)(not_regular()));
        }
    }
    let file = opened.map_err(lock_failed(path))?;
    let metadata = file.metadata().map_err(lock_failed(path))?;
    if !metadata.file_type().is_file() || filesystem::is_reparse_point(&metadata) {
        return Err(lock_failed(path)(not_regular()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & PRIVATE_FILE_MODE != PRIVATE_FILE_MODE {
            file.set_permissions(std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
                .map_err(lock_failed(path))?;
        }
    }
    if let Some(problem) = platform::private_file_problem(&file).map_err(lock_failed(path))? {
        return Err(Exit::error(
            ErrorCode::Io,
            format!("the sandbox lock {} {problem}", path.display()),
        ));
    }
    Ok(file)
}

/// Returns whether `path` still names the file that `file` holds locked, which another transition
/// may have removed or replaced after it was opened.
///
/// On Windows, a file system without POSIX deletion keeps a removed file under its name until its
/// last handle closes, and refuses to open it until then, which the check takes for a removal.
fn names_locked_file(path: &Path, file: &File) -> Result<bool> {
    let locked = platform::opened_file_id(file).map_err(lock_failed(path))?;
    match platform::file_id(path) {
        Ok(current) => Ok(current == locked),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied => Ok(false),
        Err(error) => Err(lock_failed(path)(error)),
    }
}
