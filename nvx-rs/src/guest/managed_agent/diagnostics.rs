use crate::guest::sys::{self, Errno};

use super::seam::System;

/// Reports a structured fatal or lifecycle error through the diagnostic port.
pub(crate) fn error(system: &mut impl System, stage: &str, status: i32) {
    let message = format!("NVX-MANAGED-ERROR: stage={stage} status={status}\n");
    let _ = system.write_diagnostic(message.as_bytes());
}

/// Reports a host-mapping failure through both diagnostic channels.
pub(crate) fn mapping_error(system: &mut impl System, status: i32, entry: u32) {
    error(system, "host-mapping", status);
    let message = format!("NVX-MANAGED-ERROR: stage=host-mapping status={status} entry={entry}\n");
    write_stderr(message.as_bytes());
}

/// Writes a diagnostic to the process's standard error, ignoring write failures.
pub(crate) fn write_stderr(message: &[u8]) {
    sys::write_standard_error(message);
}

/// Returns `prefix`, the description of `error`, and a newline.
pub(crate) fn with_description(prefix: &[u8], error: Errno) -> Vec<u8> {
    [prefix, error.desc().as_bytes(), b"\n"].concat()
}
