use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::protocol::nvxs::{ErrorCode, Identity};
use crate::protocol::{
    OUTER_DATA, OUTER_ERROR, OUTER_HEADER_LEN, OUTER_HOST_ATTACH, OUTER_READY, OUTER_RESET,
    OUTER_WAIT, Workload, WorkloadEnvironment, decode_outer_header, encode_outer,
};

#[cfg(any(target_os = "linux", windows))]
mod managed_lifecycle;

const INSTANCE: [u8; 16] = [7; 16];

/// The codes of the kinds of requests that version 2 shares with version 1, as its header holds
/// them.
const PING: u8 = 0x01;
const EXEC: u8 = 0x02;
const STOP: u8 = 0x03;
const CANCEL: u8 = 0x04;
const MAPS: u8 = 0x06;
const CAPTURE: u8 = 0x07;
const ACTIVATE: u8 = 0x08;
const STDIN: u8 = 0x09;
const STDIN_END: u8 = 0x0a;

/// Returns the bytes of `frame` with `request_id`.
fn encode(request_id: u64, frame: Frame<'_>) -> Vec<u8> {
    frame.encode(request_id).unwrap()
}

/// Returns a `READY` of the protocol version for `request_id`, which reports `activation`.
fn ready_frame(request_id: u64, activation: &[u8]) -> Vec<u8> {
    encode(
        request_id,
        Frame::Ready {
            version: v2::VERSION,
            activation,
        },
    )
}

/// Returns the kind, request ID and payload of a request that the host sent.
fn request(frame: &[u8]) -> (u8, u64, Vec<u8>) {
    let (request_id, _) = Frame::decode(frame).unwrap();
    (frame[5], request_id, frame[24..].to_vec())
}

/// Returns a workload of `argv`, without a timeout, in the default environment.
fn workload(argv: &[&str]) -> Vec<u8> {
    protocol::encode_exec_payload(&Workload {
        argv: argv.iter().map(|&argument| argument.to_owned()).collect(),
        timeout_ms: 0,
        cwd: None,
        environment: WorkloadEnvironment::Default,
    })
    .unwrap()
}

#[test]
fn the_legacy_connect_wrapper_keeps_indeterminate_process_failures_as_io() {
    let error = Error::io("cannot verify the VM process").with_source(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "the original process query failure",
    ));
    let error = connect_failure(error);
    let SessionError::Io(source) = error else {
        panic!("an indeterminate process check is not an endpoint refusal: {error}");
    };
    assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(source.to_string(), "the original process query failure");
}

/// What a test shares with a [`Scripted`] transport.
#[derive(Default)]
struct Wire {
    /// Bytes for the host to read.
    input: VecDeque<u8>,
    /// Whether the stream ends once the host read `input`.
    closed: bool,
    /// Bytes that the host wrote.
    output: Vec<u8>,
}

/// Transport of a scripted guest, whose records the test supplies at any time. A read
/// without input times out, or ends the stream once the test closed it.
#[derive(Clone, Default)]
struct Scripted(Arc<Mutex<Wire>>);

impl Scripted {
    /// Supplies records for the host to read.
    fn supply(&self, records: &[Vec<u8>]) {
        self.0.lock().unwrap().input.extend(records.concat());
    }

    /// Ends the stream after the records that the test supplied.
    fn close(&self) {
        self.0.lock().unwrap().closed = true;
    }

    /// Returns the bytes that the host wrote.
    fn output(&self) -> Vec<u8> {
        self.0.lock().unwrap().output.clone()
    }
}

impl Transport for Scripted {
    fn read(&mut self, buffer: &mut [u8], _timeout: Option<Duration>) -> io::Result<usize> {
        let mut wire = self.0.lock().unwrap();
        if wire.input.is_empty() {
            return if wire.closed {
                Ok(0)
            } else {
                Err(io::ErrorKind::TimedOut.into())
            };
        }
        let count = buffer.len().min(wire.input.len());
        for (byte, input) in buffer.iter_mut().zip(wire.input.drain(..count)) {
            *byte = input;
        }
        Ok(count)
    }

    fn write_all(&mut self, data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
        self.0.lock().unwrap().output.extend_from_slice(data);
        Ok(())
    }
}

