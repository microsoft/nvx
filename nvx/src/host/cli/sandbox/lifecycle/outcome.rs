//! Publication and reading of managed-workload and OpenVMM outcome reports.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::OUTCOME_SCHEMA_VERSION;
use super::directory::os_error;
use crate::ErrorCode;
use crate::hex::hex;
use crate::host::cli::json::{Object, pretty, read_object};
use crate::host::cli::session::{EXIT_CATEGORIES, ExecResult};
use crate::host::cli::{Exit, Result, random_bytes, temporary_path};
use crate::host::paths;
#[cfg(unix)]
use crate::host::platform;

/// Returns the destination of an outcome report, as `_outcome_destination` does: in a plain
/// directory, with a file name, and new. On Windows, the file name holds no colon, with which
/// NTFS names a stream of another file rather than a new file, and to which no report can be
/// linked.
fn outcome_destination(path: &str) -> Result<PathBuf> {
    let candidate =
        paths::normalized(&paths::absolute(Path::new(path)).map_err(Exit::from_os_error)?);
    let parent = candidate.parent().unwrap_or(&candidate);
    if parent.is_symlink() || !parent.is_dir() {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            format!(
                "outcome report parent is not a plain directory: {}",
                parent.display()
            ),
        ));
    }
    let Some(name) = candidate.file_name() else {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "outcome report path has no filename",
        ));
    };
    if cfg!(windows) && name.to_string_lossy().contains(':') {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            format!(
                "outcome report file name holds a colon, which names a stream of another file \
                 on Windows rather than a new file: {}",
                candidate.display()
            ),
        ));
    }
    let resolved = paths::resolve(parent)
        .map_err(Exit::from_os_error)?
        .join(name);
    match std::fs::symlink_metadata(&resolved) {
        Ok(_) => {
            return Err(Exit::error(
                ErrorCode::OutcomeReportExists,
                format!("outcome report already exists: {}", resolved.display()),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(os_error(&error, &resolved)),
    }
    Ok(resolved)
}

/// Checks the destination of an outcome report before a workload runs, as
/// `validate_outcome_destination` does.
pub(crate) fn validate_outcome_destination(path: &str) -> Result {
    outcome_destination(path).map(drop)
}

/// Writes the JSON `text` to a new file at `path`: through a temporary file of mode 0600, synced,
/// then linked to its name, which must not exist, with `\n` line ends on every host.
fn write_new_json(path: &str, text: &str) -> Result {
    let text = format!("{text}\n");
    write_new_file(path, |file| file.write_all(text.as_bytes()))
}

/// Publishes an OpenVMM report's exact bytes without replacing an existing destination.
pub(crate) fn copy_openvmm_outcome(source: &Path, path: &str) -> Result {
    let mut report = File::open(source).map_err(|error| os_error(&error, source))?;
    let permissions = report
        .metadata()
        .map_err(|error| os_error(&error, source))?
        .permissions();
    write_new_file(path, |file| {
        io::copy(&mut report, file)?;
        file.set_permissions(permissions)
    })
}

/// The name that an outcome report's staging file extends, to `.outcome-report.UUID.tmp`, in
/// place of the report's own name, which may be as long as its file system accepts and so leave
/// no room for the staging file's affixes.
const STAGING_NAME: &str = "outcome-report";

/// Writes a new file at `path` with `write`: in a staging file of its directory, synced, then
/// linked to its name, which must not exist.
fn write_new_file(path: &str, write: impl FnOnce(&mut File) -> io::Result<()>) -> Result {
    let resolved = outcome_destination(path)?;
    let temporary = temporary_path(&resolved.with_file_name(STAGING_NAME))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, platform::PRIVATE_FILE_MODE);
    let mut file = match options.open(&temporary) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Exit::error(
                ErrorCode::Io,
                "failed to reserve an outcome report staging file",
            ));
        }
        Err(error) => return Err(os_error(&error, &temporary)),
    };
    let written = (|| -> Result {
        write(&mut file)
            .and_then(|()| file.sync_all())
            .map_err(Exit::from_os_error)?;
        match std::fs::hard_link(&temporary, &resolved) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(Exit::error(
                ErrorCode::OutcomeReportExists,
                format!("outcome report already exists: {}", resolved.display()),
            )),
            Err(error) => Err(os_error(&error, &resolved)),
        }
    })();
    drop(file);
    let _ = std::fs::remove_file(&temporary);
    written
}

/// The sections of an OpenVMM outcome report that a reader requires.
const OUTCOME_SECTIONS: [&str; 3] = ["outcome", "network_policy", "teardown"];

/// Reads an OpenVMM outcome report, as [`read_object`] reads a JSON file: of schema version 1,
/// with `outcome`, `network_policy`, and `teardown` objects.
pub(super) fn read_openvmm_outcome(path: &Path) -> Result<Object> {
    let report = read_object(
        path,
        "OpenVMM outcome report",
        "schema_version",
        &[OUTCOME_SCHEMA_VERSION],
    )?;
    for name in OUTCOME_SECTIONS {
        if !report.get(name).is_some_and(Value::is_object) {
            return Err(Exit::error(
                ErrorCode::Io,
                format!(
                    "OpenVMM outcome report has an invalid {name} section: {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(report)
}

/// The size of an outcome report's operation ID, in random bytes: 128 bits, written as 32
/// hexadecimal digits, so that IDs drawn independently, without coordination, do not collide.
const OPERATION_ID_BYTES: usize = 16;

/// Writes the outcome report of a managed workload: its category and status, with a random
/// operation ID, and nothing of its output.
pub(crate) fn write_exec_outcome(path: &str, result: &ExecResult) -> Result {
    if !EXIT_CATEGORIES.contains(&result.category) {
        return Err(Exit::error(
            ErrorCode::Protocol,
            "managed workload returned an unsupported outcome category",
        ));
    }
    let operation_id = hex(&random_bytes::<OPERATION_ID_BYTES>()?);
    let report = json!({
        "schema_version": OUTCOME_SCHEMA_VERSION,
        "operation_id": operation_id,
        "outcome": {
            "operation": "exec",
            "category": result.category,
            "status_code": result.returncode,
        },
    });
    write_new_json(path, &pretty(&report))
}
