//! OpenVMM's one restore-ready event, including its terminating newline.

use super::{ProtocolError, violation};

/// Accepts exactly `OPENVMM_RESTORE_READY_V1\n`, with no missing or trailing bytes.
pub fn parse_ready_event(bytes: &[u8]) -> Result<(), ProtocolError> {
    if bytes == b"OPENVMM_RESTORE_READY_V1\n" {
        Ok(())
    } else {
        Err(violation("not OpenVMM's restore-ready event"))
    }
}

#[cfg(test)]
mod tests;