fn outer(record_type: u8, epoch: u64, sequence: u64, payload: &[u8]) -> Vec<u8> {
    let instance = if epoch == 0 { [0; 16] } else { INSTANCE };
    encode_outer(record_type, &instance, epoch, sequence, payload).unwrap()
}

/// Returns `record` with `bytes` at `offset`.
fn patched(mut record: Vec<u8>, offset: usize, bytes: &[u8]) -> Vec<u8> {
    record[offset..offset + bytes.len()].copy_from_slice(bytes);
    record
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

/// Attaches through a scripted guest that supplied `records`, and returns the attachment
/// with the guest's transport.
fn scripted(records: &[Vec<u8>]) -> (Result<ControlSession, SessionError>, Scripted) {
    let wire = Scripted::default();
    wire.supply(records);
    let session = ControlSession::attach(Box::new(wire.clone()), &[1; CAPABILITY_LEN], deadline());
    (session, wire)
}

/// Attaches through a scripted guest whose broker attaches the host at its sequence 0, in
/// epoch 1.
fn ready() -> (ControlSession, Scripted) {
    let (session, wire) = scripted(&[outer(OUTER_READY, 1, 0, &[])]);
    (session.unwrap(), wire)
}

/// Kind and payload of each application request that a guest received.
type Requests = Arc<Mutex<Vec<(u8, Vec<u8>)>>>;

/// The frames with which a guest answers a request, from its kind and request ID.
type Answer = Box<dyn Fn(u8, u64) -> Vec<Vec<u8>> + Send>;

/// The frame with which a guest answers a request, from its kind and request ID.
type Reply = fn(u8, u64) -> Vec<u8>;

/// Transport of a guest that answers each application request with the frames that `answer`
/// returns for it, each in a record, and records the kind and payload of each request.
struct Responder {
    input: VecDeque<u8>,
    next_sequence: u64,
    answer: Answer,
    requests: Requests,
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
            let (kind, request_id, payload) = request(&data[OUTER_HEADER_LEN..]);
            self.requests.lock().unwrap().push((kind, payload));
            for reply in (self.answer)(kind, request_id) {
                self.input
                    .extend(outer(OUTER_DATA, 1, self.next_sequence, &reply));
                self.next_sequence += 1;
            }
        }
        Ok(())
    }
}

/// Attaches to a guest that answers each application request with `answer`.
fn attached(answer: fn(u8, u64) -> Vec<u8>) -> ControlSession {
    attached_recording(answer).0
}

/// Attaches to a guest that answers each application request with `answer`, and returns the
/// kind and payload of each request that the guest receives.
fn attached_recording(answer: fn(u8, u64) -> Vec<u8>) -> (ControlSession, Requests) {
    responding(Box::new(move |kind, request_id| {
        vec![answer(kind, request_id)]
    }))
}

/// Attaches to a guest that answers each application request with the frames that `answer`
/// returns, and returns the kind and payload of each request that the guest receives.
fn responding(answer: Answer) -> (ControlSession, Requests) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let guest = Responder {
        input: outer(OUTER_READY, 1, 0, &[]).into(),
        next_sequence: 1,
        answer,
        requests: Arc::clone(&requests),
    };
    let session =
        ControlSession::attach(Box::new(guest), &[1; CAPABILITY_LEN], deadline()).unwrap();
    (session, requests)
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

/// Checks that `error` is a protocol violation with `message`.
#[track_caller]
fn assert_violation(error: &SessionError, message: &str, case: &str) {
    let matched = matches!(error, SessionError::Protocol(text) if text == message);
    assert!(matched, "{case}: {error:?}");
}

#[test]
fn attach_waits_for_the_guest_then_authenticates() {
    // The broker asks the host to wait while the guest has not acknowledged the session.
    let (session, wire) = scripted(&[outer(OUTER_WAIT, 0, 0, &[]), outer(OUTER_READY, 4, 1, &[])]);
    let session = session.unwrap();
    let identity = Identity {
        instance_id: INSTANCE,
        epoch: 4,
    };
    assert_eq!(session.connection.identity(), identity);
    let records = host_records(&wire.output());
    assert_eq!(records, [(OUTER_HOST_ATTACH, 0, vec![1; CAPABILITY_LEN])]);
}

#[test]
fn attach_reports_authentication_failure() {
    let code = ErrorCode::Authentication.code().to_le_bytes();
    let (session, _) = scripted(&[outer(OUTER_ERROR, 0, 0, &code)]);
    assert!(matches!(session, Err(SessionError::AuthenticationFailed)));
}

