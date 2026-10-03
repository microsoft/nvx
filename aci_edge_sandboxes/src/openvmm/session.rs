//! Host side of an authenticated control session with a sandbox's guest agent.

use std::fmt;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use super::platform::{self, Transport};
use super::protocol::{
    self, APP_CANCEL, APP_ERROR, APP_EXEC, APP_EXIT, APP_FEATURES, APP_PING, APP_READY, APP_STDERR,
    APP_STDOUT, APP_STOP, APP_STOPPED, CAPABILITY_LEN, ExitCategory, GuestFeatures, OUTER_DATA,
    OUTER_ERROR, OUTER_HEADER_LEN, OUTER_HOST_ATTACH, OUTER_READY, OUTER_RESET, OUTER_WAIT,
    OuterHeader, ProtocolError, UNSUPPORTED_OPERATION,
};

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
        Self::Protocol(error.0)
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
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match platform::connect_endpoint(endpoint, pid, remaining.min(Duration::from_millis(250))) {
            Ok(transport) => return Ok(transport),
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

/// Event observed while an execution runs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExecEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit { category: ExitCategory, status: i32 },
    Rejected { status: i32, category: String },
}

/// Authenticated control session.
///
/// Received bytes are buffered until a whole record is available, so a read that times out
/// loses nothing and callers can poll with short deadlines.
pub(crate) struct ControlSession {
    transport: Box<dyn Transport>,
    received: Vec<u8>,
    instance_id: [u8; 16],
    epoch: u64,
    send_sequence: u64,
    receive_sequence: u64,
}

