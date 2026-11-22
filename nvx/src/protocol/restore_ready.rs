//! OpenVMM's one restore-ready event, including its terminating newline.

use super::{ProtocolError, violation};

/// OpenVMM's exact version-1 restore-ready event, including its required terminating newline.
pub(crate) const READY_EVENT: &[u8] = b"OPENVMM_RESTORE_READY_V1\n";

/// Accepts exactly `OPENVMM_RESTORE_READY_V1\n`, with no missing or trailing bytes.
pub fn parse_ready_event(bytes: &[u8]) -> Result<(), ProtocolError> {
    if bytes == READY_EVENT {
        Ok(())
    } else {
        Err(violation("not OpenVMM's restore-ready event"))
    }
}

#[cfg(test)]
mod tests;