#[test]
fn attach_responses_that_the_protocol_does_not_allow_fail_closed() {
    let ready = outer(OUTER_READY, 1, 0, &[]);
    let oversized = u32::try_from(OUTER_MAX_PAYLOAD + 1).unwrap().to_le_bytes();
    let payload = "control attach response carried a payload";
    let invalid = "control endpoint returned an invalid attach response";
    let outer_record = "control endpoint returned an invalid outer record";
    let cases = [
        (
            "a READY with a payload",
            outer(OUTER_READY, 1, 0, b"x"),
            payload,
        ),
        (
            "a WAIT with a payload",
            outer(OUTER_WAIT, 0, 0, b"x"),
            payload,
        ),
        (
            "a READY without an identity",
            outer(OUTER_READY, 0, 0, &[]),
            invalid,
        ),
        (
            "a READY out of sequence",
            outer(OUTER_READY, 1, 3, &[]),
            invalid,
        ),
        ("DATA before READY", outer(OUTER_DATA, 1, 0, b"x"), invalid),
        (
            "an ERROR without a code",
            outer(OUTER_ERROR, 0, 0, &[]),
            invalid,
        ),
        (
            "an ERROR with an unknown code",
            outer(OUTER_ERROR, 0, 0, &2u32.to_le_bytes()),
            invalid,
        ),
        ("an unknown record type", outer(0x7f, 1, 0, &[]), invalid),
        (
            "a wrong magic",
            patched(ready.clone(), 0, b"NVXT"),
            outer_record,
        ),
        (
            "another version",
            patched(ready.clone(), 4, &2u16.to_le_bytes()),
            outer_record,
        ),
        (
            "nonzero flags",
            patched(ready.clone(), 7, &[1]),
            outer_record,
        ),
        (
            "an oversized record",
            patched(ready.clone(), 40, &oversized),
            "control endpoint returned an oversized outer record",
        ),
    ];
    for (case, record, message) in cases {
        let error = scripted(&[record]).0.err().unwrap();
        assert_violation(&error, message, case);
    }

    // A stream that ends, at a record boundary or inside a record, has closed.
    for records in [Vec::new(), vec![ready[..20].to_vec()]] {
        let wire = Scripted::default();
        wire.supply(&records);
        wire.close();
        let attached = ControlSession::attach(Box::new(wire), &[1; CAPABILITY_LEN], deadline());
        assert!(matches!(attached, Err(SessionError::Closed)));
    }
}

#[test]
fn exec_streams_events_in_order() {
    // The scripted guest answers the frames of the execution that the host started, whose output
    // passes the old cap of version 1, 1 MiB, in frames that each fill a record.
    let (mut session, wire) = ready();
    let payload = workload(&["/bin/sh", "-c", "echo hi"]);
    session.start_exec(9, &payload, deadline()).unwrap();
    let records = host_records(&wire.output());
    let (record_type, sequence, frame) = &records[1];
    assert_eq!((*record_type, *sequence), (OUTER_DATA, 0));
    assert_eq!(request(frame), (EXEC, 9, payload));

    let chunk = vec![b'x'; v2::MAX_PAYLOAD];
    let chunks = 1024 * 1024 / v2::MAX_PAYLOAD + 1;
    let mut records: Vec<Vec<u8>> = (1..=chunks)
        .map(|sequence| {
            outer(
                OUTER_DATA,
                1,
                sequence as u64,
                &encode(9, Frame::Stdout(&chunk)),
            )
        })
        .collect();
    let sequence = chunks as u64;
    records.extend([
        outer(
            OUTER_DATA,
            1,
            sequence + 1,
            &encode(9, Frame::Stderr(b"warn")),
        ),
        outer(
            OUTER_DATA,
            1,
            sequence + 2,
            &encode(
                9,
                Frame::Exit {
                    status: 3,
                    outcome: Outcome::Exit,
                },
            ),
        ),
    ]);
    wire.supply(&records);
    let mut stdout = 0;
    let events = loop {
        match session.next_exec_event(9, Some(deadline())).unwrap() {
            ExecEvent::Stdout(data) => stdout += data.len(),
            event => {
                break [event, session.next_exec_event(9, Some(deadline())).unwrap()];
            }
        }
    };
    assert_eq!(stdout, chunks * v2::MAX_PAYLOAD);
    assert!(stdout > 1024 * 1024);
    assert_eq!(
        events,
        [
            ExecEvent::Stderr(b"warn".to_vec()),
            ExecEvent::Exit {
                category: Outcome::Exit,
                status: 3
            },
        ]
    );
}

