//! Immutable named snapshots and their external, exclusive locks.
//!
//! Names map to hexadecimal directory names under `snapshots/`. Each published directory
//! contains only the strict snapshot's three entries; its lock stays under `locks/`.
//! Completed private directories under `staging/` publish with one non-replacing rename.

pub(super) mod capture;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::deadline::Deadline;
use super::store::{LockedState, StateDir};
use super::{Exit, Result, hex, io_error, temporary_path};
use crate::host::snapshot::Snapshot;
use crate::host::{platform, store};
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
        let lock = StateDir::create(&self.locks.dir().join(&directory))?.lock(deadline)?;
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

#[cfg(test)]
mod tests;
