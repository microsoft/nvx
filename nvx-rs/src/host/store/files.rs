//! The private files of the store: their creation, atomic replacement, reading, and removal, and
//! the syncs of their directories.
//!
//! The sandbox's store opens, creates, replaces, and reads its lock, its records, its capability,
//! and its log only as [`StoreFile`]s, each of which holds a file's path with what messages call
//! it, so that no read or lock of one of them skips the checks that refuse a file that others may
//! open.
//!
//! The template store, [`template::Store`](crate::host::template::Store), keeps its own records
//! with the same lock files, atomic replacements, removals, and syncs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

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
fn private_options() -> OpenOptions {
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
pub(crate) fn create_new(path: &Path) -> io::Result<File> {
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
            platform::restore_private_file_mode(
                path,
                nix::sys::stat::Mode::from_bits_truncate(PRIVATE_FILE_MODE),
            )?;
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

/// One file of a sandbox's store: its path, and what messages call it.
///
/// [`SandboxStore`](super::SandboxStore) builds one for each file that it creates and reads, its
/// lock, its launch journal, its runtime record, its capability, and its log, and opens, creates,
/// replaces, and reads those files only through it, so that each of them is created readable only
/// by the current user, replaced atomically, read only if it is a regular file that others may not
/// open, and locked only if others may not open it, and that every message about it calls it by
/// its description.
#[derive(Debug)]
pub(super) struct StoreFile {
    /// The file's path.
    path: PathBuf,
    /// What messages call the file, such as `sandbox runtime record`.
    description: &'static str,
}

impl StoreFile {
    /// Returns the file at `path`, which messages call `description`.
    pub(super) fn new(path: PathBuf, description: &'static str) -> Self {
        Self { path, description }
    }

    /// Returns the file's path.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Returns what messages call the file.
    pub(super) fn description(&self) -> &'static str {
        self.description
    }

    /// Creates the file, which must not exist, readable only by the current user, as
    /// [`create_new`] does.
    pub(super) fn create_new(&self) -> io::Result<File> {
        create_new(&self.path)
    }

    /// Opens the file as a lock, creating it readable only by the current user if it does not
    /// exist, as [`open_lock`] does, and refuses it if it is not private to the current user, since
    /// others could hold its lock.
    ///
    /// `failed` maps the failures to open the file and to check it, as the caller maps those of
    /// the lock that it then takes.
    pub(super) fn open_lock(&self, failed: impl Fn(io::Error) -> Error) -> Result<File> {
        let file = open_lock(&self.path).map_err(&failed)?;
        if let Some(problem) = platform::private_file_problem(&file).map_err(failed)? {
            return Err(self.not_private(&problem));
        }
        Ok(file)
    }

    /// Atomically replaces the file with `contents`, readable only by the current user, as
    /// [`write_private`] does.
    pub(super) fn replace(&self, contents: &[u8]) -> Result<()> {
        write_private(&self.path, contents)
    }

    /// Reads the file, or returns `None` if it does not exist.
    ///
    /// The open does not wait, as it would on Unix for a FIFO without a writer, and a file that is
    /// not a regular file is refused before anything is read from it, as a FIFO whose writer keeps
    /// it open would keep a read waiting. A file that is not private to the current user is
    /// refused, since others could have written it: on Windows, a file's own access control list
    /// decides who may open it, whatever the store's directory allows.
    pub(super) fn read(&self) -> Result<Option<Vec<u8>>> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            // Reads of a regular file do not depend on the flag.
            options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
        }
        let mut file = match options.open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            // A socket refuses an open.
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ENXIO as i32) => {
                return Err(self.read_failed()(not_regular()));
            }
            Err(error) => return Err(self.read_failed()(error)),
        };
        if !file
            .metadata()
            .map_err(self.read_failed())?
            .file_type()
            .is_file()
        {
            return Err(self.read_failed()(not_regular()));
        }
        if let Some(problem) = platform::private_file_problem(&file).map_err(self.read_failed())? {
            return Err(self.not_private(&problem));
        }
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .map_err(self.read_failed())?;
        Ok(Some(contents))
    }

    /// Removes the file if it exists, as [`remove_if_present`] does.
    pub(super) fn remove_if_present(&self) -> Result<()> {
        remove_if_present(&self.path)
    }

    /// Builds an error mapper that keeps the I/O source of a failure to read the file.
    pub(super) fn read_failed(&self) -> impl FnOnce(io::Error) -> Error {
        io_error(format!(
            "cannot read the {} {}",
            self.description,
            self.path.display()
        ))
    }

    /// Returns the error of the file, which is not private to the current user for the reason
    /// that `problem` gives.
    fn not_private(&self, problem: &str) -> Error {
        Error::io(format!(
            "the {} {} {problem}",
            self.description,
            self.path.display()
        ))
    }
}