#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn exec_sends_the_working_directory_and_reports_its_refusal() {
    let (mut session, wire) = ready();
    let payload = protocol::encode_exec_payload(&Workload {
        argv: vec!["/bin/pwd".to_owned()],
        timeout_ms: 0,
        cwd: Some("/work"),
        environment: WorkloadEnvironment::Default,
    })
    .unwrap();
    session.start_exec(5, &payload, deadline()).unwrap();
    let records = host_records(&wire.output());
    assert_eq!(request(&records[1].2), (EXEC, 5, payload));

    let diagnostic = b"nvx-managed-agent: cannot enter working directory /work: No such file\n";
    wire.supply(&[
        outer(OUTER_DATA, 1, 1, &encode(5, Frame::Stderr(diagnostic))),
        outer(
            OUTER_DATA,
            1,
            2,
            &encode(
                5,
                Frame::Error {
                    status: 2,
                    category: protocol::CWD_FAILED.as_bytes(),
                },
            ),
        ),
    ]);
    let events: Vec<ExecEvent> = (0..2)
        .map(|_| session.next_exec_event(5, Some(deadline())).unwrap())
        .collect();
    assert_eq!(
        events,
        [
            ExecEvent::Stderr(diagnostic.to_vec()),
            ExecEvent::Rejected {
                status: 2,
                category: protocol::CWD_FAILED.to_owned()
            },
        ]
    );
}

#[test]
fn standard_input_keeps_at_most_the_window_unacknowledged() {
    let (mut session, wire) = ready();
    session
        .start_exec(7, &workload(&["/bin/cat"]), deadline())
        .unwrap();
    // A frame holds what a record does, and the window the rest of its 64 KiB.
    let data = vec![b'i'; 100 * 1024];
    let first = session.send_stdin(7, &data, deadline()).unwrap();
    assert_eq!(first, v2::MAX_PAYLOAD);
    let second = session.send_stdin(7, &data[first..], deadline()).unwrap();
    assert_eq!(first + second, STDIN_WINDOW as usize);
    assert_eq!(session.send_stdin(7, &data, deadline()).unwrap(), 0);
    // An acknowledgement returns its bytes to the window, and no more than it.
    wire.supply(&[outer(OUTER_DATA, 1, 1, &encode(7, Frame::StdinAck(1000)))]);
    assert_eq!(
        session.next_exec_event(7, Some(deadline())).unwrap(),
        ExecEvent::Acknowledged(1000)
    );
    assert_eq!(session.send_stdin(7, &data, deadline()).unwrap(), 1000);
    assert_eq!(session.send_stdin(7, &data, deadline()).unwrap(), 0);
    session.close_stdin(7, deadline()).unwrap();
    let frames: Vec<_> = host_records(&wire.output())[2..]
        .iter()
        .map(|(_, _, frame)| request(frame))
        .collect();
    assert_eq!(
        frames,
        [
            (STDIN, 7, data[..first].to_vec()),
            (STDIN, 7, data[..second].to_vec()),
            (STDIN, 7, data[..1000].to_vec()),
            (STDIN_END, 7, Vec::new()),
        ]
    );
    // A new execution starts with its whole window, and the guest agent acknowledges no bytes
    // that the host did not send.
    session
        .start_exec(8, &workload(&["/bin/cat"]), deadline())
        .unwrap();
    assert_eq!(session.send_stdin(8, &data[..10], deadline()).unwrap(), 10);
    wire.supply(&[outer(OUTER_DATA, 1, 2, &encode(8, Frame::StdinAck(11)))]);
    assert_violation(
        &session.next_exec_event(8, Some(deadline())).unwrap_err(),
        "the guest agent acknowledged standard input that it did not receive",
        "an acknowledgement beyond the bytes sent",
    );
}

