//! Publication and reading of managed-workload and OpenVMM outcome reports.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::OUTCOME_SCHEMA_VERSION;
use super::directory::os_error;
use crate::ErrorCode;
use crate::host::cli::json::{Object, pretty, read_object};
use crate::host::cli::sandbox::state::{hex, random_bytes, temporary_path};
use crate::host::cli::session::{EXIT_CATEGORIES, ExecResult};
use crate::host::cli::{Exit, Result};
use crate::host::paths;

/// Returns the destination of an outcome report, as `_outcome_destination` does: in a plain
/// directory, with a file name, and new.
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
    let resolved = paths::resolve(parent)
        .map_err(Exit::from_os_error)?
        .join(name);
    if std::fs::symlink_metadata(&resolved).is_ok() {
        return Err(Exit::error(
            ErrorCode::OutcomeReportExists,
            format!("outcome report already exists: {}", resolved.display()),
        ));
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
    let resolved = outcome_destination(path)?;
    let temporary = temporary_path(&resolved)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
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
        let text = format!("{text}\n");
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(Exit::from_os_error)?;
        match std::fs::hard_link(&temporary, &resolved) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(Exit::error(
                ErrorCode::OutcomeReportExists,
                format!("outcome report already exists: {}", resolved.display()),
            )),
            Err(_) => Err(Exit::error(
                ErrorCode::Io,
                format!("failed to publish outcome report: {}", resolved.display()),
            )),
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

/// Writes the outcome report of a managed workload: its category and status, with a random
/// operation ID, and nothing of its output.
pub(crate) fn write_exec_outcome(path: &str, result: &ExecResult) -> Result {
    if !EXIT_CATEGORIES.contains(&result.category) {
        return Err(Exit::error(
            ErrorCode::Protocol,
            "managed workload returned an unsupported outcome category",
        ));
    }
    let operation_id = hex(&random_bytes::<16>()?);
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
