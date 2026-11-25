//! Creation and resolution of a sandbox's private state directory.

use std::path::{Path, PathBuf};

use crate::ErrorCode;
use crate::host::cli::sandbox::error_with_cause;
use crate::host::cli::store::StateDir;
use crate::host::cli::{Exit, Result, io_error};
use crate::host::paths;
use crate::host::vm_directory::check_sockets;

/// Returns the error for an I/O error that an operation on `path` met.
pub(super) fn os_error(error: &std::io::Error, path: &Path) -> Exit {
    io_error(ErrorCode::Io, error, path)
}

/// Returns a builder of directories of `mode`, which Windows ignores.
fn dir_builder(mode: u32) -> std::fs::DirBuilder {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, mode);
    #[cfg(not(unix))]
    let _ = mode;
    builder
}

/// Creates `path` and its missing parents: parents with the default mode, `path` with `mode`, and
/// no error if `path` is a directory already.
fn make_directories(path: &Path, mode: u32) -> Result {
    match dir_builder(mode).create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match parent(path) {
            Some(parent) => {
                make_directories(parent, 0o777)?;
                make_directories_once(path, mode)
            }
            None => Err(os_error(&error, path)),
        },
        Err(_) if path.is_dir() => Ok(()),
        Err(error) => Err(os_error(&error, path)),
    }
}

/// Creates `path` once its parent exists, with no error if it is a directory already.
fn make_directories_once(path: &Path, mode: u32) -> Result {
    match dir_builder(mode).create(path) {
        Ok(()) => Ok(()),
        Err(_) if path.is_dir() => Ok(()),
        Err(error) => Err(os_error(&error, path)),
    }
}

/// Returns the parent of `path`, or `None` for a root, or for a path of one relative component.
fn parent(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

/// Creates the state directory at `path`, as provision does. An existing directory stays as it is,
/// and the CLI's store then refuses one that is not private to the current user; any other
/// existing path fails as creating a directory fails for a path that exists. A new directory is
/// created as [`StateDir::create`] creates it, private to the current user, below parents that
/// are created as needed, even when an existing one goes meanwhile: nothing else creates the
/// directory itself.
pub(crate) fn create_state_directory(path: &Path) -> Result {
    if path.is_dir() {
        return Ok(());
    }
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(os_error(&already_exists(), path));
    }
    if let Some(parent) = parent(path) {
        make_directories(parent, 0o777)?;
    }
    StateDir::create(path).map(drop)
}

/// Returns the error with which creating a directory fails for a path that exists.
/// Windows reports `ERROR_ALREADY_EXISTS` for this case.
fn already_exists() -> std::io::Error {
    #[cfg(windows)]
    let error = std::io::Error::from_raw_os_error(
        windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS as i32,
    );
    #[cfg(unix)]
    let error = std::io::Error::from_raw_os_error(nix::errno::Errno::EEXIST as i32);
    #[cfg(not(any(windows, unix)))]
    let error = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    error
}

/// Resolves the state directory at `path`, as `_prepare_state_directory` does. It must be a
/// directory and not a symbolic link.
pub(crate) fn prepare_state_directory(path: &str) -> Result<PathBuf> {
    plain_directory(paths::resolve(Path::new(path)).map_err(Exit::from_os_error)?)
}

/// Resolves the state directory at `path` and creates it, as provision does, once it has checked
/// that OpenVMM can take the sockets that each start of its sandbox gives it there, as `start`
/// checks them: the control socket, and the restore-ready socket of a sandbox that `restores` its
/// snapshot, as one that `snapshot restore` provisions does. The check and the creation take one
/// resolution of `path`, so that the directory that it creates is the one that it checked. It
/// must then be a directory and not a symbolic link.
pub(crate) fn provision_state_directory(path: &str, restores: bool) -> Result<PathBuf> {
    let resolved = paths::resolve(Path::new(path)).map_err(Exit::from_os_error)?;
    check_sockets(&resolved, false, restores).map_err(error_with_cause)?;
    create_state_directory(&resolved)?;
    plain_directory(resolved)
}

/// Returns `resolved`, the resolved path of a state directory, if it names a directory and not a
/// symbolic link.
fn plain_directory(resolved: PathBuf) -> Result<PathBuf> {
    if resolved.is_symlink() || !resolved.is_dir() {
        return Err(Exit::error(
            ErrorCode::SandboxNotProvisioned,
            format!(
                "sandbox state path is not a plain directory: {}",
                resolved.display()
            ),
        ));
    }
    Ok(resolved)
}
