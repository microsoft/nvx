//! Host side of an authenticated control session with a sandbox's guest agent: version 2 of NVX's
//! control protocol, `NVXC`, over the host's role of the shared NVXS session, [`HostConnection`].
//!
//! Every exchange checks that the guest agent's `READY` carries the protocol version, and the
//! session sends no `FEATURES`, which version 2 does not have. An execution streams: the host
//! sends its standard input under the window of [`STDIN_WINDOW`] bytes that the guest agent has
//! not acknowledged, ends it explicitly, and receives its output without a total cap.

use std::fmt;
use std::io;
use std::time::Instant;

use super::process::{BootId, ProcessRef};
use super::transport::{self, HostConnection, HostConnectionError, Transport};
use crate::protocol::nvxs::{self, RecordError, RecordType};
use crate::protocol::v2::{self, Frame, Outcome, STDIN_WINDOW};
use crate::protocol::{self, CAPABILITY_LEN, HostMap, OUTER_MAX_PAYLOAD, ProtocolError};
use crate::{Error, ErrorKind};

/// Failure of a control session.
#[derive(Debug)]
pub(crate) enum SessionError {
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

/// Connects to the control endpoint of OpenVMM process `pid`, retrying until `deadline` while
/// the endpoint is not yet available and the process is still alive.
pub(crate) fn connect(
    endpoint: &str,
    pid: u32,
    start_time: u64,
    deadline: Instant,
) -> Result<Box<dyn Transport>, SessionError> {
    let reference = ProcessRef {
        pid,
        start_time,
        boot: BootId::current()?,
    };
    transport::connect_verified(endpoint, &reference, deadline).map_err(connect_failure)
}

/// Preserves the existing error type for callers of the temporary endpoint wrapper.
fn connect_failure(error: Error) -> SessionError {
    let (kind, message, source) = error.into_parts();
    match kind {
        ErrorKind::TimedOut => SessionError::TimedOut,
        ErrorKind::Exited => SessionError::ProcessExited,
        _ => {
            let Some(source) = source else {
                return SessionError::Protocol(message);
            };
            match source.downcast::<io::Error>() {
                Ok(error)
                    if kind == ErrorKind::Protocol
                        && error.kind() == io::ErrorKind::PermissionDenied =>
                {
                    SessionError::Refused(*error)
                }
                Ok(error) => SessionError::Io(*error),
                Err(source) => SessionError::Io(io::Error::other(source)),
            }
        }
    }
}

/// Event observed while an execution runs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExecEvent {
    /// A chunk of the workload's standard output.
    Stdout(Vec<u8>),
    /// A chunk of the workload's standard error.
    Stderr(Vec<u8>),
    /// The guest agent wrote this many more bytes of the standard input into the workload's,
    /// which leave the window.
    Acknowledged(u32),
    /// The workload ended.
    Exit {
        /// How it ended.
        category: Outcome,
        /// Its exit status, 128 plus the signal number for [`Outcome::Signal`].
        status: i32,
    },
    /// The guest agent refused the workload, and nothing of it runs any longer.
    Rejected {
        /// The guest's error number.
        status: i32,
        /// The error category, such as `launch-failed`, truncated to 64 characters.
        category: String,
    },
}

/// Authenticated control session: `NVXC` frames of version 2, each in one DATA record of the
/// host's NVXS connection to OpenVMM's broker.
///
/// Received bytes are buffered until a whole record is available, so a read that times out
/// loses nothing and callers can poll with short deadlines. A failure that leaves the stream
/// unusable, such as a reset or a record that the protocol does not allow, ends the session, and
/// later calls fail.
pub(crate) struct ControlSession {
    /// The host's NVXS connection to OpenVMM's control endpoint.
    pub(crate) connection: HostConnection,
    /// The bytes of the running execution's standard input that the host sent and the guest agent
    /// has not acknowledged, at most [`STDIN_WINDOW`].
    unacknowledged: u32,
}

impl From<HostConnection> for ControlSession {
    fn from(connection: HostConnection) -> Self {
        Self {
            connection,
            unacknowledged: 0,
        }
    }
}

