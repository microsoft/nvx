//! The tests of the former `scripts/test_control_session.py`, on in-memory streams and on the
//! endpoints of each host, except those that the tests of [`crate::host::session`] cover.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError, TrySendError};
use std::thread;
use std::time::Instant;

use super::*;
use crate::protocol::{
    EXEC_CWD_PRESENT, EXEC_ENVIRONMENT_PRESENT, EXEC_EXTENDED, EXEC_INHERIT_DEFAULT_ENV,
    OUTER_DATA, OUTER_HEADER_LEN, OUTER_HOST_ATTACH, OUTER_READY, OUTER_RESET, decode_app,
    decode_outer_header, encode_app, encode_outer,
};

const CAPABILITY: [u8; CAPABILITY_LEN] = [7; CAPABILITY_LEN];
const INSTANCE: [u8; 16] = [0x11; 16];

/// What a [`Recording`] transport records: the direction and timeout of each transfer.
type Transfers = Arc<Mutex<Vec<(&'static str, Option<Duration>)>>>;

/// One end of an in-memory stream, like one socket of `socket.socketpair()`: it reads what the
/// other end writes, reads the end of the stream once the other end is gone, and holds at most
/// `capacity` unread writes, after which a write waits for the reader until its timeout.
struct End {
    incoming: mpsc::Receiver<Vec<u8>>,
    outgoing: mpsc::SyncSender<Vec<u8>>,
    buffer: VecDeque<u8>,
}

fn stream_pair(capacity: usize) -> (End, End) {
    let (to_guest, from_host) = mpsc::sync_channel(capacity);
    let (to_host, from_guest) = mpsc::sync_channel(64);
    (
        End {
            incoming: from_guest,
            outgoing: to_guest,
            buffer: VecDeque::new(),
        },
        End {
            incoming: from_host,
            outgoing: to_host,
            buffer: VecDeque::new(),
        },
    )
}

impl Transport for End {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        if self.buffer.is_empty() {
            let chunk = match timeout {
                Some(timeout) => self.incoming.recv_timeout(timeout),
                None => self
                    .incoming
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
            };
            match chunk {
                Ok(chunk) => self.buffer.extend(chunk),
                Err(RecvTimeoutError::Timeout) => return Err(io::ErrorKind::TimedOut.into()),
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let count = buffer.len().min(self.buffer.len());
        for (byte, value) in buffer.iter_mut().zip(self.buffer.drain(..count)) {
            *byte = value;
        }
        Ok(count)
    }

    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut chunk = data.to_vec();
        loop {
            match self.outgoing.try_send(chunk) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                Err(TrySendError::Full(returned)) => {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    chunk = returned;
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

/// The guest side of a session: OpenVMM's endpoint and the guest agent behind it.
struct Guest<T: Transport> {
    stream: T,
    sequence: u64,
}

impl<T: Transport> Guest<T> {
    fn new(stream: T) -> Self {
        Self {
            stream,
            sequence: 0,
        }
    }

    /// Reads `length` bytes, or returns `None` at the end of the stream.
    fn read_exact(&mut self, length: usize) -> Option<Vec<u8>> {
        let mut data = vec![0; length];
        let mut filled = 0;
        while filled < length {
            match self
                .stream
                .read(&mut data[filled..], Some(Duration::from_secs(10)))
            {
                Ok(0) | Err(_) => return None,
                Ok(count) => filled += count,
            }
        }
        Some(data)
    }

    /// Reads one outer record and returns its type and payload.
    fn record(&mut self) -> Option<(u8, Vec<u8>)> {
        let header = self.read_exact(OUTER_HEADER_LEN)?;
        let header = decode_outer_header(header.as_slice().try_into().unwrap()).unwrap();
        Some((header.record_type, self.read_exact(header.length)?))
    }

    /// Authenticates the host as OpenVMM's endpoint does.
    fn accept(&mut self) {
        let (record_type, payload) = self.record().unwrap();
        assert_eq!(
            (record_type, payload.as_slice()),
            (OUTER_HOST_ATTACH, &CAPABILITY[..])
        );
        let ready = encode_outer(OUTER_READY, &INSTANCE, 1, 0, &[]).unwrap();
        self.stream.write_all(&ready, None).unwrap();
    }

    /// Reads one application request and returns its kind, request ID, and payload.
    fn request(&mut self) -> (u8, u64, Vec<u8>) {
        let (record_type, frame) = self.record().unwrap();
        assert_eq!(record_type, OUTER_DATA);
        let frame = decode_app(&frame).unwrap();
        (frame.kind, frame.request_id, frame.payload.to_vec())
    }

    fn send(&mut self, kind: u8, request_id: u64, status: i32, payload: &[u8]) {
        self.sequence += 1;
        let frame = encode_app(kind, request_id, status, payload);
        let record = encode_outer(OUTER_DATA, &INSTANCE, 1, self.sequence, &frame).unwrap();
        self.stream.write_all(&record, None).unwrap();
    }
}

/// Returns a session attached over an in-memory stream to a guest that `serve` plays.
fn session_with(
    serve: impl FnOnce(&mut Guest<End>) + Send + 'static,
) -> (Session, thread::JoinHandle<()>) {
    let (host, guest) = stream_pair(64);
    let worker = thread::spawn(move || {
        let mut guest = Guest::new(guest);
        guest.accept();
        serve(&mut guest);
    });
    let session = Session::attach(Box::new(host), &CAPABILITY, 5.0).unwrap();
    (session, worker)
}

fn request(arguments: &[&str]) -> ExecRequest {
    ExecRequest {
        arguments: arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        timeout_ms: 0,
        cwd: None,
        environment: None,
        inherit_default_environment: false,
    }
}

/// Returns the message of a failure.
fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
    match result {
        Ok(value) => panic!("unexpected success: {value:?}"),
        Err(exit) => exit
            .message()
            .map_or_else(|| format!("{exit:?}"), str::to_owned),
    }
}

/// Returns the code and the message of a failure.
fn failure<T: std::fmt::Debug>(result: Result<T>) -> (ErrorCode, String) {
    match result {
        Ok(value) => panic!("unexpected success: {value:?}"),
        Err(exit) => match (exit.code(), exit.message()) {
            (Some(code), Some(message)) => (code, message.to_owned()),
            _ => panic!("not an error: {exit:?}"),
        },
    }
}

/// Returns the payload of the exec request of `workload`, which the guest answers.
fn exec_payload(workload: ExecRequest) -> Vec<u8> {
    let (sender, receiver) = mpsc::channel();
    let (mut session, worker) = session_with(move |guest| {
        let (kind, request_id, payload) = guest.request();
        assert_eq!(kind, APP_EXEC);
        sender.send(payload).unwrap();
        guest.send(APP_EXIT, request_id, 0, b"exit");
    });
    session.exec(&workload, 5.0).unwrap();
    worker.join().unwrap();
    receiver.recv().unwrap()
}

fn u16_at(payload: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(payload[offset..offset + 2].try_into().unwrap())
}

fn u32_at(payload: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap())
}

/// Returns the length-prefixed values that start at `offset`, and the offset after them.
fn values(payload: &[u8], mut offset: usize, count: usize) -> (Vec<Vec<u8>>, usize) {
    let mut values = Vec::new();
    for _ in 0..count {
        let length = u32_at(payload, offset) as usize;
        values.push(payload[offset + 4..offset + 4 + length].to_vec());
        offset += 4 + length;
    }
    (values, offset)
}

#[test]
fn exec_streams_output_and_returns_bounded_status() {
    let (mut session, worker) = session_with(|guest| {
        let (kind, request_id, payload) = guest.request();
        assert_eq!(kind, APP_EXEC);
        // The legacy request: the timeout, the argument count, and no extension.
        assert_eq!(
            (
                u32_at(&payload, 0),
                u16_at(&payload, 4),
                u16_at(&payload, 6)
            ),
            (5000, 3, 0)
        );
        guest.send(APP_STDOUT, request_id, 0, b"hello");
        guest.send(APP_STDERR, request_id, 0, b"warning");
        guest.send(APP_EXIT, request_id, 7, b"exit");
    });
    let mut workload = request(&["/bin/sh", "-c", "echo hello"]);
    workload.timeout_ms = 5000;
    let result = session.exec(&workload, 5.0).unwrap();
    worker.join().unwrap();
    assert_eq!(
        result,
        ExecResult {
            returncode: 7,
            category: "exit",
            stdout: b"hello".to_vec(),
            stderr: b"warning".to_vec(),
        }
    );
}

#[test]
fn exec_rejects_unbounded_or_relative_arguments() {
    let (mut session, worker) = session_with(|_| {});
    assert_eq!(
        message(session.exec(&request(&["relative"]), 1.0)),
        "managed exec entrypoint must be absolute"
    );
    assert_eq!(
        message(session.exec(&request(&["/bin/true"; 65]), 1.0)),
        "managed exec requires 1 through 64 arguments"
    );
    for argument in ["", "a\0b", &"a".repeat(4097)] {
        assert_eq!(
            message(session.exec(&request(&["/bin/true", argument]), 1.0)),
            "managed exec argument is empty or exceeds 4096 bytes"
        );
    }
    drop(session);
    worker.join().unwrap();
}

#[test]
fn exec_encodes_extended_cwd_and_environment_exactly() {
    let environment = ["EMPTY=", "SPACED=a b", "EQUALS=a=b", "UTF8=\u{2603}"];
    let cwd = "/tmp/space \u{2603}";
    let payload = exec_payload(ExecRequest {
        arguments: ["/bin/sh", "-c", "printf exact"]
            .map(str::to_owned)
            .to_vec(),
        timeout_ms: 0xFFFF_FFFF,
        cwd: Some(cwd.to_owned()),
        environment: Some(environment.map(str::to_owned).to_vec()),
        inherit_default_environment: false,
    });
    assert_eq!(
        (
            u32_at(&payload, 0),
            u16_at(&payload, 4),
            u16_at(&payload, 6),
            u16_at(&payload, 8),
            u16_at(&payload, 10),
            u32_at(&payload, 12) as usize
        ),
        (
            0xFFFF_FFFF,
            3,
            EXEC_EXTENDED,
            EXEC_CWD_PRESENT | EXEC_ENVIRONMENT_PRESENT,
            4,
            cwd.len()
        )
    );
    let (arguments, offset) = values(&payload, 16, 3);
    assert_eq!(arguments, [&b"/bin/sh"[..], b"-c", b"printf exact"]);
    assert_eq!(&payload[offset..offset + cwd.len()], cwd.as_bytes());
    let (entries, end) = values(&payload, offset + cwd.len(), 4);
    assert_eq!(entries, environment.map(|entry| entry.as_bytes().to_vec()));
    assert_eq!(end, payload.len());
}

fn environment_payload(environment: Option<&[&str]>, inherit: bool) -> Vec<u8> {
    let mut workload = request(&["/bin/true"]);
    workload.environment =
        environment.map(|entries| entries.iter().map(|entry| (*entry).to_owned()).collect());
    workload.inherit_default_environment = inherit;
    exec_payload(workload)
}

#[test]
fn exec_distinguishes_omitted_and_empty_environment() {
    let omitted = environment_payload(None, false);
    let empty = environment_payload(Some(&[]), false);
    assert_eq!(u16_at(&omitted, 6), 0);
    assert_eq!(u16_at(&empty, 6), EXEC_EXTENDED);
    assert_eq!(u16_at(&empty, 8), EXEC_ENVIRONMENT_PRESENT);
    assert_eq!(u16_at(&empty, 10), 0);
}

#[test]
fn exec_layers_only_a_supplied_environment_over_the_defaults() {
    let layered_flags = EXEC_ENVIRONMENT_PRESENT | EXEC_INHERIT_DEFAULT_ENV;
    let fields = |payload: &[u8]| {
        (
            u16_at(payload, 6),
            u16_at(payload, 8),
            u16_at(payload, 10),
            u32_at(payload, 12),
        )
    };
    let layered = environment_payload(Some(&["A=1"]), true);
    assert_eq!(fields(&layered), (EXEC_EXTENDED, layered_flags, 1, 0));
    assert!(layered.ends_with(b"\x03\x00\x00\x00A=1"));
    let empty = environment_payload(Some(&[]), true);
    assert_eq!(fields(&empty), (EXEC_EXTENDED, layered_flags, 0, 0));
    let replaced = environment_payload(Some(&["A=1"]), false);
    assert_eq!(u16_at(&replaced, 8), EXEC_ENVIRONMENT_PRESENT);
    // An omitted environment already is the default one, so the request stays the legacy one,
    // which the guest agent accepts without an environment.
    assert_eq!(
        environment_payload(None, true),
        environment_payload(None, false)
    );
}

#[test]
fn exec_accepts_full_uint32_timeout_range_without_waiting() {
    for timeout in [0_u32, 3_600_001, 86_400_000, 0xFFFF_FFFF] {
        let mut workload = request(&["/bin/true"]);
        workload.timeout_ms = timeout;
        let started = Instant::now();
        assert_eq!(u32_at(&exec_payload(workload), 0), timeout);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

/// Returns a session whose guest reports how many bytes reach it before the end of the stream.
fn silent_session() -> (Session, thread::JoinHandle<()>, mpsc::Receiver<usize>) {
    let (sender, receiver) = mpsc::channel();
    let (session, worker) = session_with(move |guest| {
        let mut buffer = [0u8; 64];
        let count = guest.stream.read(&mut buffer, None).unwrap_or(0);
        sender.send(count).unwrap();
    });
    (session, worker, receiver)
}

#[test]
fn exec_rejects_invalid_execution_fields_before_sending() {
    let (mut session, worker, received) = silent_session();
    for cwd in [
        "",
        "relative",
        "/bad\0path",
        &format!("/{}", "a".repeat(4096)),
    ] {
        let mut workload = request(&["/bin/true"]);
        workload.cwd = Some(cwd.to_owned());
        assert_eq!(
            failure(session.exec(&workload, 1.0)),
            (
                ErrorCode::Unsupported,
                "managed exec working directory must be an absolute path of at most 4096 bytes"
                    .to_owned()
            )
        );
    }
    let invalid = "managed exec environment entries must be non-empty KEY=VALUE strings of at \
                   most 4096 bytes";
    for (environment, expected) in [
        (&["MISSING_EQUALS"][..], invalid),
        (&["=empty-name"][..], invalid),
        (
            &["DUP=1", "DUP=2"][..],
            "managed exec environment contains duplicate key: DUP",
        ),
    ] {
        let mut workload = request(&["/bin/true"]);
        workload.environment = Some(
            environment
                .iter()
                .map(|entry| (*entry).to_owned())
                .collect(),
        );
        assert_eq!(message(session.exec(&workload, 1.0)), expected);
    }
    // Nothing reached the guest, which reads the end of the stream.
    drop(session);
    worker.join().unwrap();
    assert_eq!(received.recv().unwrap(), 0);
}

#[test]
fn exec_rejects_invalid_response_timeout_before_sending() {
    let (mut session, worker, received) = silent_session();
    for timeout in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            message(session.exec(&request(&["/bin/true"]), timeout)),
            "managed exec response timeout must be positive and finite"
        );
    }
    drop(session);
    worker.join().unwrap();
    assert_eq!(received.recv().unwrap(), 0);
}

#[test]
fn exec_rejects_unknown_exit_category() {
    // A cancellation, which this session never requests, is not among the categories.
    for category in [&b"guest-provided-detail"[..], b"cancelled"] {
        let (mut session, worker) = session_with(move |guest| {
            let (_, request_id, _) = guest.request();
            guest.send(APP_EXIT, request_id, 125, category);
        });
        assert_eq!(
            message(session.exec(&request(&["/bin/true"]), 5.0)),
            "managed guest returned an unsupported exit category"
        );
        worker.join().unwrap();
    }
    let (mut session, worker) = session_with(|guest| {
        let (_, request_id, _) = guest.request();
        guest.send(APP_EXIT, request_id, 0, b"\xffexit");
    });
    assert_eq!(
        message(session.exec(&request(&["/bin/true"]), 5.0)),
        "managed guest returned an invalid exit category"
    );
    worker.join().unwrap();
}

#[test]
fn exec_refusal_keeps_status_category_and_diagnostic() {
    let (mut session, worker) = session_with(|guest| {
        let (_, request_id, _) = guest.request();
        guest.send(
            APP_STDERR,
            request_id,
            0,
            b"nvx-managed-agent: cannot enter working directory /missing: No such file or \
              directory\n",
        );
        guest.send(APP_ERROR, request_id, 2, b"cwd-failed");
    });
    let mut workload = request(&["/bin/true"]);
    workload.cwd = Some("/missing".to_owned());
    assert_eq!(
        failure(session.exec(&workload, 5.0)),
        (
            ErrorCode::ExecRefused,
            "managed guest rejected exec (status=2, category=cwd-failed)".to_owned()
        )
    );
    worker.join().unwrap();
    // A category that is not ASCII is decoded with replacement characters.
    let (mut session, worker) = session_with(|guest| {
        let (_, request_id, _) = guest.request();
        guest.send(APP_ERROR, request_id, 5, b"bad\xff");
    });
    assert_eq!(
        message(session.exec(&request(&["/bin/true"]), 5.0)),
        "managed guest rejected exec (status=5, category=bad\u{fffd})"
    );
    worker.join().unwrap();
}

#[test]
fn session_reset_is_not_a_closed_endpoint() {
    // A reset comes from an OpenVMM that still serves the endpoint, so it must not look like the
    // closure with which OpenVMM shuts down.
    let (mut session, worker) = session_with(|guest| {
        let _ = guest.request();
        // The reset starts the session's next epoch, at the next sequence number of the host's
        // leg.
        let reset = encode_outer(OUTER_RESET, &INSTANCE, 2, guest.sequence + 1, &[]).unwrap();
        guest.stream.write_all(&reset, None).unwrap();
        let _ = guest.record();
    });
    let failure = session.ping(5.0).unwrap_err();
    assert!(
        matches!(
            &failure,
            Exit::Error { code: ErrorCode::Protocol, message }
                if message == "managed control session was reset"
        ),
        "{failure:?}"
    );
    drop(session);
    worker.join().unwrap();
}

/// A guest transport that answers each request at once, PING with READY, EXEC with an exit, and
/// STOP with STOPPED, records the direction and timeout of each transfer, and from the transfer
/// `from` on fails each transfer in the direction `failure` by timing out: the
/// `_RecordingStream` of the Python tests.
struct Recording {
    responses: VecDeque<u8>,
    sequence: u64,
    transfers: Transfers,
    failure: Option<(&'static str, usize)>,
}

impl Recording {
    fn new(failure: Option<(&'static str, usize)>) -> (Self, Transfers) {
        let transfers = Transfers::default();
        let transport = Self {
            responses: VecDeque::new(),
            sequence: 0,
            transfers: Arc::clone(&transfers),
            failure,
        };
        (transport, transfers)
    }

    fn transfer(&mut self, kind: &'static str, timeout: Option<Duration>) -> io::Result<()> {
        let mut transfers = self.transfers.lock().unwrap();
        let index = transfers.len();
        transfers.push((kind, timeout));
        match self.failure {
            Some((failing, from)) if failing == kind && index >= from => {
                Err(io::ErrorKind::TimedOut.into())
            }
            _ => Ok(()),
        }
    }
}

impl Transport for Recording {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        self.transfer("read", timeout)?;
        let count = buffer.len().min(self.responses.len());
        for (byte, value) in buffer.iter_mut().zip(self.responses.drain(..count)) {
            *byte = value;
        }
        Ok(count)
    }

    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        self.transfer("write", timeout)?;
        let header = decode_outer_header(data[..OUTER_HEADER_LEN].try_into().unwrap()).unwrap();
        let response = if header.record_type == OUTER_HOST_ATTACH {
            encode_outer(OUTER_READY, &INSTANCE, 1, 0, &[]).unwrap()
        } else {
            let request = decode_app(&data[OUTER_HEADER_LEN..]).unwrap();
            let (kind, payload) = match request.kind {
                APP_PING => (APP_READY, &b""[..]),
                APP_EXEC => (APP_EXIT, &b"exit"[..]),
                _ => (APP_STOPPED, &b""[..]),
            };
            self.sequence += 1;
            let frame = encode_app(kind, request.request_id, 0, payload);
            encode_outer(OUTER_DATA, &INSTANCE, 1, self.sequence, &frame).unwrap()
        };
        self.responses.extend(response);
        Ok(())
    }
}

#[test]
fn operations_bound_their_requests_by_their_deadlines() {
    // Issue #464: an operation's timeout bounds sending its request as well as receiving its
    // response, so attach, ping, exec, and stop each write within the time in which they then
    // read: no transfer gets more than what is left of its operation's timeout.
    let (transport, transfers) = Recording::new(None);
    let mut session = Session::attach(Box::new(transport), &CAPABILITY, 7.0).unwrap();
    let mut operations = vec![(7.0, transfers.lock().unwrap().split_off(0))];
    session.ping(5.0).unwrap();
    operations.push((5.0, transfers.lock().unwrap().split_off(0)));
    session.exec(&request(&["/bin/true"]), 3.0).unwrap();
    operations.push((3.0, transfers.lock().unwrap().split_off(0)));
    session.stop(2.0).unwrap();
    operations.push((2.0, transfers.lock().unwrap().split_off(0)));
    for (timeout, recorded) in operations {
        let kinds: Vec<_> = recorded.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(kinds.first(), Some(&"write"), "{kinds:?}");
        assert!(kinds[1..].iter().all(|kind| *kind == "read"), "{kinds:?}");
        for (kind, left) in recorded {
            let left = left.unwrap().as_secs_f64();
            // Like Python's, reads wait in slices of at most 250 ms, so that Ctrl-C ends them.
            let (least, most) = if kind == "write" {
                (timeout - 1.0, timeout)
            } else {
                (0.0, 0.25)
            };
            assert!(left > least && left <= most, "{kind}: {left} of {timeout}");
        }
    }
}

#[test]
fn failed_transfer_closes_the_session() {
    // A transfer that fails, as by timing out, may cut its record short, after which the
    // endpoint would frame every later record wrongly.
    for (failure, expected) in [
        ("write", "managed control request timed out"),
        ("read", "managed control response timed out"),
    ] {
        // The attachment's write and read succeed.
        let (transport, transfers) = Recording::new(Some((failure, 2)));
        let mut session = Session::attach(Box::new(transport), &CAPABILITY, 5.0).unwrap();
        assert_eq!(
            self::failure(session.ping(5.0)),
            (ErrorCode::TimedOut, expected.to_owned())
        );
        let count = transfers.lock().unwrap().len();
        assert_eq!(
            self::failure(session.exec(&request(&["/bin/true"]), 5.0)),
            (
                ErrorCode::Protocol,
                "managed control session is closed".to_owned()
            )
        );
        assert_eq!(
            message(session.stop(5.0)),
            "managed control session is closed"
        );
        assert_eq!(transfers.lock().unwrap().len(), count);
    }
}

#[test]
fn request_to_a_stalled_endpoint_times_out_and_closes_the_session() {
    // Issue #464: a request that the endpoint does not read must not hold the operation past its
    // timeout.
    let (host, guest) = stream_pair(0);
    let (release, released) = mpsc::channel::<()>();
    let worker = thread::spawn(move || {
        let mut guest = Guest::new(guest);
        guest.accept();
        // The endpoint reads nothing more until the client gives up.
        let _ = released.recv_timeout(Duration::from_secs(10));
    });
    let mut session = Session::attach(Box::new(host), &CAPABILITY, 5.0).unwrap();
    let started = Instant::now();
    assert_eq!(
        message(session.exec(&request(&["/bin/true"]), 0.25)),
        "managed control request timed out"
    );
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(2250), "{elapsed:?}");
    assert_eq!(
        message(session.ping(5.0)),
        "managed control session is closed"
    );
    release.send(()).unwrap();
    worker.join().unwrap();
}

/// Checks that a write to `client`, an endpoint's transport, ends by its deadline while
/// `endpoint` keeps reading, slowly: a request that the endpoint drains slowly must not hold the
/// operation past its timeout either, as Python's sends, which check their deadline before each
/// send, do not. The write is far larger than a request, since the buffers of an endpoint hold a
/// whole request.
#[cfg(any(target_os = "linux", windows))]
fn check_writes_to_a_slow_endpoint(
    mut client: Box<dyn Transport>,
    mut endpoint: impl io::Read + Send + 'static,
) {
    let reader = thread::spawn(move || {
        let mut buffer = vec![0u8; 16 * 1024];
        let mut read = 0;
        while let Ok(count @ 1..) = endpoint.read(&mut buffer) {
            read += count;
            thread::sleep(Duration::from_millis(5));
        }
        read
    });
    let data = vec![0u8; 16 * 1024 * 1024];
    let started = Instant::now();
    let error = client
        .write_all(&data, Some(Duration::from_millis(200)))
        .unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    drop(client);
    assert!(reader.join().unwrap() > 0, "the endpoint read nothing");
}

/// Checks the wait for `endpoint`, which never appears, of a start that checks after each
/// attempt whether the OpenVMM that it launched has exited, as [`Session::connect_launched`]
/// checks it: an OpenVMM that exits ends the wait at once, as a closed endpoint, rather than at
/// the timeout, and one that runs on keeps it going until the timeout.
#[cfg(any(unix, windows))]
fn check_waits_for_openvmm_that_exits(endpoint: &str) {
    let mut checks = 0;
    let started = Instant::now();
    let failure =
        Session::connect_unless_exited(endpoint, std::process::id(), &CAPABILITY, 60.0, || {
            checks += 1;
            checks == 3
        })
        .err()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    assert!(matches!(failure, Exit::EndpointClosed), "{failure:?}");
    assert_eq!(checks, 3);
    let mut checks = 0;
    let started = Instant::now();
    let failure =
        Session::connect_unless_exited(endpoint, std::process::id(), &CAPABILITY, 0.2, || {
            checks += 1;
            false
        })
        .err()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(
        failure.message().unwrap(),
        format!("managed control endpoint did not become available: {endpoint}")
    );
    assert_ne!(checks, 0);
}

#[test]
fn response_that_does_not_arrive_times_out() {
    let (mut session, worker) = session_with(|guest| {
        let _ = guest.request();
        // The guest answers nothing until the host gives up.
        let _ = guest.record();
    });
    let started = Instant::now();
    assert_eq!(
        message(session.ping(0.2)),
        "managed control response timed out"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(session);
    worker.join().unwrap();
}

#[test]
fn a_closed_endpoint_ends_the_operation_with_its_own_error() {
    // OpenVMM closes the endpoint as it tears the VM down, which start and stop tell apart from
    // other failures.
    let (mut session, worker) = session_with(|guest| {
        let _ = guest.record();
    });
    let failure = session.ping(5.0).unwrap_err();
    worker.join().unwrap();
    assert!(matches!(failure, Exit::EndpointClosed), "{failure:?}");
    assert_eq!(failure.code(), Some(ErrorCode::EndpointClosed));
    assert_eq!(failure.message(), Some("managed control endpoint closed"));
    // So does a write to an endpoint that is gone.
    let (mut session, worker) = session_with(|_| {});
    worker.join().unwrap();
    let failure = session.stop(5.0).unwrap_err();
    assert!(matches!(failure, Exit::EndpointClosed), "{failure:?}");
}

#[test]
fn output_beyond_the_guest_limit_breaks_the_protocol() {
    // The guest agent forwards at most 1 MiB of output, so a guest that sends more misbehaves.
    let (mut session, worker) = session_with(|guest| {
        let (_, request_id, _) = guest.request();
        let chunk = vec![b'x'; 60_000];
        for _ in 0..18 {
            guest.send(APP_STDOUT, request_id, 0, &chunk);
        }
        guest.send(APP_EXIT, request_id, 0, b"exit");
    });
    assert_eq!(
        failure(session.exec(&request(&["/bin/true"]), 5.0)),
        (
            ErrorCode::Protocol,
            format!("managed guest exceeded the {MAX_OUTPUT_BYTES}-byte output limit")
        )
    );
    drop(session);
    worker.join().unwrap();
}

#[test]
fn features_of_a_guest_that_predates_them_are_none() {
    let (mut session, worker) = session_with(|guest| {
        let (kind, request_id, _) = guest.request();
        assert_eq!(kind, APP_FEATURES);
        guest.send(APP_ERROR, request_id, 95, UNSUPPORTED_OPERATION);
        let (_, request_id, _) = guest.request();
        guest.send(
            APP_READY,
            request_id,
            0,
            &GuestFeatures::SANDBOX_AGENT.encode(),
        );
        let (_, request_id, _) = guest.request();
        guest.send(APP_ERROR, request_id, 22, b"invalid-request");
    });
    assert_eq!(session.features(5.0).unwrap(), GuestFeatures::NONE);
    assert_eq!(session.features(5.0).unwrap(), GuestFeatures::SANDBOX_AGENT);
    assert_eq!(
        message(session.features(5.0)),
        "managed guest returned an invalid features response"
    );
    worker.join().unwrap();
}

#[test]
fn protocol_violations_keep_the_messages_of_python() {
    let (mut session, worker) = session_with(|guest| {
        let (_, request_id, _) = guest.request();
        guest.send(APP_READY, request_id ^ 1, 0, &[]);
    });
    assert_eq!(
        message(session.ping(5.0)),
        "managed guest did not acknowledge readiness"
    );
    worker.join().unwrap();
    let (mut session, worker) = session_with(|guest| {
        let (_, request_id, _) = guest.request();
        guest.send(APP_STDOUT, request_id ^ 1, 0, b"x");
    });
    assert_eq!(
        message(session.exec(&request(&["/bin/true"]), 5.0)),
        "managed guest returned a mismatched request ID"
    );
    worker.join().unwrap();
    let (mut session, worker) = session_with(|guest| {
        let (_, request_id, _) = guest.request();
        guest.send(APP_READY, request_id ^ 1, 0, &GuestFeatures::NONE.encode());
    });
    assert_eq!(
        message(session.features(5.0)),
        "managed guest returned a mismatched request ID"
    );
    worker.join().unwrap();
    let (mut session, worker) = session_with(|guest| {
        let (_, request_id, _) = guest.request();
        guest.send(APP_READY, request_id, 0, b"");
    });
    assert_eq!(
        message(session.stop(5.0)),
        "managed guest did not acknowledge stop"
    );
    worker.join().unwrap();
    // An acknowledgement has status 0 and no payload.
    for (status, payload) in [(1, &b""[..]), (0, b"x")] {
        let (mut session, worker) = session_with(move |guest| {
            let (_, request_id, _) = guest.request();
            guest.send(APP_READY, request_id, status, payload);
        });
        assert_eq!(
            message(session.ping(5.0)),
            "managed guest did not acknowledge readiness"
        );
        worker.join().unwrap();
    }
}

/// The endpoints of POSIX hosts: Unix-domain sockets that this process serves.
#[cfg(unix)]
mod endpoints {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    use super::*;

    /// The server side of a Unix-domain socket.
    struct Server(UnixStream);

    impl Transport for Server {
        fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
            self.0.set_read_timeout(timeout)?;
            self.0.read(buffer)
        }

        fn write_all(&mut self, data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
            self.0.write_all(data)
        }
    }

    /// Serves `serve` on a fresh endpoint and returns a session connected to it.
    fn connected(
        serve: impl FnOnce(&mut Guest<Server>) + Send + 'static,
    ) -> (Session, thread::JoinHandle<()>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut guest = Guest::new(Server(stream));
            guest.accept();
            serve(&mut guest);
        });
        let session =
            Session::connect(path.to_str().unwrap(), std::process::id(), &CAPABILITY, 5.0).unwrap();
        (session, worker, directory)
    }

    #[test]
    fn read_reports_a_closed_endpoint() {
        let (mut session, worker, _directory) = connected(|guest| {
            let _ = guest.record();
        });
        let failure = session.ping(5.0).unwrap_err();
        worker.join().unwrap();
        assert!(matches!(failure, Exit::EndpointClosed), "{failure:?}");
    }

    #[test]
    fn reset_and_broken_pipe_report_a_closed_endpoint() {
        // OpenVMM may close the socket before it reads everything that the client sent, which
        // resets the client's reads, and a later write fails with a broken pipe.
        let (mut session, worker, _directory) = connected(|guest| {
            let _ = guest.read_exact(OUTER_HEADER_LEN);
        });
        let mut workload = request(&["/bin/true"]);
        workload
            .arguments
            .extend(std::iter::repeat_n("a".repeat(4000), 10));
        let failure = session.exec(&workload, 5.0).unwrap_err();
        worker.join().unwrap();
        assert!(matches!(failure, Exit::EndpointClosed), "{failure:?}");
        let (mut session, worker, _directory) = connected(|_| {});
        worker.join().unwrap();
        let failure = session.stop(5.0).unwrap_err();
        assert!(matches!(failure, Exit::EndpointClosed), "{failure:?}");
    }

    #[test]
    fn endpoints_that_never_appear_time_out_with_their_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let started = Instant::now();
        let failure =
            Session::connect(path.to_str().unwrap(), std::process::id(), &CAPABILITY, 0.2)
                .err()
                .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(
            failure.message().unwrap(),
            format!(
                "managed control endpoint did not become available: {}",
                path.display()
            )
        );
    }

    #[test]
    fn waits_for_an_endpoint_end_once_openvmm_exits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        check_waits_for_openvmm_that_exits(path.to_str().unwrap());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn writes_to_a_slow_endpoint_end_by_their_deadline() {
        // A blocking send restarts its timeout whenever the endpoint makes room.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = platform::connect_endpoint(
            path.to_str().unwrap(),
            std::process::id(),
            Duration::from_secs(5),
        )
        .unwrap();
        let (endpoint, _) = listener.accept().unwrap();
        check_writes_to_a_slow_endpoint(client, endpoint);
    }
}

