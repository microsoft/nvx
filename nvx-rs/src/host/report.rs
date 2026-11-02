//! OpenVMM's outcome report of a VM.
//!
//! OpenVMM writes a JSON report when a managed VM ends: the category and status code of its
//! outcome, and whether each step of its teardown completed. [`read`] summarizes it without its
//! free-form fields.

use std::fs;
use std::path::Path;

use serde_json::Value;

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

/// Reads the report at `path`, or returns `None` if it is missing or not JSON.
pub fn read(path: &Path) -> Option<Report> {
    let report: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
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
    Some(summary)
}

#[cfg(test)]
mod tests;