impl ControlSession {
    /// Authenticates with the capability OpenVMM received at launch, and waits until `deadline`
    /// while OpenVMM's broker waits for the guest to acknowledge the session.
    pub(crate) fn attach(
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> Result<Self, SessionError> {
        let connection = HostConnection::attach(transport, capability, deadline)
            .map_err(|error| failure(error, Stage::Attach))?;
        Ok(connection.into())
    }

    /// Waits until the guest agent acknowledges readiness.
    pub(crate) fn ping(&mut self, deadline: Instant) -> Result<(), SessionError> {
        self.acknowledged(Frame::Ping, "readiness", deadline)
    }

    /// Asks nvx-init to capture the VM, killing its executions first when requested.
    pub(crate) fn capture(
        &mut self,
        kill_executions: bool,
        deadline: Instant,
    ) -> Result<(), SessionError> {
        let payload = protocol::encode_capture_payload(kill_executions);
        self.acknowledged(Frame::Capture(&payload), "capture", deadline)
    }

    /// Sends nvx-init's activate message, whose `payload` carries a restore's guest-restore record
    /// and workload identity, and returns the guest restore that the guest's Ready reports.
    pub(crate) fn activate(
        &mut self,
        payload: &[u8],
        deadline: Instant,
    ) -> Result<crate::guest_restore::GuestRestoreReady, SessionError> {
        let request_id = request_id()?;
        self.send(request_id, Frame::Activate(payload), Some(deadline))?;
        let reply = self.reply(request_id, Some(deadline))?;
        let ready = ready(Frame::decode(&reply)?.1)?.ok_or_else(|| {
            SessionError::Protocol("the guest agent did not activate the restore".to_owned())
        })?;
        crate::guest_restore::GuestRestoreReady::decode(ready).map_err(|error| {
            SessionError::Protocol(format!("the guest's Ready is invalid: {error}"))
        })
    }

    /// Hands the guest agent the host mapping table, in as many requests as it takes, and waits
    /// until it has mounted every entry.
    pub(crate) fn map_host_paths(
        &mut self,
        maps: &[HostMap],
        deadline: Instant,
    ) -> Result<(), SessionError> {
        for payload in protocol::encode_maps_payloads(maps, v2::MAX_PAYLOAD)? {
            let request_id = request_id()?;
            self.send(request_id, Frame::Maps(&payload), Some(deadline))?;
            let reply = self.reply(request_id, Some(deadline))?;
            match Frame::decode(&reply)?.1 {
                Frame::Error { status, category }
                    if category == protocol::MAPPING_FAILED.as_bytes() =>
                {
                    return Err(SessionError::Protocol(format!(
                        "the guest agent could not mount a mapped host path (error {status})"
                    )));
                }
                Frame::Error { status, category } => {
                    let category = error_category(category);
                    return Err(SessionError::Protocol(format!(
                        "the guest agent did not mount the host paths: {category} (status \
                         {status})"
                    )));
                }
                frame if ready(frame)?.is_some_and(<[u8]>::is_empty) => {}
                _ => {
                    return Err(SessionError::Protocol(
                        "the guest agent returned an invalid host mapping response".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Starts the workload that the `EXEC` payload `payload` describes as the execution of
    /// `request_id`, which its events repeat, with none of its standard input in the window.
    pub(crate) fn start_exec(
        &mut self,
        request_id: u64,
        payload: &[u8],
        deadline: Instant,
    ) -> Result<(), SessionError> {
        self.unacknowledged = 0;
        self.send(request_id, Frame::Exec(payload), Some(deadline))
    }

    /// Sends the next bytes of `data` that the window takes, at most what one frame holds, as
    /// standard input of the execution of `request_id`, and returns how many it sent: none
    /// while the guest agent has not acknowledged [`STDIN_WINDOW`] bytes.
    pub(crate) fn send_stdin(
        &mut self,
        request_id: u64,
        data: &[u8],
        deadline: Instant,
    ) -> Result<usize, SessionError> {
        let room = (STDIN_WINDOW - self.unacknowledged) as usize;
        let chunk = &data[..data.len().min(room).min(v2::MAX_PAYLOAD)];
        if !chunk.is_empty() {
            self.send(request_id, Frame::Stdin(chunk), Some(deadline))?;
            self.unacknowledged += chunk.len() as u32;
        }
        Ok(chunk.len())
    }

    /// Ends the standard input of the execution of `request_id` after the bytes that it sent.
    pub(crate) fn close_stdin(
        &mut self,
        request_id: u64,
        deadline: Instant,
    ) -> Result<(), SessionError> {
        self.send(request_id, Frame::StdinEnd, Some(deadline))
    }

    /// Reads the next event of the workload started with `request_id`.
    pub(crate) fn next_exec_event(
        &mut self,
        request_id: u64,
        deadline: Option<Instant>,
    ) -> Result<ExecEvent, SessionError> {
        let reply = self.reply(request_id, deadline)?;
        Ok(match Frame::decode(&reply)?.1 {
            Frame::Stdout(data) => ExecEvent::Stdout(data.to_vec()),
            Frame::Stderr(data) => ExecEvent::Stderr(data.to_vec()),
            Frame::StdinAck(count) => {
                self.unacknowledged = self.unacknowledged.checked_sub(count).ok_or_else(|| {
                    SessionError::Protocol(
                        "the guest agent acknowledged standard input that it did not receive"
                            .to_owned(),
                    )
                })?;
                ExecEvent::Acknowledged(count)
            }
            Frame::Exit { status, outcome } => ExecEvent::Exit {
                category: outcome,
                status,
            },
            Frame::Error { status, category } => ExecEvent::Rejected {
                status,
                category: error_category(category),
            },
            _ => {
                return Err(SessionError::Protocol(
                    "the guest agent returned an invalid exec response".to_owned(),
                ));
            }
        })
    }

    /// Asks the guest agent to terminate the workload started with `request_id`.
    ///
    /// The guest reports the workload's outcome as a regular exec event, with the cancelled
    /// category if the request arrived before the workload exited. A request for a workload that
    /// already finished is ignored.
    pub(crate) fn cancel_exec(
        &mut self,
        request_id: u64,
        deadline: Instant,
    ) -> Result<(), SessionError> {
        self.send(request_id, Frame::Cancel, Some(deadline))
    }

    /// Asks the guest agent to shut the VM down, which first ends an execution that runs: the
    /// frames of that execution, which come before the acknowledgement, are skipped.
    pub(crate) fn stop(&mut self, deadline: Instant) -> Result<(), SessionError> {
        let request_id = request_id()?;
        self.send(request_id, Frame::Stop, Some(deadline))?;
        loop {
            let record = self.receive(Some(deadline))?;
            match Frame::decode(&record)? {
                (id, Frame::Stopped) if id == request_id => return Ok(()),
                (
                    id,
                    Frame::Stdout(_)
                    | Frame::Stderr(_)
                    | Frame::StdinAck(_)
                    | Frame::Exit { .. }
                    | Frame::Error { .. },
                ) if id != request_id => {}
                _ => {
                    return Err(SessionError::Protocol(
                        "the guest agent did not acknowledge stop".to_owned(),
                    ));
                }
            }
        }
    }

    /// Encodes `frame` with `request_id`, and sends it in the next DATA record, which the guest
    /// agent takes as one frame.
    fn send(
        &mut self,
        request_id: u64,
        frame: Frame<'_>,
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        let frame = frame.encode(request_id)?;
        self.connection
            .send(&frame, deadline)
            .map_err(|error| failure(error, Stage::Data))
    }

    /// Returns the payload of the next DATA record: the bytes of the guest agent's next frame.
    fn receive(&mut self, deadline: Option<Instant>) -> Result<Vec<u8>, SessionError> {
        self.connection
            .receive(deadline)
            .map_err(|error| failure(error, Stage::Data))
    }

    /// Returns the bytes of the next frame, which must answer `request_id`.
    fn reply(
        &mut self,
        request_id: u64,
        deadline: Option<Instant>,
    ) -> Result<Vec<u8>, SessionError> {
        let record = self.receive(deadline)?;
        if Frame::decode(&record)?.0 != request_id {
            return Err(SessionError::Protocol(
                "the guest agent returned a mismatched request ID".to_owned(),
            ));
        }
        Ok(record)
    }

    /// Sends `request` and waits until the guest agent acknowledges `what` with a `READY` of its
    /// request ID that carries nothing but the protocol version.
    fn acknowledged(
        &mut self,
        request: Frame<'_>,
        what: &str,
        deadline: Instant,
    ) -> Result<(), SessionError> {
        let request_id = request_id()?;
        self.send(request_id, request, Some(deadline))?;
        let reply = self.receive(Some(deadline))?;
        match Frame::decode(&reply)? {
            (id, frame) if id == request_id && ready(frame)?.is_some_and(<[u8]>::is_empty) => {
                Ok(())
            }
            _ => Err(SessionError::Protocol(format!(
                "the guest agent did not acknowledge {what}"
            ))),
        }
    }
}

/// Returns what a `READY` reports about a guest restore, once it checked that the `READY` carries
/// the protocol version, or `None` for another frame.
fn ready(frame: Frame<'_>) -> Result<Option<&[u8]>, SessionError> {
    match frame {
        Frame::Ready {
            version: v2::VERSION,
            activation,
        } => Ok(Some(activation)),
        Frame::Ready { version, .. } => Err(SessionError::Protocol(format!(
            "the guest agent speaks NVXC version {version}, not version {}",
            v2::VERSION
        ))),
        _ => Ok(None),
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
fn failure(error: Error, stage: Stage) -> SessionError {
    let (kind, message, source) = error.into_parts();
    let Some(source) = source else {
        return match kind {
            ErrorKind::TimedOut => SessionError::TimedOut,
            ErrorKind::Exited => SessionError::ProcessExited,
            _ => SessionError::Protocol(message),
        };
    };
    let error = match source.downcast::<HostConnectionError>() {
        Ok(error) => *error,
        Err(source) => match source.downcast::<nvxs::SessionError>() {
            Ok(error) => HostConnectionError::Protocol(*error),
            Err(source) => match source.downcast::<io::Error>() {
                Ok(error) => HostConnectionError::Io(*error),
                Err(source) => HostConnectionError::Io(io::Error::other(source)),
            },
        },
    };
    connection_failure(error, stage)
}

/// Maps retained connection cause details to the legacy NVXC failure at `stage`.
fn connection_failure(error: HostConnectionError, stage: Stage) -> SessionError {
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