/// The endpoints of Windows hosts: named pipes that this process serves, through the Win32 calls
/// that the standard library lacks.
#[cfg(windows)]
#[allow(unsafe_code)]
mod endpoints {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    use windows_sys::Win32::Foundation::{
        ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    use super::*;

    /// The server side of a named pipe.
    struct Server(File);

    impl Transport for Server {
        fn read(&mut self, buffer: &mut [u8], _timeout: Option<Duration>) -> io::Result<usize> {
            match self.0.read(buffer) {
                // ERROR_BROKEN_PIPE: the client closed its end.
                Err(error) if error.raw_os_error() == Some(109) => Ok(0),
                result => result,
            }
        }

        fn write_all(&mut self, data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
            self.0.write_all(data)
        }
    }

    /// Creates a named pipe that this process serves, and returns its name and server end.
    fn create_pipe() -> (String, File) {
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).unwrap();
        let id: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
        let name = format!(r"\\.\pipe\nvx-cli-session-test-{id}");
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                65_536,
                65_536,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        // SAFETY: CreateNamedPipeW returned a fresh handle that nothing else owns.
        (name, unsafe { File::from_raw_handle(handle) })
    }

    /// Waits for the client of a pipe.
    fn accept(server: &File) {
        // SAFETY: the handle is the open server end of a pipe.
        let connected = unsafe { ConnectNamedPipe(server.as_raw_handle(), std::ptr::null_mut()) };
        // SAFETY: reads the calling thread's last error.
        assert!(connected != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED);
    }

