//! Private files of an instance's caller and the temporary files of their replacements.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

/// Builds an error mapper that preserves an I/O source under `context`.
pub(super) fn io_error(context: String) -> impl FnOnce(io::Error) -> Error {
    move |error| Error::io(context).with_source(error)
}

/// Returns create-and-write options that request owner-only permissions on Unix.
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Returns the name of the temporary file through which [`write_private`] replaces the file `name`
/// for the random `number`: `.NAME.NUMBER.tmp`, which stays beside it if the replacement is
/// interrupted.
pub(super) fn temporary_name(name: &str, number: u64) -> String {
    format!(".{name}.{number}.tmp")
}

/// Atomically replaces `path` with `contents`, readable only by the current user, as callers do
/// with the configuration records that they keep beside an instance's store.
///
/// The contents go to a temporary file beside `path` first, which a replacement that is
/// interrupted leaves, and which [`remove_interrupted_writes`] removes.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix)
        .map_err(|error| Error::other("cannot name a temporary file").with_source(error))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(temporary_name(&name, u64::from_le_bytes(suffix)));
    let written = (|| -> io::Result<()> {
        let mut file = private_options().create_new(true).open(&temporary)?;
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

/// Atomically replaces `path` with `value` as pretty JSON and a newline, readable only by the
/// current user.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(value)
        .map_err(|error| Error::other("cannot encode sandbox state").with_source(error))?;
    text.push(b'\n');
    write_private(path, &text)
}

/// Reads the JSON at `path`, or returns `None` if the file does not exist.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match fs::read(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(format!("cannot read {}", path.display()))(error)),
    };
    serde_json::from_slice(&text).map(Some).map_err(|error| {
        Error::io(format!("sandbox state {} is malformed", path.display())).with_source(error)
    })
}

/// Removes the file at `path` if it exists.
pub fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(io_error(format!("cannot remove {}", path.display()))(error))
        }
        _ => Ok(()),
    }
}

/// Removes the temporary files that interrupted replacements of `path` with [`write_private`] or
/// [`write_json`] left beside it, and keeps every other file. A directory that does not exist has
/// none.
///
/// The store's removal refuses those files, which are neither the store's nor the files that its
/// caller declares, so a caller that writes its files with [`write_private`] removes them first,
/// under the instance's lock.
pub fn remove_interrupted_writes(path: &Path) -> Result<()> {
    let Some(name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return Ok(());
    };
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let unlisted = || io_error(format!("cannot list {}", dir.display()));
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(unlisted()(error)),
    };
    for entry in entries {
        let entry = entry.map_err(unlisted())?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|entry| is_temporary_of(entry, &name))
        {
            remove_if_present(&entry.path())?;
        }
    }
    Ok(())
}

/// Returns whether `entry` is the name of a temporary file through which [`write_private`]
/// replaces the file `name`.
fn is_temporary_of(entry: &str, name: &str) -> bool {
    entry
        .strip_suffix(".tmp")
        .and_then(|rest| rest.rsplit_once('.'))
        .and_then(|(_, number)| number.parse::<u64>().ok())
        .is_some_and(|number| temporary_name(name, number) == entry)
}
