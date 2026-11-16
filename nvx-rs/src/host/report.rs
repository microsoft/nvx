//! OpenVMM's outcome report of a VM.
//!
//! OpenVMM writes a JSON report when a managed VM ends: the category and status code of its
//! outcome, and whether each step of its teardown completed. [`read`] summarizes it without its
//! free-form fields.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::{Error, Result};

/// Summary of an outcome report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Category of the VM's outcome, such as `success`.
    pub category: Option<String>,
    /// Status code of the VM's outcome.
    pub status_code: Option<i64>,
    /// Whether every step of the teardown completed, when the report lists the steps.
    pub teardown_complete: Option<bool>,
}

/// Reads the report at `path`, or returns `Ok(None)` if it is missing.
///
/// An unreadable or malformed report fails with [`ErrorKind::Io`](crate::ErrorKind::Io),
/// retaining the underlying filesystem or JSON error.
pub fn read(path: &Path) -> Result<Option<Report>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(
                Error::io(format!("cannot read outcome report {}", path.display()))
                    .with_source(error),
            );
        }
    };
    let report: Value = serde_json::from_slice(&bytes).map_err(|error| {
        Error::io(format!("cannot parse outcome report {}", path.display())).with_source(error)
    })?;
    let mut summary = Report::default();
    if let Some(outcome) = report.get("outcome") {
        summary.category = outcome
            .get("category")
            .and_then(Value::as_str)
            .map(str::to_owned);
        summary.status_code = outcome.get("status_code").and_then(Value::as_i64);
    }
    if let Some(teardown) = report.get("teardown").and_then(Value::as_object) {
        let complete = teardown.values().all(|step| step.as_bool() == Some(true));
        summary.teardown_complete = Some(complete);
    }
    Ok(Some(summary))
}

#[cfg(test)]
mod tests;
