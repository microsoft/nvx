//! The private files of the store: their creation, atomic replacement, reading, and removal, and
//! the syncs of their directories.
//!
//! The template store, [`template::Store`](crate::host::template::Store), keeps its own records
//! with the same lock files, atomic replacements, removals, and syncs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::error::{Error, Result};
use crate::host::{filesystem, platform};

/// Builds an error mapper that keeps an I/O source under `context`.
pub(super) fn io_error(context: String) -> impl FnOnce(io::Error) -> Error {
    move |error| Error::io(context).with_source(error)
}

/// Returns the error of a lock, a record, or a capability that is not a regular file.
fn not_regular() -> io::Error {
    io::Error::other("is not a regular file")
}

/// Mode of the store's files on Unix: readable and writable by the current user only.
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Returns options that create a file readable only by the current user (mode 0600 on Unix)
/// and that open no link that its path names on Unix.
pub(super) fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    options
}

/// Creates the file at `path`, which must not exist, readable only by the current user.
///
/// On Unix, the umask may clear bits of the mode that the file is created with, even the
/// current user's permission to read it, so the mode is set again through the new file.
pub(super) fn create_new(path: &Path) -> io::Result<File> {
    let file = private_options().create_new(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    }
    Ok(file)
}

/// Opens the lock file at `path`, creating it readable only by the current user if it does not
/// exist.
///
/// A lock file that is not a regular file is refused, and so is a link, which is not followed, or
/// on Windows any other reparse point: a lock taken through a link would hold the file that the
/// link names, which a launch may replace while the lock is held, so that the next transition
/// would lock the replacement.
///
/// On Unix, a lock and the checks of the locked file need no read access, so the file is opened
/// only for writing. The open does not wait, as it would for a FIFO without a reader. The umask
/// may have cleared even the current user's permission to write it, whether this or another call
/// created it, so a lock file of the current user that the user can open for writing no more is
/// given its mode again.
pub(crate) fn open_lock(path: &Path) -> io::Result<File> {
    let open = || {
        let mut options = private_options();
        options.read(cfg!(windows)).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            // These flags replace those of `private_options`.
            options.custom_flags(
                (nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_NONBLOCK).bits(),
            );
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
            platform::restore_private_file_mode(path, PRIVATE_FILE_MODE)?;
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
            return Err(not_regular());
        }
    }
    let file = opened?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || filesystem::is_reparse_point(&metadata) {
        return Err(not_regular());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & PRIVATE_FILE_MODE != PRIVATE_FILE_MODE {
            file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        }
    }
    Ok(file)
}

/// Atomically replaces `path` with `contents`, readable only by the current user.
///
/// The contents go to a fresh temporary file beside `path`, which is flushed to disk and then
/// renamed over it, so a reader sees the old contents or the new ones, and a crash leaves at most
/// the temporary file.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix)
        .map_err(|error| Error::other("cannot name a temporary file").with_source(error))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", u64::from_le_bytes(suffix)));
    let written = (|| -> io::Result<()> {
        let mut file = create_new(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written.map_err(io_error(format!("cannot write {}", path.display())))
}

/// Reads the file at `path`, the store's `description`, or returns `None` if it does not exist.
///
/// The open does not wait, as it would on Unix for a FIFO without a writer, and a file that is
/// not a regular file is refused before anything is read from it, as a FIFO whose writer keeps
/// it open would keep a read waiting. A file that is not private to the current user is refused,
/// since others could have written it: on Windows, a file's own access control list decides who
/// may open it, whatever the store's directory allows.
pub(super) fn read_private(path: &Path, description: &str) -> Result<Option<Vec<u8>>> {
    let failed = || io_error(format!("cannot read the {description} {}", path.display()));
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // Reads of a regular file do not depend on the flag.
        options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        // A socket refuses an open.
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ENXIO as i32) => {
            return Err(failed()(not_regular()));
        }
        Err(error) => return Err(failed()(error)),
    };
    if !file.metadata().map_err(failed())?.file_type().is_file() {
        return Err(failed()(not_regular()));
    }
    if let Some(problem) = platform::private_file_problem(&file).map_err(failed())? {
        return Err(Error::io(format!(
            "the {description} {} {problem}",
            path.display()
        )));
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents).map_err(failed())?;
    Ok(Some(contents))
}

/// Makes the changes to the entries of the directory `dir` so far durable, such as files that
/// were renamed into it or removed from it: a crash that keeps a later change keeps them too, even
/// one on another filesystem.
///
/// On Unix, it syncs the directory. Windows has no portable way to flush a directory, so there a
/// rename or a removal is as durable as the filesystem makes it, as OpenVMM's publication of a
/// snapshot assumes.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|directory| directory.sync_all())
            .map_err(io_error(format!(
                "cannot sync the directory {}",
                dir.display()
            )))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Removes the file at `path` if it exists.
pub(crate) fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(io_error(format!("cannot remove {}", path.display()))(error))
        }
        _ => Ok(()),
    }
}