#[test]
fn records_that_the_protocol_does_not_allow_fail_closed() {
    let frame = encode(1, Frame::Stdout(b"x"));
    let data = outer(OUTER_DATA, 1, 1, &frame);
    let oversized = u32::try_from(OUTER_MAX_PAYLOAD + 1).unwrap().to_le_bytes();
    let invalid = "control endpoint returned an invalid data record";
    let truncated = "control endpoint returned a truncated application frame";
    let malformed = "the NVXC frame of version 2 is malformed";
    let cases = [
        (
            "DATA out of sequence",
            outer(OUTER_DATA, 1, 5, &frame),
            invalid,
        ),
        (
            "a repeated sequence number",
            outer(OUTER_DATA, 1, 0, &frame),
            invalid,
        ),
        (
            "DATA of another epoch",
            outer(OUTER_DATA, 2, 1, &frame),
            invalid,
        ),
        (
            "DATA of another instance",
            encode_outer(OUTER_DATA, &[9; 16], 1, 1, &frame).unwrap(),
            invalid,
        ),
        ("a WAIT", outer(OUTER_WAIT, 1, 1, &[]), invalid),
        (
            "a RESET of the session's epoch",
            outer(OUTER_RESET, 1, 1, &[]),
            invalid,
        ),
        ("an unknown record type", outer(0x7f, 1, 1, &[]), invalid),
        (
            "an empty DATA record",
            outer(OUTER_DATA, 1, 1, &[]),
            truncated,
        ),
        (
            "a truncated frame",
            outer(OUTER_DATA, 1, 1, &frame[..23]),
            malformed,
        ),
        (
            "a frame of version 1",
            outer(OUTER_DATA, 1, 1, &patched(frame.clone(), 4, &[1])),
            "the control peer sent a frame of another protocol than NVXC version 2",
        ),
        (
            "nonzero flags",
            patched(data.clone(), 7, &[1]),
            "control endpoint returned an invalid outer record",
        ),
        (
            "an oversized record",
            patched(data.clone(), 40, &oversized),
            "control endpoint returned an oversized outer record",
        ),
    ];
    for (case, record, message) in cases {
        let (mut session, wire) = ready();
        wire.supply(&[record]);
        let error = session.next_exec_event(1, Some(deadline())).unwrap_err();
        assert_violation(&error, message, case);
    }

    // A violation ends the session.
    let (mut session, wire) = ready();
    wire.supply(&[outer(OUTER_DATA, 1, 5, &frame)]);
    assert!(session.next_exec_event(1, Some(deadline())).is_err());
    wire.supply(std::slice::from_ref(&data));
    let error = session.next_exec_event(1, Some(deadline())).unwrap_err();
    let ended = "the control session ended after an earlier failure";
    assert_violation(&error, ended, "a call after a violation");
    let error = session.cancel_exec(1, deadline()).unwrap_err();
    assert_violation(&error, ended, "a request after a violation");

    // A reset to a later epoch ends the session too.
    let (mut session, wire) = ready();
    wire.supply(&[outer(OUTER_RESET, 2, 1, &[])]);
    assert!(matches!(
        session.next_exec_event(1, Some(deadline())),
        Err(SessionError::Reset)
    ));

    // A stream that ends, at a record boundary or inside a record, has closed.
    for records in [Vec::new(), vec![data[..OUTER_HEADER_LEN + 1].to_vec()]] {
        let (mut session, wire) = ready();
        wire.supply(&records);
        wire.close();
        assert!(matches!(
            session.next_exec_event(1, Some(deadline())),
            Err(SessionError::Closed)
        ));
    }
}

#[test]
fn timed_out_polls_keep_partial_records() {
    let (mut session, wire) = ready();
    let exit = Frame::Exit {
        status: 0,
        outcome: Outcome::Cancelled,
    };
    let record = outer(OUTER_DATA, 1, 1, &encode(9, exit));
    let (first, second) = record.split_at(30);
    wire.supply(&[first.to_vec()]);
    assert!(matches!(
        session.next_exec_event(9, Some(deadline())),
        Err(SessionError::TimedOut)
    ));
    session.cancel_exec(9, deadline()).unwrap();
    wire.supply(&[second.to_vec()]);
    assert_eq!(
        session.next_exec_event(9, Some(deadline())).unwrap(),
        ExecEvent::Exit {
            category: Outcome::Cancelled,
            status: 0
        }
    );
    let records = host_records(&wire.output());
    assert_eq!(request(&records.last().unwrap().2), (CANCEL, 9, Vec::new()));
}

