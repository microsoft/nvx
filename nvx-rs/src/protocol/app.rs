//! Application frames (magic `NVXC`), which outer data records carry between the host client and
//! the guest agent.
//!
//! A frame is a 24-byte header (the magic, the version, the frame kind, two zero flags bytes, the
//! request ID, a status, and the payload length) followed by the payload. Responses repeat the ID
//! of their request.

use super::{ProtocolError, array, violation};

/// Magic of every application frame.
pub const APP_MAGIC: [u8; 4] = *b"NVXC";
/// Version of the application frame format.
pub const APP_VERSION: u8 = 1;
/// Length of an application frame header.
pub const APP_HEADER_LEN: usize = 24;

/// Request that the guest agent acknowledge readiness with [`APP_READY`].
pub const APP_PING: u8 = 1;
/// Request that the guest agent run a workload; the payload is an `EXEC` payload.
pub const APP_EXEC: u8 = 2;
/// Request that the guest agent shut the VM down, which it acknowledges with [`APP_STOPPED`].
pub const APP_STOP: u8 = 3;
/// Request that the guest agent terminate the workload of the request with the frame's ID.
pub const APP_CANCEL: u8 = 4;
/// Request for the guest's control features, which the guest agent answers with [`APP_READY`]
/// and a [`GuestFeatures`](super::GuestFeatures) payload.
pub const APP_FEATURES: u8 = 5;
/// Request that the guest agent mount entries of the host mapping table; the payload is a `MAPS`
/// payload.
pub const APP_MAPS: u8 = 6;
/// Request that nvx-init capture the VM after optionally killing its executions.
#[cfg_attr(
    not(any(test, feature = "host")),
    expect(dead_code, reason = "SL7.7a makes nvx-init handle capture requests")
)]
pub(crate) const APP_CAPTURE: u8 = 7;
/// Response that acknowledges a request.
pub const APP_READY: u8 = 0x81;
/// Response that carries a chunk of a workload's standard output.
pub const APP_STDOUT: u8 = 0x82;
/// Response that carries a chunk of a workload's standard error.
pub const APP_STDERR: u8 = 0x83;
/// Response that ends a workload: the status is its exit status, and the payload its
/// [`ExitCategory`].
pub const APP_EXIT: u8 = 0x84;
/// Response that acknowledges [`APP_STOP`].
pub const APP_STOPPED: u8 = 0x85;
/// Response that refuses a request: the status is an error number, and the payload an error
/// category such as [`LAUNCH_FAILED`].
pub const APP_ERROR: u8 = 0xff;

/// Error category with which a guest agent refuses a request kind that it does not know.
pub const UNSUPPORTED_OPERATION: &[u8] = b"unsupported-operation";
/// Error category with which a guest agent refuses a malformed request.
pub const INVALID_REQUEST: &str = "invalid-request";
/// Error category with which a guest agent refuses a request while a workload runs.
pub const BUSY: &str = "busy";
/// Error category with which a guest agent reports a workload that it could not launch.
pub const LAUNCH_FAILED: &str = "launch-failed";
/// Error category with which a guest agent reports a working directory that the workload cannot
/// enter. The status is the guest's error number, and nothing of the workload ran.
pub const CWD_FAILED: &str = "cwd-failed";
/// Error category with which a guest agent refuses a workload until it has applied every host
/// mapping that the kernel command line announced.
pub const MAPPINGS_INCOMPLETE: &str = "mappings-incomplete";
/// Error category with which a guest agent reports a host mapping that it could not apply. The
/// status is the guest's error number.
pub const MAPPING_FAILED: &str = "mapping-failed";

/// Decoded application frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppFrame<'a> {
    /// One of the `APP_*` frame kinds.
    pub kind: u8,
    /// ID of the request, which its responses repeat.
    pub request_id: u64,
    /// Status of a response; zero in requests.
    pub status: i32,
    /// Payload that follows the header.
    pub payload: &'a [u8],
}

/// Encodes one application frame.
pub fn encode_app(kind: u8, request_id: u64, status: i32, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut frame = Vec::with_capacity(APP_HEADER_LEN + payload.len());
    frame.extend_from_slice(&APP_MAGIC);
    frame.push(APP_VERSION);
    frame.push(kind);
    frame.extend_from_slice(&0u16.to_le_bytes());
    frame.extend_from_slice(&request_id.to_le_bytes());
    frame.extend_from_slice(&status.to_le_bytes());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Decodes and validates an application frame.
pub fn decode_app(frame: &[u8]) -> Result<AppFrame<'_>, ProtocolError> {
    if frame.len() < APP_HEADER_LEN {
        return Err(violation(
            "control endpoint returned a truncated application frame",
        ));
    }
    let flags = u16::from_le_bytes([frame[6], frame[7]]);
    let length = u32::from_le_bytes(array(&frame[20..24])) as usize;
    let payload = &frame[APP_HEADER_LEN..];
    if frame[..4] != APP_MAGIC || frame[4] != APP_VERSION || flags != 0 || length != payload.len() {
        return Err(violation(
            "control endpoint returned an invalid application frame",
        ));
    }
    Ok(AppFrame {
        kind: frame[5],
        request_id: u64::from_le_bytes(array(&frame[8..16])),
        status: i32::from_le_bytes(array(&frame[16..20])),
        payload,
    })
}

/// Encodes the capture request's flag as one byte, 0 or 1.
#[cfg_attr(
    not(any(test, feature = "host")),
    expect(dead_code, reason = "SL7.7a makes nvx-init handle capture requests")
)]
pub(crate) fn encode_capture_payload(kill_executions: bool) -> [u8; 1] {
    [u8::from(kill_executions)]
}

/// Decodes the capture request's sole kill-executions flag.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL7.7a makes nvx-init handle capture requests")
)]
pub(crate) fn decode_capture_payload(payload: &[u8]) -> Result<bool, ProtocolError> {
    match payload {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(violation(
            "the capture request must contain one kill-executions flag",
        )),
    }
}

/// Exit category reported by the guest agent, the payload of an [`APP_EXIT`] response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCategory {
    /// The workload exited; the status is its exit code.
    Exit,
    /// The workload's timeout elapsed and the guest agent killed it.
    Timeout,
    /// The workload exceeded the output limit and the guest agent killed it.
    OutputLimit,
    /// A signal ended the workload; the status is 128 plus the signal number.
    Signal,
    /// The guest agent could not supervise the workload.
    Failed,
    /// A cancellation request ended the workload.
    Cancelled,
}

impl ExitCategory {
    /// Parses the payload of an [`APP_EXIT`] response.
    pub fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        match payload {
            b"exit" => Ok(Self::Exit),
            b"timeout" => Ok(Self::Timeout),
            b"output-limit" => Ok(Self::OutputLimit),
            b"signal" => Ok(Self::Signal),
            b"failed" => Ok(Self::Failed),
            b"cancelled" => Ok(Self::Cancelled),
            _ => Err(violation(
                "guest agent returned an unsupported exit category",
            )),
        }
    }

    /// Returns the payload of an [`APP_EXIT`] response that reports this category.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exit => "exit",
            Self::Timeout => "timeout",
            Self::OutputLimit => "output-limit",
            Self::Signal => "signal",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}
