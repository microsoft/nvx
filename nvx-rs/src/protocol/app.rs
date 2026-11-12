//! Application frames (magic `NVXC`), which outer data records carry between the host client and
//! the guest agent.
//!
//! A frame is a 24-byte header (the magic, the version, the frame kind, two zero flags bytes, the
//! request ID, a status, and the payload length) followed by the payload. Responses repeat the ID
//! of their request. `v2` holds version 2, NVXC's only version, which carries every control path
//! and streams an execution's standard input, output and error. This module holds the codes of
//! its kinds and its error categories, with which NVX's guest agents and the fake OpenVMM serve
//! it, and the payloads of the capture and activate requests.

#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
use super::array;
#[cfg(any(
    feature = "cli",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
use super::{ProtocolError, violation};
#[cfg(any(
    feature = "cli",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
use crate::activation::ActivationRecord;

#[cfg(any(
    feature = "cli",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
use self::workload::WorkloadIdentity;

/// Shared identity data.
#[cfg(any(
    feature = "cli",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) mod workload {
    /// The identity that workloads run as: a non-root user and group ID.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct WorkloadIdentity {
        /// User ID.
        pub(crate) uid: u32,
        /// Primary group ID.
        pub(crate) gid: u32,
        /// Whether the guest creates an account for the IDs if its image lacks a usable one,
        /// replacing an image account for the UID whose primary group or home does not fit.
        pub(crate) create_account: bool,
    }
}

#[cfg(any(
    feature = "cli",
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) mod v2;

/// Length of an application frame header.
#[cfg(any(
    feature = "cli",
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_HEADER_LEN: usize = 24;

/// Request that the guest agent acknowledge readiness with [`APP_READY`].
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_PING: u8 = 1;
/// Request that the guest agent run a workload; the payload is an `EXEC` payload.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_EXEC: u8 = 2;
/// Request that the guest agent shut the VM down, which it acknowledges with [`APP_STOPPED`].
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_STOP: u8 = 3;
/// Request that the guest agent terminate the workload of the request with the frame's ID.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_CANCEL: u8 = 4;
/// Request that the guest agent mount entries of the host mapping table; the payload is a `MAPS`
/// payload.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_MAPS: u8 = 6;
/// Request that nvx-init capture the VM after optionally killing its executions.
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
pub(crate) const APP_CAPTURE: u8 = 7;
/// Request that nvx-init activate a restored VM with its record and workload identity.
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
pub(crate) const APP_ACTIVATE: u8 = 8;
/// Response that acknowledges a request.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_READY: u8 = 0x81;
/// Response that carries a chunk of a workload's standard output.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_STDOUT: u8 = 0x82;
/// Response that carries a chunk of a workload's standard error.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_STDERR: u8 = 0x83;
/// Response that ends a workload: the status is its exit status, and the payload how it ended,
/// as [`v2::Outcome`] names it.
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
pub(crate) const APP_EXIT: u8 = 0x84;
/// Response that acknowledges [`APP_STOP`].
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_STOPPED: u8 = 0x85;
/// Response that refuses a request: the status is an error number, and the payload an error
/// category such as [`LAUNCH_FAILED`].
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const APP_ERROR: u8 = 0xff;

/// Error category with which a guest agent refuses a request kind that it does not know.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const UNSUPPORTED_OPERATION: &[u8] = b"unsupported-operation";
/// Error category with which a guest agent refuses a malformed request.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const INVALID_REQUEST: &str = "invalid-request";
/// Error category with which a guest agent refuses a request while a workload runs.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const BUSY: &str = "busy";
/// Error category with which a guest agent reports a workload that it could not launch.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const LAUNCH_FAILED: &str = "launch-failed";
/// Error category with which a guest agent reports a working directory that the workload cannot
/// enter. The status is the guest's error number, and nothing of the workload ran.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const CWD_FAILED: &str = "cwd-failed";
/// Error category with which a guest agent refuses a workload until it has applied every host
/// mapping that the kernel command line announced.
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const MAPPINGS_INCOMPLETE: &str = "mappings-incomplete";
/// Error category with which a guest agent reports a host mapping that it could not apply. The
/// status is the guest's error number.
#[cfg(any(
    feature = "cli",
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) const MAPPING_FAILED: &str = "mapping-failed";

/// Decoded application frame.
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AppFrame<'a> {
    /// One of the `APP_*` frame kinds.
    pub(crate) kind: u8,
    /// ID of the request, which its responses repeat.
    pub(crate) request_id: u64,
    /// Status of a response; zero in requests.
    pub(crate) status: i32,
    /// Payload that follows the header.
    pub(crate) payload: &'a [u8],
}

/// Encodes the capture request's flag as one byte, 0 or 1.
#[cfg(feature = "cli")]
pub(crate) fn encode_capture_payload(kill_executions: bool) -> [u8; 1] {
    [u8::from(kill_executions)]
}

/// Decodes the capture request's sole kill-executions flag.
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn decode_capture_payload(payload: &[u8]) -> Result<bool, ProtocolError> {
    match payload {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(violation(
            "the capture request must contain one kill-executions flag",
        )),
    }
}

/// Encodes the persistent workload identity, then the existing activation record.
///
/// The prefix is the UID and GID as little-endian `u32`s, then the account-creation flag as
/// exactly 0 or 1. The remainder is the activation record's unchanged encoding.
#[cfg(feature = "cli")]
pub(crate) fn encode_activate_payload(
    record: &ActivationRecord,
    workload: &WorkloadIdentity,
) -> Result<Vec<u8>, ProtocolError> {
    check_workload_identity(workload)?;
    let record = record.encode();
    let mut payload = Vec::with_capacity(9 + record.len());
    payload.extend_from_slice(&workload.uid.to_le_bytes());
    payload.extend_from_slice(&workload.gid.to_le_bytes());
    payload.push(u8::from(workload.create_account));
    payload.extend_from_slice(&record);
    Ok(payload)
}

/// Decodes the persistent workload identity and the activation record that follows it.
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn decode_activate_payload(
    payload: &[u8],
) -> Result<(ActivationRecord, WorkloadIdentity), ProtocolError> {
    let prefix = payload
        .get(..9)
        .ok_or_else(|| violation("the activate request is truncated"))?;
    let workload = WorkloadIdentity {
        uid: u32::from_le_bytes(array(&prefix[..4])),
        gid: u32::from_le_bytes(array(&prefix[4..8])),
        create_account: match prefix[8] {
            0 => false,
            1 => true,
            _ => {
                return Err(violation(
                    "the activate request has an invalid account flag",
                ));
            }
        },
    };
    check_workload_identity(&workload)?;
    let record = ActivationRecord::decode(&payload[9..])
        .map_err(|error| violation(format!("invalid activation record: {error}")))?;
    Ok((record, workload))
}

/// Matches the guest's non-root credential range, excluding the unchanged-ID sentinel.
#[cfg(any(
    feature = "cli",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
fn check_workload_identity(workload: &WorkloadIdentity) -> Result<(), ProtocolError> {
    if matches!(workload.uid, 0 | u32::MAX) || matches!(workload.gid, 0 | u32::MAX) {
        return Err(violation(
            "activation workload requires a non-root UID and GID below u32::MAX",
        ));
    }
    Ok(())
}
