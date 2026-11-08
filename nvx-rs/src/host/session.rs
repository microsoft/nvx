//! Host side of an authenticated control session with a sandbox's guest agent: NVX's control
//! protocol, `NVXC`, over the host's role of the shared NVXS session, [`HostConnection`].

use std::fmt;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use super::nvxs::{HostConnection, HostConnectionError};
use super::platform::{self, Transport};
use crate::protocol::nvxs::{self, RecordError, RecordType};
use crate::protocol::{
    self, APP_CANCEL, APP_ERROR, APP_EXEC, APP_EXIT, APP_FEATURES, APP_HEADER_LEN, APP_MAPS,
    APP_PING, APP_READY, APP_STDERR, APP_STDOUT, APP_STOP, APP_STOPPED, CAPABILITY_LEN,
    ExitCategory, GuestFeatures, HostMap, OUTER_MAX_PAYLOAD, ProtocolError, UNSUPPORTED_OPERATION,
    Workload,
};

/// Failure of a control session.
#[derive(Debug)]
pub enum SessionError {
    /// The deadline passed.
    TimedOut,
    /// The OpenVMM process exited.
    ProcessExited,
    /// The endpoint closed the stream.
    Closed,
    /// OpenVMM reset the session.
    Reset,
    /// OpenVMM rejected the capability.
    AuthenticationFailed,
    /// The endpoint cannot be used, for example because another process serves it.
    Refused(io::Error),
    /// The peer violated the protocol.
    Protocol(String),
    /// An I/O error occurred.
    Io(io::Error),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => formatter.write_str("the control operation timed out"),
            Self::ProcessExited => formatter.write_str("the OpenVMM process exited"),
            Self::Closed => formatter.write_str("the control endpoint closed the session"),
            Self::Reset => formatter.write_str("the control session was reset"),
            Self::AuthenticationFailed => {
                formatter.write_str("the control endpoint rejected the sandbox capability")
            }
            Self::Refused(error) => write!(formatter, "the control endpoint was refused: {error}"),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Io(error) => write!(formatter, "control I/O failed: {error}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<ProtocolError> for SessionError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error.into_message())
    }
}

impl From<io::Error> for SessionError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::TimedOut {
            Self::TimedOut
        } else {
            Self::Io(error)
        }
    }
}

