//! ARM command refusals and diagnostics over the shared command implementation.

use super::*;

/// Refuses capture before parsing entropy paths or constructing capture state.
pub(super) fn cmd_capture(
    _: TimeAbi,
    _: &[&[u8]],
    _: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    Err(time_failure(err, TimeError::Unsupported("ARM capture")))
}

/// Refuses preparation before changing detector or capture state.
pub(super) fn cmd_pre_capture(_: TimeAbi, err: &mut dyn Write) -> Result<(), CommandError> {
    Err(time_failure(err, TimeError::Unsupported("ARM capture")))
}

/// Refuses cancellation because no ARM capture lifecycle is admitted.
pub(super) fn cmd_cancel_capture(_: TimeAbi, err: &mut dyn Write) -> Result<(), CommandError> {
    Err(time_failure(err, TimeError::Unsupported("ARM capture")))
}

/// Refuses restore completion before consuming a packet or acknowledging repair.
pub(super) fn cmd_restore_finish(
    _: TimeAbi,
    _: &[&[u8]],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    Err(time_failure(err, TimeError::Unsupported("ARM restore")))
}

/// Refuses capture/restore checks rather than publishing a successful unsupported phase.
pub(super) fn cmd_check(
    _: &Runtime<'_>,
    _: &[&[u8]],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    report(
        err,
        &[b"nvx-time: ARM capture/restore checks are unsupported\n"],
    );
    Err(CommandError::Operation)
}

#[cfg(test)]
mod tests;