impl ControlSession {
    /// Authenticates with the capability OpenVMM received at launch.
    pub(crate) fn attach(
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> Result<Self, SessionError> {
        let mut session = Self {
            transport,
            received: Vec::new(),
            instance_id: [0; 16],
            epoch: 0,
            send_sequence: 0,
            receive_sequence: 0,
        };
        let record = protocol::encode_outer(OUTER_HOST_ATTACH, &[0; 16], 0, 0, capability)?;
        session
            .transport
            .write_all(&record, remaining(Some(deadline))?)?;
        loop {
            let (header, payload) = session.read_outer(Some(deadline))?;
            if !payload.is_empty() {
                return Err(SessionError::Protocol(
                    "control attach response carried a payload".to_owned(),
                ));
            }
            match header.record_type {
                // Another client holds the broker's single host slot.
                OUTER_WAIT => continue,
                OUTER_ERROR => return Err(SessionError::AuthenticationFailed),
                OUTER_READY if header.instance_id != [0; 16] && header.epoch != 0 => {
                    session.instance_id = header.instance_id;
                    session.epoch = header.epoch;
                    session.receive_sequence = header.sequence.wrapping_add(1);
                    return Ok(session);
                }
                _ => {
                    return Err(SessionError::Protocol(
                        "control endpoint returned an invalid attach response".to_owned(),
                    ));
                }
            }
        }
    }

    /// Waits until the guest agent acknowledges readiness.
    pub(crate) fn ping(&mut self, deadline: Instant) -> Result<(), SessionError> {
        let request_id = request_id()?;
        self.send_app(APP_PING, request_id, &[], Some(deadline))?;
        let (kind, response_id, status, payload) = self.read_app(Some(deadline))?;
        if kind != APP_READY || response_id != request_id || status != 0 || !payload.is_empty() {
            return Err(SessionError::Protocol(
                "the guest agent did not acknowledge readiness".to_owned(),
            ));
        }
        Ok(())
    }

    /// Asks the guest agent which control features its image provides.
    ///
    /// An agent that predates the request refuses it as an unsupported operation, which means
    /// that the image provides none of them.
    pub(crate) fn features(&mut self, deadline: Instant) -> Result<GuestFeatures, SessionError> {
        let request_id = request_id()?;
        self.send_app(APP_FEATURES, request_id, &[], Some(deadline))?;
        let (kind, response_id, status, payload) = self.read_app(Some(deadline))?;
        if response_id != request_id {
            return Err(SessionError::Protocol(
                "the guest agent returned a mismatched request ID".to_owned(),
            ));
        }
        match kind {
            APP_READY if status == 0 => Ok(GuestFeatures::decode(&payload)?),
            APP_ERROR if payload == UNSUPPORTED_OPERATION => Ok(GuestFeatures::NONE),
            _ => Err(SessionError::Protocol(
                "the guest agent returned an invalid features response".to_owned(),
            )),
        }
    }

    /// Starts a workload and returns the request ID that identifies its events.
    pub(crate) fn start_exec(
        &mut self,
        argv: &[String],
        timeout_ms: u32,
        deadline: Instant,
    ) -> Result<u64, SessionError> {
        let payload = protocol::encode_exec_payload(argv, timeout_ms)?;
        let request_id = request_id()?;
        self.send_app(APP_EXEC, request_id, &payload, Some(deadline))?;
        Ok(request_id)
    }

    /// Reads the next event of the workload started with `request_id`.
    pub(crate) fn next_exec_event(
        &mut self,
        request_id: u64,
        deadline: Option<Instant>,
    ) -> Result<ExecEvent, SessionError> {
        let (kind, response_id, status, payload) = self.read_app(deadline)?;
        if response_id != request_id {
            return Err(SessionError::Protocol(
                "the guest agent returned a mismatched request ID".to_owned(),
            ));
        }
        match kind {
            APP_STDOUT => Ok(ExecEvent::Stdout(payload)),
            APP_STDERR => Ok(ExecEvent::Stderr(payload)),
            APP_EXIT => Ok(ExecEvent::Exit {
                category: ExitCategory::parse(&payload)?,
                status,
            }),
            APP_ERROR => {
                let category: String = String::from_utf8_lossy(&payload).chars().take(64).collect();
                Ok(ExecEvent::Rejected { status, category })
            }
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
    pub(crate) fn cancel_exec(
        &mut self,
        request_id: u64,
        deadline: Instant,
    ) -> Result<(), SessionError> {
        self.send_app(APP_CANCEL, request_id, &[], Some(deadline))
    }

    /// Asks the guest agent to shut the VM down.
    pub(crate) fn stop(&mut self, deadline: Instant) -> Result<(), SessionError> {
        let request_id = request_id()?;
        self.send_app(APP_STOP, request_id, &[], Some(deadline))?;
        let (kind, response_id, status, payload) = self.read_app(Some(deadline))?;
        if kind != APP_STOPPED || response_id != request_id || status != 0 || !payload.is_empty() {
            return Err(SessionError::Protocol(
                "the guest agent did not acknowledge stop".to_owned(),
            ));
        }
        Ok(())
    }

    fn send_app(
        &mut self,
        kind: u8,
        request_id: u64,
        payload: &[u8],
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        let frame = protocol::encode_app(kind, request_id, 0, payload);
        let record = protocol::encode_outer(
            OUTER_DATA,
            &self.instance_id,
            self.epoch,
            self.send_sequence,
            &frame,
        )?;
        self.transport.write_all(&record, remaining(deadline)?)?;
        self.send_sequence = self.send_sequence.wrapping_add(1);
        Ok(())
    }

    fn read_app(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<(u8, u64, i32, Vec<u8>), SessionError> {
        let (header, frame) = self.read_outer(deadline)?;
        if header.record_type == OUTER_RESET {
            return Err(SessionError::Reset);
        }
        if header.record_type != OUTER_DATA
            || header.instance_id != self.instance_id
            || header.epoch != self.epoch
            || header.sequence != self.receive_sequence
        {
            return Err(SessionError::Protocol(
                "control endpoint returned an invalid data record".to_owned(),
            ));
        }
        self.receive_sequence = self.receive_sequence.wrapping_add(1);
        let frame = protocol::decode_app(&frame)?;
        Ok((
            frame.kind,
            frame.request_id,
            frame.status,
            frame.payload.to_vec(),
        ))
    }

    fn read_outer(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<(OuterHeader, Vec<u8>), SessionError> {
        loop {
            if self.received.len() >= OUTER_HEADER_LEN {
                let header = protocol::decode_outer_header(
                    self.received[..OUTER_HEADER_LEN]
                        .try_into()
                        .expect("the slice has the header length"),
                )?;
                let end = OUTER_HEADER_LEN + header.length;
                if self.received.len() >= end {
                    let payload = self.received[OUTER_HEADER_LEN..end].to_vec();
                    self.received.drain(..end);
                    return Ok((header, payload));
                }
            }
            self.fill(deadline)?;
        }
    }

    /// Appends the next bytes from the transport to the receive buffer.
    fn fill(&mut self, deadline: Option<Instant>) -> Result<(), SessionError> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match self.transport.read(&mut chunk, remaining(deadline)?) {
                Ok(0) => return Err(SessionError::Closed),
                Ok(count) => {
                    self.received.extend_from_slice(&chunk[..count]);
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// Converts a deadline into the time left, failing once it has passed.
fn remaining(deadline: Option<Instant>) -> Result<Option<Duration>, SessionError> {
    match deadline {
        None => Ok(None),
        Some(deadline) => deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .map(Some)
            .ok_or(SessionError::TimedOut),
    }
}

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
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::openvmm::protocol::{decode_app, decode_outer_header, encode_app, encode_outer};

    const INSTANCE: [u8; 16] = [7; 16];

    /// Transport that replays scripted guest records and records host writes.
    struct Scripted {
        input: VecDeque<u8>,
        output: Arc<Mutex<Vec<u8>>>,
    }

    impl Transport for Scripted {
        fn read(&mut self, buffer: &mut [u8], _timeout: Option<Duration>) -> io::Result<usize> {
            if self.input.is_empty() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let count = buffer.len().min(self.input.len());
            for byte in &mut buffer[..count] {
                *byte = self.input.pop_front().unwrap();
            }
            Ok(count)
        }

        fn write_all(&mut self, data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
            self.output.lock().unwrap().extend_from_slice(data);
            Ok(())
        }
    }

    fn outer(record_type: u8, epoch: u64, sequence: u64, payload: &[u8]) -> Vec<u8> {
        let instance = if epoch == 0 { [0; 16] } else { INSTANCE };
        encode_outer(record_type, &instance, epoch, sequence, payload).unwrap()
    }

    fn scripted(records: &[Vec<u8>]) -> (Box<dyn Transport>, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let transport = Scripted {
            input: records.concat().into(),
            output: Arc::clone(&output),
        };
        (Box::new(transport), output)
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// Transport of a guest that answers each application request with `answer(kind, request_id)`.
    struct Responder {
        input: VecDeque<u8>,
        next_sequence: u64,
        answer: fn(u8, u64) -> Vec<u8>,
    }

    impl Transport for Responder {
        fn read(&mut self, buffer: &mut [u8], _timeout: Option<Duration>) -> io::Result<usize> {
            if self.input.is_empty() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let count = buffer.len().min(self.input.len());
            for byte in &mut buffer[..count] {
                *byte = self.input.pop_front().unwrap();
            }
            Ok(count)
        }

        fn write_all(&mut self, data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
            let header = decode_outer_header(data[..OUTER_HEADER_LEN].try_into().unwrap()).unwrap();
            if header.record_type == OUTER_DATA {
                let request = decode_app(&data[OUTER_HEADER_LEN..]).unwrap();
                let reply = (self.answer)(request.kind, request.request_id);
                self.input
                    .extend(outer(OUTER_DATA, 1, self.next_sequence, &reply));
                self.next_sequence += 1;
            }
            Ok(())
        }
    }

    /// Attaches to a guest that answers each application request with `answer`.
    fn attached(answer: fn(u8, u64) -> Vec<u8>) -> ControlSession {
        let guest = Responder {
            input: outer(OUTER_READY, 1, 0, &[]).into(),
            next_sequence: 1,
            answer,
        };
        ControlSession::attach(Box::new(guest), &[1; CAPABILITY_LEN], deadline()).unwrap()
    }

    /// Splits the host's writes into (record type, sequence, application frame) tuples.
    fn host_records(output: &[u8]) -> Vec<(u8, u64, Vec<u8>)> {
        let mut records = Vec::new();
        let mut rest = output;
        while !rest.is_empty() {
            let header = decode_outer_header(rest[..OUTER_HEADER_LEN].try_into().unwrap()).unwrap();
            let end = OUTER_HEADER_LEN + header.length;
            records.push((
                header.record_type,
                header.sequence,
                rest[OUTER_HEADER_LEN..end].to_vec(),
            ));
            rest = &rest[end..];
        }
        records
    }

    #[test]
    fn attach_waits_for_the_host_slot_then_authenticates() {
        let (transport, output) =
            scripted(&[outer(OUTER_WAIT, 0, 0, &[]), outer(OUTER_READY, 4, 9, &[])]);
        let capability = [0x5a; CAPABILITY_LEN];
        let session = ControlSession::attach(transport, &capability, deadline()).unwrap();
        assert_eq!(session.epoch, 4);
        assert_eq!(session.receive_sequence, 10);
        let records = host_records(&output.lock().unwrap());
        assert_eq!(records, [(OUTER_HOST_ATTACH, 0, capability.to_vec())]);
    }

    #[test]
    fn attach_reports_authentication_failure() {
        let (transport, _) = scripted(&[outer(OUTER_ERROR, 0, 0, &[])]);
        let error = ControlSession::attach(transport, &[1; CAPABILITY_LEN], deadline())
            .err()
            .unwrap();
        assert!(matches!(error, SessionError::AuthenticationFailed));
    }

    #[test]
    fn exec_streams_events_in_order() {
        // The request ID is random, so the scripted guest answers the frames the host sent.
        let (transport, output) = scripted(&[outer(OUTER_READY, 1, 0, &[])]);
        let mut session =
            ControlSession::attach(transport, &[1; CAPABILITY_LEN], deadline()).unwrap();
        let argv = ["/bin/sh".to_owned(), "-c".to_owned(), "echo hi".to_owned()];
        let request_id = session.start_exec(&argv, 1000, deadline()).unwrap();
        let records = host_records(&output.lock().unwrap());
        let (record_type, sequence, frame) = &records[1];
        assert_eq!((*record_type, *sequence), (OUTER_DATA, 0));
        let frame = decode_app(frame).unwrap();
        assert_eq!((frame.kind, frame.request_id), (APP_EXEC, request_id));

        let replies = [
            outer(
                OUTER_DATA,
                1,
                1,
                &encode_app(APP_STDOUT, request_id, 0, b"hi\n"),
            ),
            outer(
                OUTER_DATA,
                1,
                2,
                &encode_app(APP_STDERR, request_id, 0, b"warn"),
            ),
            outer(
                OUTER_DATA,
                1,
                3,
                &encode_app(APP_EXIT, request_id, 3, b"exit"),
            ),
        ];
        session.transport = scripted(&replies).0;
        let events: Vec<ExecEvent> = (0..3)
            .map(|_| {
                session
                    .next_exec_event(request_id, Some(deadline()))
                    .unwrap()
            })
            .collect();
        assert_eq!(
            events,
            [
                ExecEvent::Stdout(b"hi\n".to_vec()),
                ExecEvent::Stderr(b"warn".to_vec()),
                ExecEvent::Exit {
                    category: ExitCategory::Exit,
                    status: 3
                },
            ]
        );
    }

    #[test]
    fn out_of_order_and_reset_records_fail_closed() {
        let (transport, _) = scripted(&[outer(OUTER_READY, 1, 0, &[])]);
        let mut session =
            ControlSession::attach(transport, &[1; CAPABILITY_LEN], deadline()).unwrap();
        session.transport =
            scripted(&[outer(OUTER_DATA, 1, 5, &encode_app(APP_STDOUT, 1, 0, b"x"))]).0;
        assert!(matches!(
            session.next_exec_event(1, Some(deadline())),
            Err(SessionError::Protocol(_))
        ));
        session.transport = scripted(&[outer(OUTER_RESET, 1, 1, &[])]).0;
        assert!(matches!(
            session.next_exec_event(1, Some(deadline())),
            Err(SessionError::Reset)
        ));
    }

    #[test]
    fn timed_out_polls_keep_partial_records() {
        let (transport, _) = scripted(&[outer(OUTER_READY, 1, 0, &[])]);
        let mut session =
            ControlSession::attach(transport, &[1; CAPABILITY_LEN], deadline()).unwrap();
        let record = outer(OUTER_DATA, 1, 1, &encode_app(APP_EXIT, 9, 0, b"cancelled"));
        let (first, second) = record.split_at(30);
        let (transport, output) = scripted(&[first.to_vec()]);
        session.transport = transport;
        assert!(matches!(
            session.next_exec_event(9, Some(deadline())),
            Err(SessionError::TimedOut)
        ));
        session.cancel_exec(9, deadline()).unwrap();
        session.transport = scripted(&[second.to_vec()]).0;
        assert_eq!(
            session.next_exec_event(9, Some(deadline())).unwrap(),
            ExecEvent::Exit {
                category: ExitCategory::Cancelled,
                status: 0
            }
        );
        let records = host_records(&output.lock().unwrap());
        let frame = decode_app(&records.last().unwrap().2).unwrap();
        assert_eq!((frame.kind, frame.request_id), (APP_CANCEL, 9));
    }

    #[test]
    fn features_report_what_the_guest_provides() {
        let mut session = attached(|kind, id| {
            assert_eq!(kind, APP_FEATURES);
            encode_app(APP_READY, id, 0, &GuestFeatures::REQUIRED.encode())
        });
        assert_eq!(
            session.features(deadline()).unwrap(),
            GuestFeatures::REQUIRED
        );
    }

    #[test]
    fn guests_that_predate_the_features_request_provide_none() {
        // This is how an agent without the request refuses an unknown request kind.
        let mut session = attached(|_, id| encode_app(APP_ERROR, id, 95, b"unsupported-operation"));
        assert_eq!(session.features(deadline()).unwrap(), GuestFeatures::NONE);
    }

    #[test]
    fn malformed_features_responses_fail_closed() {
        let answers: [fn(u8, u64) -> Vec<u8>; 5] = [
            |_, id| encode_app(APP_READY, id, 0, &[1, 0]),
            |_, id| encode_app(APP_ERROR, id, 16, b"busy"),
            |_, id| encode_app(APP_READY, id, 1, &[0; 4]),
            |_, id| encode_app(APP_READY, id ^ 1, 0, &[0; 4]),
            |_, id| encode_app(APP_STDOUT, id, 0, &[0; 4]),
        ];
        for answer in answers {
            let mut session = attached(answer);
            assert!(matches!(
                session.features(deadline()),
                Err(SessionError::Protocol(_))
            ));
        }
    }

    #[test]
    fn expired_deadlines_time_out() {
        let (transport, _) = scripted(&[]);
        let error = ControlSession::attach(transport, &[1; CAPABILITY_LEN], Instant::now())
            .err()
            .unwrap();
        assert!(matches!(error, SessionError::TimedOut));
    }
}