#[test]
fn frames_travel_in_one_record_each() {
    let (mut session, wire) = ready();
    let longest = vec![0x5a; v2::MAX_PAYLOAD];
    session.start_exec(1, &longest, deadline()).unwrap();
    let error = session
        .start_exec(2, &[longest.as_slice(), &[0]].concat(), deadline())
        .unwrap_err();
    let message = "an NVXC frame must fit one outer data record";
    assert_violation(&error, message, "a frame longer than a record");
    let records = host_records(&wire.output());
    assert_eq!(records.len(), 2);
    let (record_type, sequence, frame) = &records[1];
    assert_eq!((*record_type, *sequence), (OUTER_DATA, 0));
    assert_eq!(request(frame), (EXEC, 1, longest));
    // The refused frame changed nothing.
    session.cancel_exec(1, deadline()).unwrap();
    let records = host_records(&wire.output());
    assert_eq!((records[2].0, records[2].1), (OUTER_DATA, 1));
}

/// Returns the frame that acknowledges a ping or a stop request of `request_id`.
fn acknowledgement(kind: u8, request_id: u64) -> Vec<u8> {
    if kind == PING {
        ready_frame(request_id, &[])
    } else {
        encode(request_id, Frame::Stopped)
    }
}

#[test]
fn pings_and_stops_need_an_empty_acknowledgement() {
    let (mut session, requests) = attached_recording(acknowledgement);
    session.ping(deadline()).unwrap();
    session.stop(deadline()).unwrap();
    assert_eq!(
        *requests.lock().unwrap(),
        [(PING, Vec::new()), (STOP, Vec::new())]
    );
    let readiness = "the guest agent did not acknowledge readiness";
    let stop = "the guest agent did not acknowledge stop";
    let malformed = "the NVXC frame of version 2 is malformed";
    // A refusal, another request's acknowledgement, one with a status, and one of the other
    // request or with a payload.
    let answers: [(Reply, &str, &str); 4] = [
        (|_, id| encode(id, Frame::Busy), readiness, stop),
        (|kind, id| acknowledgement(kind, id ^ 1), readiness, stop),
        (
            |kind, id| patched(acknowledgement(kind, id), 16, &[1]),
            malformed,
            malformed,
        ),
        (
            |kind, id| {
                if kind == PING {
                    ready_frame(id, b"x")
                } else {
                    ready_frame(id, &[])
                }
            },
            readiness,
            stop,
        ),
    ];
    for (answer, pinged, stopped) in answers {
        let error = attached(answer).ping(deadline()).unwrap_err();
        assert_violation(&error, pinged, "a ping");
        let error = attached(answer).stop(deadline()).unwrap_err();
        assert_violation(&error, stopped, "a stop");
    }
}

#[test]
fn ready_must_carry_the_protocol_version() {
    let maps = [HostMap {
        source: "0".to_owned(),
        target: "/work".to_owned(),
        read_only: true,
    }];
    let answers: [(Reply, u8); 2] = [
        (
            |_, id| {
                let frame = Frame::Ready {
                    version: 1,
                    activation: &[],
                };
                encode(id, frame)
            },
            1,
        ),
        (
            |_, id| {
                let frame = Frame::Ready {
                    version: 3,
                    activation: &[],
                };
                encode(id, frame)
            },
            3,
        ),
    ];
    for (answer, version) in answers {
        let message = format!("the guest agent speaks NVXC version {version}, not version 2");
        let error = attached(answer).ping(deadline()).unwrap_err();
        assert_violation(&error, &message, "a ping");
        let error = attached(answer).capture(false, deadline()).unwrap_err();
        assert_violation(&error, &message, "a capture");
        let error = attached(answer)
            .map_host_paths(&maps, deadline())
            .unwrap_err();
        assert_violation(&error, &message, "a mapping table");
        let error = attached(answer)
            .activate(b"activation", deadline())
            .unwrap_err();
        assert_violation(&error, &message, "an activation");
    }
    // A guest agent that predates version 2 answers in version 1's frames.
    let error = attached(|_, id| {
        [
            b"NVXC".as_slice(),
            &[1, 0x81, 0, 0],
            &id.to_le_bytes(),
            &[0; 8],
        ]
        .concat()
    })
    .ping(deadline())
    .unwrap_err();
    let message = "the control peer sent a frame of another protocol than NVXC version 2";
    assert_violation(&error, message, "a READY of version 1");
}

