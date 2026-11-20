//! The lock of the downloads into a checkout.
//!
//! A download holds it exclusively while it searches for, downloads, and installs a release,
//! and a command that runs the artifacts of the checkout holds it shared while it decides whether
//! to download one, and then while it reads the artifacts and launches OpenVMM on them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Report;
use crate::ErrorCode;
use crate::host::artifacts::{ArtifactDir, REPO_BUILD_DIR};
use crate::host::cli::{Result, check_interrupt, io_error};

/// The name of the lock file of the downloads into a checkout, in its `build/`.
pub(super) const LOCK_NAME: &str = ".nvx-download.lock";
/// How long a download, or a command that waits for one, waits between its attempts to take the
/// lock.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Returns the path of the lock file of the downloads into the checkout `dir`.
fn lock_path(dir: &ArtifactDir) -> PathBuf {
    dir.root().join(REPO_BUILD_DIR).join(LOCK_NAME)
}

/// The lock of the downloads into a checkout, on its `build/.nvx-download.lock`. One download at
/// a time holds it exclusively while it searches for, downloads, and installs a release, and a
/// command that runs the artifacts of the checkout holds it shared while it decides whether to
/// download one, and then while it reads the artifacts and launches OpenVMM on them, as [`Hold`]
/// says. The operating system releases it when its holder exits, however it exits, and the lock
/// file stays.
///
/// [`Hold`]: super::Hold
pub(super) struct DownloadLock {
    /// The lock file, whose lock the holder holds until it drops the file.
    _file: fs::File,
}

impl DownloadLock {
    /// Takes the lock of the downloads into the checkout `dir` exclusively, creating its file,
    /// and waits while another download, or a command that reads the checkout, holds it, as
    /// [`Self::wait`] says.
    pub(super) fn acquire(dir: &ArtifactDir, report: Report) -> Result<Self> {
        let path = lock_path(dir);
        let file =
            create_lock_file(dir).map_err(|(error, at)| io_error(ErrorCode::Io, &error, &at))?;
        Self::wait(
            dir,
            &path,
            file,
            report,
            "another nvx command that uses the artifacts of",
            fs::File::try_lock,
        )
    }

    /// Takes the lock of the downloads into the checkout `dir` shared, which other commands that
    /// read the checkout share, creating its file as a download does, and waits while a download
    /// holds it, as [`Self::wait`] says. Returns `None` for a checkout that has no lock file and
    /// may not create one, such as a read-only one, into which no download installs either.
    pub(super) fn shared(dir: &ArtifactDir, report: Report) -> Result<Option<Self>> {
        let path = lock_path(dir);
        let file = match create_lock_file(dir) {
            Ok(file) => file,
            Err((error, _))
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
                ) =>
            {
                match fs::File::open(&path) {
                    Ok(file) => file,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) =>
                    {
                        return Ok(None);
                    }
                    Err(error) => return Err(io_error(ErrorCode::Io, &error, &path)),
                }
            }
            Err((error, at)) => return Err(io_error(ErrorCode::Io, &error, &at)),
        };
        Self::wait(
            dir,
            &path,
            file,
            report,
            "another download into",
            fs::File::try_lock_shared,
        )
        .map(Some)
    }

    /// Takes the lock of the lock `file` at `path` of the checkout `dir` with `try_lock`, waiting
    /// while another holder keeps it. A wait that outlasts a retry interval is reported once as
    /// `report` says, as waiting for `holder` the checkout: a command that only decides holds the
    /// lock for far less. Ctrl-C ends the wait.
    fn wait(
        dir: &ArtifactDir,
        path: &Path,
        file: fs::File,
        report: Report,
        holder: &str,
        try_lock: fn(&fs::File) -> std::result::Result<(), fs::TryLockError>,
    ) -> Result<Self> {
        let mut attempts = 0_u32;
        loop {
            match try_lock(&file) {
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {
                    if attempts == 1 {
                        report
                            .print(&format!(">> waiting for {holder} {}", dir.root().display()))?;
                    }
                    attempts = attempts.saturating_add(1);
                    check_interrupt()?;
                    std::thread::sleep(LOCK_RETRY_INTERVAL);
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(io_error(ErrorCode::Io, &error, path));
                }
            }
        }
    }
}

/// Opens the lock file of the downloads into the checkout `dir` for writing, creating it and the
/// checkout's `build/` if they do not exist, or returns the error and the path that it concerns.
fn create_lock_file(dir: &ArtifactDir) -> std::result::Result<fs::File, (io::Error, PathBuf)> {
    let build = dir.root().join(REPO_BUILD_DIR);
    fs::create_dir_all(&build).map_err(|error| (error, build.clone()))?;
    let path = lock_path(dir);
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| (error, path))
}

#[cfg(test)]
mod tests;