/// Connects to an endpoint that OpenVMM process `pid` serves, its control endpoint, or the boot
/// console of a clone,
/// [`Store::clone_console_endpoint`](crate::host::template::Store::clone_console_endpoint),
/// retrying until `deadline` while the endpoint is not yet available and the process is still
/// alive. An endpoint that another process serves is [`SessionError::Refused`]. Once connected, it
/// checks that the process with the ID `pid` still started at `start_time`, as the caller recorded
/// it: one that took the ID of the recorded process once that process exited is another, whose
/// endpoint the caller must not take for its own, since a clone's boot console authenticates
/// no one, so the connection then fails with [`SessionError::ProcessExited`], as one to a process
/// that exited does, and with [`SessionError::Io`] if the start time cannot be read, rather than
/// return an endpoint that it could not verify.
pub fn connect(
    endpoint: &str,
    pid: u32,
    start_time: u64,
    deadline: Instant,
) -> Result<Box<dyn Transport>, SessionError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match platform::connect_endpoint(endpoint, pid, remaining.min(Duration::from_millis(250))) {
            Ok(transport) => {
                return verified(transport, platform::process_start_time(pid), start_time);
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Err(SessionError::Refused(error));
            }
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                return Err(SessionError::Io(error));
            }
            Err(_) => {}
        }
        match platform::process_start_time(pid) {
            Ok(Some(current)) if current == start_time => {}
            // An inconclusive check keeps waiting for the endpoint until the deadline.
            Err(_) => {}
            Ok(_) => return Err(SessionError::ProcessExited),
        }
        if Instant::now() >= deadline {
            return Err(SessionError::TimedOut);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Returns `transport`, a connection to an endpoint that the process with a recorded ID serves,
/// if `current`, the start time of the process that has that ID now, is the recorded
/// `start_time`, as [`connect`] checks it. Fails with [`SessionError::ProcessExited`] if another
/// process has the ID, or none does, and with [`SessionError::Io`] if the start time cannot be
/// read, which verifies nothing.
fn verified(
    transport: Box<dyn Transport>,
    current: io::Result<Option<u64>>,
    start_time: u64,
) -> Result<Box<dyn Transport>, SessionError> {
    match current {
        Ok(Some(current)) if current == start_time => Ok(transport),
        Ok(_) => Err(SessionError::ProcessExited),
        Err(error) => Err(SessionError::Io(error)),
    }
}

/// Event observed while an execution runs.
#[derive(Debug, PartialEq, Eq)]
pub enum ExecEvent {
    /// A chunk of the workload's standard output.
    Stdout(Vec<u8>),
    /// A chunk of the workload's standard error.
    Stderr(Vec<u8>),
    /// The workload ended.
    Exit {
        /// How it ended.
        category: ExitCategory,
        /// Its exit status, 128 plus the signal number for [`ExitCategory::Signal`].
        status: i32,
    },
    /// The guest agent refused the workload, and nothing of it runs any longer.
    Rejected {
        /// The guest's error number.
        status: i32,
        /// The error category, such as [`LAUNCH_FAILED`](crate::protocol::LAUNCH_FAILED),
        /// truncated to 64 characters.
        category: String,
    },
}

/// Authenticated control session: `NVXC` frames, each in one DATA record of the host's NVXS
/// connection to OpenVMM's broker.
///
/// Received bytes are buffered until a whole record is available, so a read that times out
/// loses nothing and callers can poll with short deadlines. A failure that leaves the stream
/// unusable, such as a reset or a record that the protocol does not allow, ends the session, and
/// later calls fail.
pub struct ControlSession {
    /// The host's NVXS connection to OpenVMM's control endpoint.
    connection: HostConnection,
}

impl ControlSession {
    /// Authenticates with the capability OpenVMM received at launch, and waits until `deadline`
    /// while OpenVMM's broker waits for the guest to acknowledge the session.
    pub fn attach(
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> Result<Self, SessionError> {
        let connection = HostConnection::attach(transport, capability, deadline)
            .map_err(|error| failure(error, Stage::Attach))?;
        Ok(Self { connection })
    }

    /// Waits until the guest agent acknowledges readiness.
    pub fn ping(&mut self, deadline: Instant) -> Result<(), SessionError> {
        self.acknowledged(APP_PING, APP_READY, "readiness", deadline)
    }

    /// Asks the guest agent which control features its image provides.
    ///
    /// An agent that predates the request refuses it as an unsupported operation, which means
    /// that the image provides none of them.
    pub fn features(&mut self, deadline: Instant) -> Result<GuestFeatures, SessionError> {
        let request_id = request_id()?;
        self.send_app(APP_FEATURES, request_id, &[], Some(deadline))?;
        let (kind, status, payload) = self.read_reply(request_id, Some(deadline))?;
        match kind {
            APP_READY if status == 0 => Ok(GuestFeatures::decode(&payload)?),
            APP_ERROR if payload == UNSUPPORTED_OPERATION => Ok(GuestFeatures::NONE),
            _ => Err(SessionError::Protocol(
                "the guest agent returned an invalid features response".to_owned(),
            )),
        }
    }

    /// Hands the guest agent the host mapping table, in as many requests as it takes, and waits
    /// until it has mounted every entry.
    pub fn map_host_paths(
        &mut self,
        maps: &[HostMap],
        deadline: Instant,
    ) -> Result<(), SessionError> {
        let payloads = protocol::encode_maps_payloads(maps, OUTER_MAX_PAYLOAD - APP_HEADER_LEN)?;
        for payload in payloads {
            let request_id = request_id()?;
            self.send_app(APP_MAPS, request_id, &payload, Some(deadline))?;
            let (kind, status, payload) = self.read_reply(request_id, Some(deadline))?;
            match kind {
                APP_READY if status == 0 && payload.is_empty() => {}
                APP_ERROR if payload == protocol::MAPPING_FAILED.as_bytes() => {
                    return Err(SessionError::Protocol(format!(
                        "the guest agent could not mount a mapped host path (error {status})"
                    )));
                }
                APP_ERROR => {
                    let category = error_category(&payload);
                    return Err(SessionError::Protocol(format!(
                        "the guest agent did not mount the host paths: {category} (status \
                         {status})"
                    )));
                }
                _ => {
                    return Err(SessionError::Protocol(
                        "the guest agent returned an invalid host mapping response".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Starts a workload and returns the request ID that identifies its events.
    pub fn start_exec(
        &mut self,
        workload: &Workload<'_>,
        deadline: Instant,
    ) -> Result<u64, SessionError> {
        let payload = protocol::encode_exec_payload(workload)?;
        let request_id = request_id()?;
        self.send_app(APP_EXEC, request_id, &payload, Some(deadline))?;
        Ok(request_id)
    }

    /// Reads the next event of the workload started with `request_id`.
    pub fn next_exec_event(
        &mut self,
        request_id: u64,
        deadline: Option<Instant>,
    ) -> Result<ExecEvent, SessionError> {
        let (kind, status, payload) = self.read_reply(request_id, deadline)?;
        match kind {
            APP_STDOUT => Ok(ExecEvent::Stdout(payload)),
            APP_STDERR => Ok(ExecEvent::Stderr(payload)),
            APP_EXIT => Ok(ExecEvent::Exit {
                category: ExitCategory::parse(&payload)?,
                status,
            }),
            APP_ERROR => Ok(ExecEvent::Rejected {
                status,
                category: error_category(&payload),
            }),
            _ => Err(SessionError::Protocol(
                "the guest agent returned an invalid exec response".to_owned(),
            )),
        }
    }

    /// Asks the guest agent to terminate the workload started with `request_id`.
    ///
    /// The guest reports the workload's outcome as a regular exec event, with the cancelled
    /// category if the request arrived before the workload exited. A request for a workload that
    /// already finished is ignored.
    pub fn cancel_exec(&mut self, request_id: u64, deadline: Instant) -> Result<(), SessionError> {
        self.send_app(APP_CANCEL, request_id, &[], Some(deadline))
    }

    /// Asks the guest agent to shut the VM down.
    pub fn stop(&mut self, deadline: Instant) -> Result<(), SessionError> {
        self.acknowledged(APP_STOP, APP_STOPPED, "stop", deadline)
    }

    /// Sends an application request of `kind` with `request_id` and `payload` in the next
    /// sequenced outer record, for a caller that checks the responses itself, as the `nvx-rs`
    /// CLI does with the messages of `nvx.py`.
    pub fn send_request(
        &mut self,
        kind: u8,
        request_id: u64,
        payload: &[u8],
        deadline: Instant,
    ) -> Result<(), SessionError> {
        self.send_app(kind, request_id, payload, Some(deadline))
    }

    /// Reads the next application frame, checked against the session's identity and sequence,
    /// and returns its kind, request ID, status, and payload.
    pub fn read_response(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<(u8, u64, i32, Vec<u8>), SessionError> {
        self.read_app(deadline)
    }

    /// Encodes and sends one application frame in the next DATA record.
    fn send_app(
        &mut self,
        kind: u8,
        request_id: u64,
        payload: &[u8],
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        let frame = protocol::encode_app(kind, request_id, 0, payload);
        // The guest agent takes each DATA record as one frame, so a frame must fit one.
        if frame.len() > OUTER_MAX_PAYLOAD {
            return Err(SessionError::Protocol(
                "control payload exceeds the outer protocol limit".to_owned(),
            ));
        }
        self.connection
            .send(&frame, deadline)
            .map_err(|error| failure(error, Stage::Data))
    }

    /// Reads and decodes the next application frame, the payload of the next DATA record.
    fn read_app(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<(u8, u64, i32, Vec<u8>), SessionError> {
        let payload = self
            .connection
            .receive(deadline)
            .map_err(|error| failure(error, Stage::Data))?;
        let frame = protocol::decode_app(&payload)?;
        Ok((
            frame.kind,
            frame.request_id,
            frame.status,
            frame.payload.to_vec(),
        ))
    }

    /// Reads the next application frame, which must answer `request_id`, and returns its kind,
    /// status, and payload.
    fn read_reply(
        &mut self,
        request_id: u64,
        deadline: Option<Instant>,
    ) -> Result<(u8, i32, Vec<u8>), SessionError> {
        let (kind, response_id, status, payload) = self.read_app(deadline)?;
        if response_id != request_id {
            return Err(SessionError::Protocol(
                "the guest agent returned a mismatched request ID".to_owned(),
            ));
        }
        Ok((kind, status, payload))
    }

    /// Sends a request of `kind` without a payload and waits until the guest agent acknowledges
    /// `what` with an empty `reply` of status 0.
    fn acknowledged(
        &mut self,
        kind: u8,
        reply: u8,
        what: &str,
        deadline: Instant,
    ) -> Result<(), SessionError> {
        let request_id = request_id()?;
        self.send_app(kind, request_id, &[], Some(deadline))?;
        let (kind, response_id, status, payload) = self.read_app(Some(deadline))?;
        if kind != reply || response_id != request_id || status != 0 || !payload.is_empty() {
            return Err(SessionError::Protocol(format!(
                "the guest agent did not acknowledge {what}"
            )));
        }
        Ok(())
    }
}

/// Stage of a control session at which a failure arrived, which names a protocol violation.
#[derive(Clone, Copy)]
enum Stage {
    /// The host attaches.
    Attach,
    /// Application frames flow.
    Data,
}

/// Converts a failure of the host's NVXS connection at `stage` into the session's failure.
///
/// A violation keeps the message of the check that it fails: a malformed or oversized header, a
/// payload in WAIT or READY, an empty DATA record, which carries no frame, and otherwise an
/// invalid attach response or data record. A stream that ends inside a record has closed.
fn failure(error: HostConnectionError, stage: Stage) -> SessionError {
    let violation = match error {
        HostConnectionError::TimedOut => return SessionError::TimedOut,
        HostConnectionError::Closed => return SessionError::Closed,
        HostConnectionError::Rejected(_) => return SessionError::AuthenticationFailed,
        HostConnectionError::Reset(_) => return SessionError::Reset,
        HostConnectionError::Io(error) => return error.into(),
        HostConnectionError::Ended => {
            return SessionError::Protocol(
                "the control session ended after an earlier failure".to_owned(),
            );
        }
        HostConnectionError::Protocol(violation) => violation,
    };
    let message = match (violation, stage) {
        (nvxs::SessionError::Truncated { .. }, _) => return SessionError::Closed,
        (
            nvxs::SessionError::Record(
                RecordError::Magic(_) | RecordError::Version(_) | RecordError::Flags(_),
            ),
            _,
        ) => "control endpoint returned an invalid outer record",
        (nvxs::SessionError::Record(RecordError::Length { length, .. }), _)
            if length > OUTER_MAX_PAYLOAD =>
        {
            "control endpoint returned an oversized outer record"
        }
        (
            nvxs::SessionError::Record(RecordError::Length {
                record_type: RecordType::Wait | RecordType::Ready,
                ..
            }),
            Stage::Attach,
        ) => "control attach response carried a payload",
        (
            nvxs::SessionError::Record(RecordError::Length {
                record_type: RecordType::Data,
                length: 0,
            }),
            Stage::Data,
        ) => "control endpoint returned a truncated application frame",
        (_, Stage::Attach) => "control endpoint returned an invalid attach response",
        (_, Stage::Data) => "control endpoint returned an invalid data record",
    };
    SessionError::Protocol(message.to_owned())
}

/// Returns the error category that the payload of an `APP_ERROR` response names, truncated to 64
/// characters.
fn error_category(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload).chars().take(64).collect()
}

/// Generates a random nonzero request identifier.
fn request_id() -> Result<u64, SessionError> {
    loop {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|error| SessionError::Io(io::Error::other(error)))?;
        let value = u64::from_le_bytes(bytes);
        if value != 0 {
            return Ok(value);
        }
    }
}

#[cfg(test)]
mod tests;