#[test]
fn a_stop_ends_the_execution_that_runs_and_skips_its_frames() {
    // The guest agent ends the execution, whose output and end come before the acknowledgement.
    let (mut session, requests) = responding(Box::new(|kind, id| match kind {
        STOP => vec![
            encode(4, Frame::Stdout(b"partial")),
            encode(4, Frame::StdinAck(1)),
            encode(
                4,
                Frame::Exit {
                    status: 137,
                    outcome: Outcome::Cancelled,
                },
            ),
            encode(id, Frame::Stopped),
        ],
        _ => Vec::new(),
    }));
    session
        .start_exec(4, &workload(&["/bin/cat"]), deadline())
        .unwrap();
    assert_eq!(session.send_stdin(4, b"i", deadline()).unwrap(), 1);
    session.stop(deadline()).unwrap();
    let kinds: Vec<u8> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|(kind, _)| *kind)
        .collect();
    assert_eq!(kinds, [EXEC, STDIN, STOP]);
    // An execution that the guest agent refused ends with its error, which comes before the
    // acknowledgement too.
    let (mut session, _) = responding(Box::new(|kind, id| match kind {
        STOP => vec![
            encode(5, Frame::Stderr(b"cannot enter /missing")),
            encode(
                5,
                Frame::Error {
                    status: 2,
                    category: b"cwd-failed",
                },
            ),
            encode(id, Frame::Stopped),
        ],
        _ => Vec::new(),
    }));
    session
        .start_exec(5, &workload(&["/bin/pwd"]), deadline())
        .unwrap();
    session.stop(deadline()).unwrap();
    // A frame of the stop's own ID other than its acknowledgement is refused, its refusal too.
    let (mut session, _) = responding(Box::new(|_, id| vec![encode(id, Frame::Stdout(b"x"))]));
    assert_violation(
        &session.stop(deadline()).unwrap_err(),
        "the guest agent did not acknowledge stop",
        "output of the stop's own ID",
    );
    let (mut session, _) = responding(Box::new(|_, id| {
        vec![encode(
            id,
            Frame::Error {
                status: 1,
                category: b"invalid-request",
            },
        )]
    }));
    assert_violation(
        &session.stop(deadline()).unwrap_err(),
        "the guest agent did not acknowledge stop",
        "a refusal of the stop",
    );
}

#[test]
fn capture_sends_both_kill_flags_and_waits_for_acknowledgement() {
    let (mut session, requests) = attached_recording(|kind, id| {
        assert_eq!(kind, CAPTURE);
        assert_ne!(id, 0);
        ready_frame(id, &[])
    });
    session.capture(false, deadline()).unwrap();
    session.capture(true, deadline()).unwrap();
    assert_eq!(
        *requests.lock().unwrap(),
        [(CAPTURE, vec![0]), (CAPTURE, vec![1])]
    );
}

#[test]
fn capture_refuses_invalid_acknowledgements() {
    let answers: [fn(u8, u64) -> Vec<u8>; 4] = [
        |_, id| encode(id, Frame::Busy),
        |_, id| encode(id, Frame::Stopped),
        |_, id| ready_frame(id ^ 1, &[]),
        |_, id| ready_frame(id, b"x"),
    ];
    for answer in answers {
        let error = attached(answer).capture(false, deadline()).unwrap_err();
        assert_violation(
            &error,
            "the guest agent did not acknowledge capture",
            "an invalid capture acknowledgement",
        );
    }
    // Version 2 has no status in a READY.
    let error = attached(|_, id| patched(ready_frame(id, &[]), 16, &[1]))
        .capture(false, deadline())
        .unwrap_err();
    assert_violation(
        &error,
        "the NVXC frame of version 2 is malformed",
        "a READY with a status",
    );
}

#[cfg(feature = "cli")]
#[test]
fn activate_sends_its_payload_and_returns_the_activation_that_ready_reports() {
    let (mut session, requests) = attached_recording(|kind, id| {
        assert_eq!(kind, ACTIVATE);
        ready_frame(id, &[[1].as_slice(), &[9; 16]].concat())
    });
    let ready = session.activate(b"activation", deadline()).unwrap();
    assert_eq!(
        ready.transaction,
        Some(crate::guest_restore::TransactionId::from_bytes([9; 16]))
    );
    assert_eq!(
        *requests.lock().unwrap(),
        [(ACTIVATE, b"activation".to_vec())]
    );
}

