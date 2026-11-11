//! Immutable named snapshots and their external, exclusive locks.
//!
//! Names map to hexadecimal directory names under `snapshots/`. Each published directory
//! contains only the strict snapshot's three entries; its lock stays under `locks/`.
//! Completed private directories under `staging/` publish with one non-replacing rename.
//!
//! Each start of a restored sandbox links the snapshot's files into the sandbox's view of it
//! under the snapshot's lock, which [`LockedSnapshot::remove_view`] also takes to remove the
//! view.

pub(super) mod capture;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::deadline::Deadline;
use super::store::{LockedState, StateDir};
use super::{Exit, Result, hex, io_error, temporary_path};
use crate::host::snapshot::{OPENVMM_DIR, SNAPSHOT_ARTIFACTS, Snapshot};
use crate::host::vm::VmDir;
use crate::host::{filesystem, paths, platform, store};
use crate::{Error, ErrorCode};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct Name(String);

impl TryFrom<String> for Name {
    type Error = Error;

    fn try_from(value: String) -> crate::Result<Self> {
        if !(1..=64).contains(&value.len())
            || !value.as_bytes()[0].is_ascii_alphanumeric()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(Error::unsupported(
                "a snapshot name must be 1 to 64 ASCII letters, digits, dots, underscores or \
                 hyphens, starting with a letter or digit",
            ));
        }
        Ok(Self(value))
    }
}

#[derive(Debug)]
pub(crate) struct Store {
    root: StateDir,
    entries: StateDir,
    locks: StateDir,
    staging: StateDir,
}

impl Store {
    pub(crate) fn create(root: &Path) -> Result<Self> {
        let root = StateDir::create(&std::path::absolute(root).map_err(Exit::from_os_error)?)?;
        Ok(Self {
            entries: StateDir::create(&root.dir().join("snapshots"))?,
            locks: StateDir::create(&root.dir().join("locks"))?,
            staging: StateDir::create(&root.dir().join("staging"))?,
            root,
        })
    }

    /// Opens an existing store, private to the current user, as [`create`](Self::create) makes
    /// it, without creating anything.
    pub(crate) fn open(root: &Path) -> Result<Self> {
        let root = StateDir::open(&std::path::absolute(root).map_err(Exit::from_os_error)?)?;
        Ok(Self {
            entries: StateDir::open(&root.dir().join("snapshots"))?,
            locks: StateDir::open(&root.dir().join("locks"))?,
            staging: StateDir::open(&root.dir().join("staging"))?,
            root,
        })
    }

    fn check(&self) -> Result {
        for directory in [&self.root, &self.entries, &self.locks, &self.staging] {
            StateDir::open(directory.dir())?;
        }
        Ok(())
    }

    pub(crate) fn lock(&self, name: &Name, deadline: Deadline) -> Result<LockedSnapshot<'_>> {
        self.check()?;
        // Hex preserves case distinctions and avoids reserved host filenames.
        let directory = hex(name.0.as_bytes());
        let lock = StateDir::create(&self.locks.dir().join(&directory))?
            .lock(deadline)
            .map_err(|error| match error.code() {
                Some(ErrorCode::SandboxBusy) => Exit::error(
                    ErrorCode::SandboxBusy,
                    format!(
                        "another command still holds the lock of the snapshot {}",
                        name.0
                    ),
                ),
                _ => error,
            })?;
        Ok(LockedSnapshot {
            store: self,
            dir: self.entries.dir().join(directory),
            _lock: lock,
        })
    }
}

#[derive(Debug)]
pub(crate) struct LockedSnapshot<'a> {
    store: &'a Store,
    dir: PathBuf,
    _lock: LockedState,
}

impl LockedSnapshot<'_> {
    pub(crate) fn stage(&self) -> Result<StateDir> {
        self.store.check()?;
        StateDir::create(&temporary_path(&self.store.staging.dir().join("snapshot"))?)
    }

    pub(crate) fn open(&self) -> Result<Snapshot> {
        self.store.check()?;
        Snapshot::open(&self.dir).map_err(Into::into)
    }

    /// Removes the view of this snapshot that a restore created in `vm`, and on Linux the
    /// restore-ready socket of that restore, which this lock must cover, since removing the view
    /// changes the links of the snapshot's files. Refuses a view that is a link or holds anything
    /// else, which it leaves, and anything else in place of the socket, and, before it removes
    /// anything, a VM directory in the store, whose view could be a snapshot's own files.
    pub(crate) fn remove_view(&self, vm: &VmDir) -> Result {
        outside_store(self.store.root.dir(), vm.path())?;
        let foreign = |path: &Path| {
            Exit::error(
                ErrorCode::ForeignFiles,
                format!("{} is not a restore's own", path.display()),
            )
        };
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::FileTypeExt;

            let ready = PathBuf::from(vm.restore_ready_endpoint()?);
            match fs::symlink_metadata(&ready) {
                Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&ready)
                    .map_err(|error| io_error(ErrorCode::Io, &error, &ready))?,
                Ok(_) => return Err(foreign(&ready)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(ErrorCode::Io, &error, &ready)),
            }
        }
        let view = vm.path().join(OPENVMM_DIR);
        match fs::symlink_metadata(&view) {
            Ok(metadata) if metadata.is_dir() && !filesystem::is_reparse_point(&metadata) => {}
            Ok(_) => return Err(foreign(&view)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(ErrorCode::Io, &error, &view)),
        }
        for name in SNAPSHOT_ARTIFACTS {
            let path = view.join(name);
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(io_error(ErrorCode::Io, &error, &path));
                }
                _ => {}
            }
        }
        match fs::remove_dir(&view) {
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Err(foreign(&view)),
            removed => removed.map_err(|error| io_error(ErrorCode::Io, &error, &view)),
        }
    }

    pub(crate) fn publish(&self, staged: StateDir) -> Result<Snapshot> {
        self.store.check()?;
        if staged.dir().parent() != Some(self.store.staging.dir()) {
            return Err(Exit::error(
                ErrorCode::Io,
                "only a private staging directory can publish a named snapshot",
            ));
        }
        Snapshot::open(staged.dir())?;
        store::files::sync_dir(staged.dir())?;
        platform::rename_no_replace(staged.dir(), &self.dir)
            .map_err(|error| io_error(ErrorCode::Io, &error, &self.dir))?;
        store::files::sync_dir(self.store.entries.dir())?;
        store::files::sync_dir(self.store.staging.dir())?;
        self.open()
    }
}

/// Refuses `dir` in the snapshot store at `root`, once the links of both are resolved: the view
/// of a restore in that directory could take the files of the store's snapshots for its own.
fn outside_store(root: &Path, dir: &Path) -> Result {
    let resolve = |path: &Path| paths::resolve(path).map_err(Exit::from_os_error);
    if resolve(dir)?.starts_with(resolve(root)?) {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "the sandbox's state directory must be outside the snapshot store",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