    /// Serves `serve` on a fresh pipe and returns a session connected to it.
    fn connected(
        serve: impl FnOnce(&mut Guest<Server>) + Send + 'static,
    ) -> (Session, thread::JoinHandle<()>) {
        let (name, server) = create_pipe();
        let worker = thread::spawn(move || {
            accept(&server);
            let mut guest = Guest::new(Server(server));
            guest.accept();
            serve(&mut guest);
        });
        let endpoint = name.replace('\\', "/");
        let session = Session::connect(&endpoint, std::process::id(), &CAPABILITY, 5.0).unwrap();
        (session, worker)
    }

    #[test]
    fn write_reports_a_closed_endpoint() {
        // OpenVMM closes the pipe as it tears the VM down.
        let (mut session, worker) = connected(|_| {});
        worker.join().unwrap();
        let failure = session.ping(5.0).unwrap_err();
        assert!(matches!(failure, Exit::EndpointClosed), "{failure:?}");
    }

    #[test]
    fn read_reports_a_closed_endpoint_after_its_data() {
        let (mut session, worker) = connected(|guest| {
            let (_, request_id, _) = guest.request();
            guest.send(APP_READY, request_id, 0, &[]);
            let _ = guest.request();
        });
        session.ping(5.0).unwrap();
        let failure = session.ping(5.0).unwrap_err();
        worker.join().unwrap();
        assert!(matches!(failure, Exit::EndpointClosed), "{failure:?}");
    }

