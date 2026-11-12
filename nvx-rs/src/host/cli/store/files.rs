//! Private, regular, non-link files, atomically replaced and synced under the state lock.

use std::fs;
use std::io;

use super::StateFile;
use super::locked::LockedState;
use crate::host::platform;
use crate::{Error, Result};

/// Bounds a fixed record's paths and identities without reading an unbounded corrupt file.
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

pub(super) fn read(state: &LockedState, name: StateFile) -> Result<Option<Vec<u8>>> {
    let path = state.path(name);
    let failed = |error| Error::io(format!("cannot read {}", path.display())).with_source(error);
    let file = match platform::open_regular(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failed(error)),
    };
    if let Some(problem) = platform::private_file_problem(&file).map_err(failed)? {
        return Err(Error::io(format!(
            "the sandbox file {} {problem}",
            path.display()
        )));
    }
    platform::read_at_most(file, MAX_RECORD_BYTES, "a sandbox record")
        .map(Some)
        .map_err(failed)
}

pub(super) fn replace(state: &LockedState, name: StateFile, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(Error::io("the sandbox record exceeds its size bound"));
    }
    // Refuse an existing unsafe entry rather than replacing it or following its target.
    read(state, name)?;
    platform::write_private(&state.path(name), bytes)?;
    platform::sync_dir(state.dir())
}

pub(super) fn remove(state: &LockedState, name: StateFile) -> Result<()> {
    read(state, name)?;
    let path = state.path(name);
    fs::remove_file(&path).map_err(|error| {
        Error::io(format!("cannot remove {}", path.display())).with_source(error)
    })?;
    platform::sync_dir(state.dir())
}