#[cfg(feature = "cli")]
#[test]
fn activate_refuses_a_reply_other_than_a_valid_ready() {
    let answers: [fn(u8, u64) -> Vec<u8>; 4] = [
        |_, id| {
            let frame = Frame::Error {
                status: 22,
                category: b"invalid-request",
            };
            encode(id, frame)
        },
        |_, id| ready_frame(id ^ 1, &[0]),
        |_, id| patched(ready_frame(id, &[0]), 16, &[1]),
        |_, id| ready_frame(id, &[2]),
    ];
    for answer in answers {
        let error = attached(answer)
            .activate(b"activation", deadline())
            .unwrap_err();
        assert!(matches!(error, SessionError::Protocol(_)), "{error:?}");
    }
}

#[test]
fn host_mappings_travel_in_records_that_the_guest_acknowledges() {
    let (mut session, requests) = attached_recording(|kind, id| {
        assert_eq!(kind, MAPS);
        ready_frame(id, &[])
    });
    let target = format!("/{}", "t".repeat(4000));
    let maps: Vec<HostMap> = (0..40)
        .map(|index| HostMap {
            source: format!("0/{index}"),
            target: target.clone(),
            read_only: index % 2 == 0,
        })
        .collect();
    session.map_host_paths(&maps, deadline()).unwrap();
    // Entries of about 4 KiB each fill a 64 KiB record 16 at a time.
    let requests = requests.lock().unwrap();
    let positions: Vec<(u32, u32)> = requests
        .iter()
        .map(|(_, payload)| {
            (
                u32::from_le_bytes(payload[..4].try_into().unwrap()),
                u32::from_le_bytes(payload[4..8].try_into().unwrap()),
            )
        })
        .collect();
    assert_eq!(positions, [(0, 16), (16, 16), (32, 8)]);
}

#[test]
fn refused_host_mappings_fail_the_session() {
    let maps = [HostMap {
        source: "0".to_owned(),
        target: "/work".to_owned(),
        read_only: false,
    }];
    let mut session = attached(|_, id| {
        let frame = Frame::Error {
            status: 2,
            category: b"mapping-failed",
        };
        encode(id, frame)
    });
    let error = session.map_host_paths(&maps, deadline()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("could not mount a mapped host path (error 2)"),
        "{error}"
    );
    let answers: [fn(u8, u64) -> Vec<u8>; 5] = [
        |_, id| {
            let frame = Frame::Error {
                status: 22,
                category: b"invalid-request",
            };
            encode(id, frame)
        },
        |_, id| {
            let frame = Frame::Error {
                status: 95,
                category: b"unsupported-operation",
            };
            encode(id, frame)
        },
        |_, id| encode(id, Frame::Busy),
        |_, id| ready_frame(id, b"x"),
        |_, id| ready_frame(id ^ 1, &[]),
    ];
    for answer in answers {
        let mut session = attached(answer);
        assert!(matches!(
            session.map_host_paths(&maps, deadline()),
            Err(SessionError::Protocol(_))
        ));
    }
}

#[test]
fn responses_to_another_request_fail_closed() {
    let mismatched = "the guest agent returned a mismatched request ID";
    let maps = [HostMap {
        source: "0".to_owned(),
        target: "/work".to_owned(),
        read_only: true,
    }];
    let error = attached(|_, id| ready_frame(id ^ 1, &[]))
        .map_host_paths(&maps, deadline())
        .unwrap_err();
    assert_eq!(error.to_string(), mismatched);
    let mut session = attached(|_, id| encode(id ^ 1, Frame::Stdout(b"x")));
    session
        .start_exec(6, &workload(&["/bin/true"]), deadline())
        .unwrap();
    let error = session.next_exec_event(6, Some(deadline())).unwrap_err();
    assert_eq!(error.to_string(), mismatched);
}

#[test]
fn expired_deadlines_time_out() {
    let wire = Scripted::default();
    let error = ControlSession::attach(Box::new(wire), &[1; CAPABILITY_LEN], Instant::now())
        .err()
        .unwrap();
    assert!(matches!(error, SessionError::TimedOut));
}