    #[test]
    fn read_times_out_and_closes_the_session() {
        let (release, released) = mpsc::channel::<()>();
        let (mut session, worker) = connected(move |guest| {
            let _ = guest.request();
            let _ = released.recv_timeout(Duration::from_secs(10));
        });
        let started = Instant::now();
        assert_eq!(
            message(session.ping(0.1)),
            "managed control response timed out"
        );
        assert!(started.elapsed() < Duration::from_millis(2100));
        release.send(()).unwrap();
        worker.join().unwrap();
        // Like Python's session, a session whose transfer failed is closed.
        assert_eq!(
            message(session.ping(5.0)),
            "managed control session is closed"
        );
    }

    #[test]
    fn endpoints_that_never_appear_time_out_with_their_path() {
        let started = Instant::now();
        let failure = Session::connect(
            r"\\.\pipe\nvx-cli-session-test-missing",
            std::process::id(),
            &CAPABILITY,
            0.2,
        )
        .err()
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(
            failure.message().unwrap(),
            r"managed control endpoint did not become available: \\.\pipe\nvx-cli-session-test-missing"
        );
    }

    #[test]
    fn waits_for_an_endpoint_end_once_openvmm_exits() {
        check_waits_for_openvmm_that_exits(r"\\.\pipe\nvx-cli-session-test-exited");
    }

    #[test]
    fn writes_to_a_slow_endpoint_end_by_their_deadline() {
        let (name, server) = create_pipe();
        let accepted = thread::spawn(move || {
            accept(&server);
            server
        });
        let client = platform::connect_endpoint(
            &name.replace('\\', "/"),
            std::process::id(),
            Duration::from_secs(5),
        )
        .unwrap();
        check_writes_to_a_slow_endpoint(client, accepted.join().unwrap());
    }
}
