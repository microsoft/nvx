//! The adapter's host program attaches to the control session between the host and the guest, and
//! exchanges the adapter's own framing with the guest agent, through public APIs only.
//!
//! A test broker plays the broker's host-facing end of the session, and the guest agent behind it,
//! with the public record API of `nvx`, over an in-memory transport, so that the tests run on Linux
//! and on Windows. The tests cover the attachment and its bootstrap identity, waits and refusals,
//! messages that span records and records that share messages, partial records, record lengths,
//! ERROR payloads, sequence and identity errors, resets and disconnects in the middle of a message,
//! timeouts, sends that fail, the answers that requests check, the bounded buffer of the adapter's
//! decoder, the requests of a layered root: its configuration, its probes, and a stop whose
//! teardown failed, and the host's order of a layered root: the guest agent's readiness and build,
//! the mapping table in parts, and `Start`, and each failure of the order, after which the host
//! sends nothing more, or stops the guest. They also cover a clone's order, the framing of the
//! restore sequence of `nvx::host::template`: the bootstrap of its session, its record as the
//! first request, the `Ready` that reports its activation, the sessions that it takes for lost,
//! after a later reset, the end of the stream, or a failure of the transport, whose successor sends
//! the same record, and the failures that are final. The broker's guest also runs the runners of
//! NVX's execution engine for the executions that the host program's driver runs, so that the tests
//! cover the driver's output, credit, outcomes, and timers, the refusals that end its run, a guest
//! that stops reading, and a stop that skips the guest's late messages about the executions that
//! the driver gave up on.
#![cfg(feature = "host")]

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nvx::activation::{ReadyActivation, TransactionId};
use nvx::exec::{
    Cause, Key, LaunchFailure, Limits, Outcome, Runner, Settlement, Status, Stream, Timers,
    Violation, Window, runner,
};
use nvx::host::nvxs::HostConnectionError;
use nvx::host::platform::Transport;
use nvx::host::template::CloneControl;
use nvx::protocol::nvxs::{
    ErrorCode, Identity, Record, RecordError, RecordParser, RecordType, SessionError, encode_record,
};
use nvx::protocol::{
    CAPABILITY_LEN, HostMap, MAX_HOST_MAPPINGS, MAX_MAP_PATH_BYTES, OUTER_HEADER_LEN,
    OUTER_MAX_PAYLOAD, OUTER_MIN_RECEIVE_WINDOW,
};
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::exec::{GuestMessage, HostMessage, MAX_EXECUTIONS, Start};
use runtime_adapter::frame::{
    Decoder, FrameError, HEADER_LEN, MAX_ACTIVATE_BODY, MAX_BODY, MAX_TEXT_BODY, Message,
};
use runtime_adapter::host::{
    self, Client, ClientError, CloneOrder, Executions, ExecutionsError, MAX_OUTPUT_FRAMES,
    MapReply, OrderError,
};
use runtime_adapter::probe::{Facts, Probe, ProbeReply};
use runtime_adapter::root::{
    MapFailure, Ready, RootConfig, RootFailure, RootReply, RootStage, decode_maps, encode_maps,
};

/// Launch capability that the test broker accepts.
const CAPABILITY: [u8; CAPABILITY_LEN] = [0xa5; CAPABILITY_LEN];

/// Identity of the broker's first epoch.
const FIRST: Identity = Identity {
    instance_id: [0x5c; 16],
    epoch: 1,
};

/// Longest wait for a record from the host, for the client's requests, and for a thread to end.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Wait of a receive, a send, or an attachment that the test expects to time out.
const SHORT: Duration = Duration::from_millis(200);

/// Offset of the protocol version in a record's header.
const VERSION_OFFSET: usize = 4;
/// Offset of the record type in a record's header.
const TYPE_OFFSET: usize = 6;
/// Offset of the flags in a record's header.
const FLAGS_OFFSET: usize = 7;
/// Offset of the instance ID in a record's header.
const INSTANCE_OFFSET: usize = 8;
/// Offset of the payload length in a record's header.
const LENGTH_OFFSET: usize = 40;

/// One direction of an in-memory stream.
#[derive(Default)]
struct Pipe {
    /// Bytes that the writer wrote and the reader has not read.
    bytes: VecDeque<u8>,
    /// Whether either end closed the direction.
    closed: bool,
}

/// One direction of an in-memory stream, which its writer and its reader share. Unless the writer
/// limits the bytes in flight, writes never wait, so one thread can play both ends of an exchange.
#[derive(Clone, Default)]
struct Direction(Arc<(Mutex<Pipe>, Condvar)>);

impl Direction {
    /// Appends `bytes` for the reader while at most `capacity` bytes are in flight, waiting at
    /// most `timeout` for the reader to make room, unless either end closed the direction.
    fn write(
        &self,
        mut bytes: &[u8],
        capacity: usize,
        timeout: Option<Duration>,
    ) -> io::Result<()> {
        let (pipe, changed) = &*self.0;
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut pipe = pipe.lock().unwrap();
        while !bytes.is_empty() {
            if pipe.closed {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let room = capacity.saturating_sub(pipe.bytes.len()).min(bytes.len());
            if room == 0 {
                pipe = wait(changed, pipe, deadline)?;
                continue;
            }
            pipe.bytes.extend(&bytes[..room]);
            bytes = &bytes[room..];
            changed.notify_all();
        }
        Ok(())
    }

    /// Reads into `buffer` once any bytes arrive, waiting at most `timeout`, or returns 0 once the
    /// direction is closed and every byte was read.
    fn read(&self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        let (pipe, changed) = &*self.0;
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut pipe = pipe.lock().unwrap();
        while pipe.bytes.is_empty() && !pipe.closed {
            pipe = wait(changed, pipe, deadline)?;
        }
        let count = buffer.len().min(pipe.bytes.len());
        for (target, byte) in buffer.iter_mut().zip(pipe.bytes.drain(..count)) {
            *target = byte;
        }
        // The writer may wait for room.
        changed.notify_all();
        Ok(count)
    }

    /// Closes the direction: the reader reads what is left, then the end of the stream, and
    /// writes fail.
    fn close(&self) {
        let (pipe, changed) = &*self.0;
        pipe.lock().unwrap().closed = true;
        changed.notify_all();
    }
}

/// Waits until `pipe` changes or `deadline` passes, and fails once it passed.
fn wait<'a>(
    changed: &Condvar,
    pipe: MutexGuard<'a, Pipe>,
    deadline: Option<Instant>,
) -> io::Result<MutexGuard<'a, Pipe>> {
    let Some(deadline) = deadline else {
        return Ok(changed.wait(pipe).unwrap());
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::ErrorKind::TimedOut.into());
    }
    Ok(changed.wait_timeout(pipe, left).unwrap().0)
}

/// The host's end of an in-memory stream to the broker. Each read returns at most `piece` bytes,
/// so that the broker's records arrive in pieces of that size, and at most `capacity` bytes of the
/// host's are in flight.
struct HostEnd {
    /// Bytes from the broker.
    input: Direction,
    /// Bytes to the broker.
    output: Direction,
    /// Most bytes that one read returns.
    piece: usize,
    /// Most bytes to the broker in flight.
    capacity: usize,
}

impl Transport for HostEnd {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        let limit = buffer.len().min(self.piece);
        self.input.read(&mut buffer[..limit], timeout)
    }

    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        self.output.write(data, self.capacity, timeout)
    }
}

impl Drop for HostEnd {
    // The host's stream ends with its transport, as a socket's or a pipe's does.
    fn drop(&mut self) {
        self.output.close();
    }
}

/// The broker's host-facing end of the session, and the guest agent behind it.
///
/// It takes the host's attachment, tells the host to wait for the guest, then attaches it, and
/// numbers its records to the host from 0. It checks the identity and sequence number of each of
/// the host's DATA records, and panics on any record that the protocol does not allow. Its guest
/// agent answers the host's messages as the adapter's guest agent does.
struct Broker {
    /// Bytes to the host.
    to_host: Direction,
    /// Bytes from the host.
    from_host: Direction,
    /// Parser of the host's records.
    parser: RecordParser,
    /// Identity of the session's epoch.
    identity: Identity,
    /// Sequence number of the broker's next record to the host.
    send_sequence: u64,
    /// Sequence number of the host's next DATA record.
    receive_sequence: u64,
    /// Decoder of the host's messages.
    decoder: Decoder,
}

impl Broker {
    /// Takes the host's attachment: a bootstrap record, which carries no identity, with the launch
    /// capability.
    fn accept(&mut self) {
        let record = self.read().expect("the host attaches");
        assert_eq!(record.record_type(), RecordType::HostAttach);
        let header = (record.instance_id(), record.epoch(), record.sequence());
        assert_eq!(header, ([0; 16], 0, 0));
        assert_eq!(record.payload(), CAPABILITY);
    }

    /// Takes the host's attachment, tells the host to wait for the guest, then attaches it to the
    /// session's identity.
    fn attach(&mut self) {
        self.accept();
        self.send(RecordType::Wait, &[]);
        self.send(RecordType::Ready, &[]);
    }

    /// Sends a record of the session's identity at the broker's next sequence number.
    fn send(&mut self, record_type: RecordType, payload: &[u8]) {
        let record = record(record_type, self.identity, self.send_sequence, payload);
        self.send_sequence += 1;
        self.write(&record);
    }

    /// Sends `bytes` in DATA records of at most `size` bytes.
    fn send_data(&mut self, bytes: &[u8], size: usize) {
        for chunk in bytes.chunks(size) {
            self.send(RecordType::Data, chunk);
        }
    }

    /// Writes bytes to the host.
    fn write(&self, bytes: &[u8]) {
        self.to_host
            .write(bytes, usize::MAX, None)
            .expect("the broker's stream is open");
    }

    /// Ends the broker's stream to the host.
    fn close(&self) {
        self.to_host.close();
    }

    /// Returns the host's next record, or `None` once the host closed its stream at a record
    /// boundary.
    fn read(&mut self) -> Option<Record> {
        let deadline = Instant::now() + TIMEOUT;
        let mut buffer = [0; 4096];
        loop {
            let wanted = self.parser.needed().min(buffer.len());
            let left = deadline.saturating_duration_since(Instant::now());
            let count = self
                .from_host
                .read(&mut buffer[..wanted], Some(left))
                .expect("the host sends a record or ends its stream in time");
            if count == 0 {
                assert_eq!(
                    self.parser.buffered(),
                    0,
                    "the host ended its stream inside a record"
                );
                return None;
            }
            let (consumed, record) = self.parser.push(&buffer[..count]).unwrap();
            assert_eq!(consumed, count);
            if record.is_some() {
                return record;
            }
        }
    }

    /// Takes the host's next record, a DATA record with the session's identity and the host's
    /// next sequence number, and returns the messages that it completes.
    fn take(&mut self) -> Vec<Message> {
        let record = self.read().expect("the host sends DATA");
        assert_eq!(record.record_type(), RecordType::Data);
        let header = (record.instance_id(), record.epoch(), record.sequence());
        let expected = (
            self.identity.instance_id,
            self.identity.epoch,
            self.receive_sequence,
        );
        assert_eq!(header, expected);
        self.receive_sequence += 1;
        decode(&mut self.decoder, record.payload())
    }

    /// Takes the host's next `count` messages.
    fn messages(&mut self, count: usize) -> Vec<Message> {
        let mut messages = Vec::new();
        while messages.len() < count {
            messages.extend(self.take());
        }
        assert_eq!(messages.len(), count, "{messages:?}");
        messages
    }

    /// Answers the host's next message as the guest agent does, in DATA records of at most `size`
    /// bytes, and returns the message.
    fn answer(&mut self, size: usize) -> Message {
        let [message] = <[Message; 1]>::try_from(self.messages(1)).unwrap();
        self.send_data(&answer(&message).encode().unwrap(), size);
        message
    }

    /// Takes the host's next message, which must be `expected`, and answers it with `reply`.
    fn expect(&mut self, expected: Message, reply: &Message) {
        assert_eq!(self.messages(1), [expected]);
        self.send_data(&reply.encode().unwrap(), OUTER_MAX_PAYLOAD);
    }

    /// Checks that the host sends nothing for a while, as when it waits for an answer.
    fn assert_quiet(&self) {
        let mut byte = [0];
        let read = self.from_host.read(&mut byte, Some(SHORT));
        let quiet = matches!(&read, Err(error) if error.kind() == io::ErrorKind::TimedOut);
        assert!(quiet, "the host sent more: {read:?}");
    }

    /// Serves the host as the guest agent does until it answered `Stop`: it answers the messages
    /// that each of the host's records completes, together, in DATA records of at most `size`
    /// bytes. Returns the host's messages.
    fn serve(&mut self, size: usize) -> Vec<Message> {
        let mut served = Vec::new();
        while !served.contains(&Message::Stop) {
            let messages = self.take();
            let answers = messages.iter().map(answer).collect::<Vec<_>>();
            self.send_data(&encode(&answers), size);
            served.extend(messages);
        }
        served
    }
}

/// Connects a new broker of the session `identity` to a transport for the host, whose reads
/// return at most `piece` bytes.
fn connect(identity: Identity, piece: usize) -> (Broker, Box<dyn Transport>) {
    connect_bounded(identity, piece, usize::MAX)
}

/// Connects a new broker of the session `identity` to a transport for the host, whose reads
/// return at most `piece` bytes, and which holds at most `capacity` bytes of the host's in flight.
fn connect_bounded(
    identity: Identity,
    piece: usize,
    capacity: usize,
) -> (Broker, Box<dyn Transport>) {
    let (to_host, from_host) = (Direction::default(), Direction::default());
    let transport = HostEnd {
        input: to_host.clone(),
        output: from_host.clone(),
        piece,
        capacity,
    };
    let broker = Broker {
        to_host,
        from_host,
        parser: RecordParser::new(),
        identity,
        send_sequence: 0,
        receive_sequence: 0,
        decoder: Decoder::new(),
    };
    (broker, Box::new(transport))
}

/// Attaches a client to a new broker of the session `identity`, over a transport whose reads
/// return at most `piece` bytes, and returns both.
fn attached(identity: Identity, piece: usize) -> (Broker, Client) {
    attach_over(connect(identity, piece))
}

/// Attaches a client over the transport of a connection to its broker, and returns both.
fn attach_over((mut broker, transport): (Broker, Box<dyn Transport>)) -> (Broker, Client) {
    let attaching = attach(transport, deadline());
    broker.attach();
    let client = joined(attaching).unwrap();
    assert_eq!(client.identity(), broker.identity);
    (broker, client)
}

/// Starts the client's attachment over `transport` in a thread, with `deadline`.
fn attach(
    transport: Box<dyn Transport>,
    deadline: Instant,
) -> JoinHandle<Result<Client, ClientError>> {
    thread::spawn(move || Client::attach(transport, &CAPABILITY, deadline))
}

/// Runs `request` with `client` in a thread, and returns the thread, which returns the client with
/// the request's result.
fn spawn<T: Send + 'static>(
    mut client: Client,
    request: impl FnOnce(&mut Client) -> T + Send + 'static,
) -> JoinHandle<(Client, T)> {
    thread::spawn(move || {
        let result = request(&mut client);
        (client, result)
    })
}

/// Returns what `thread` returned, and fails the test if it does not end in time, instead of
/// hanging it.
fn joined<T>(thread: JoinHandle<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    while !thread.is_finished() {
        assert!(Instant::now() < deadline, "the client ends in time");
        thread::sleep(Duration::from_millis(5));
    }
    thread.join().unwrap()
}

/// Returns the deadline of an operation that the test expects to complete.
fn deadline() -> Instant {
    Instant::now() + TIMEOUT
}

/// Encodes a valid record.
fn record(record_type: RecordType, identity: Identity, sequence: u64, payload: &[u8]) -> Vec<u8> {
    let mut record = Vec::new();
    let (instance, epoch) = (identity.instance_id, identity.epoch);
    encode_record(
        record_type,
        &instance,
        epoch,
        sequence,
        payload,
        &mut record,
    )
    .unwrap();
    record
}

/// Returns `bytes` with `patch` written at `offset`.
fn patched(mut bytes: Vec<u8>, offset: usize, patch: &[u8]) -> Vec<u8> {
    bytes[offset..offset + patch.len()].copy_from_slice(patch);
    bytes
}

/// Returns a record of `record_type` whose header declares a payload of `length` bytes, followed
/// by that many bytes, which no valid record of the type carries.
fn with_length(record_type: RecordType, identity: Identity, sequence: u64, length: u32) -> Vec<u8> {
    // Each type allows a payload of one of these lengths.
    let valid = match record_type {
        RecordType::Data | RecordType::Error => 4,
        _ => 0,
    };
    let payload = if record_type == RecordType::Error {
        ErrorCode::Authentication.code().to_le_bytes().to_vec()
    } else {
        vec![0; valid]
    };
    let mut bytes = patched(
        record(record_type, identity, sequence, &payload),
        LENGTH_OFFSET,
        &length.to_le_bytes(),
    );
    bytes.truncate(OUTER_HEADER_LEN);
    bytes.resize(OUTER_HEADER_LEN + length as usize, 0);
    bytes
}

/// Returns the guest agent's answer to `message`, in a guest whose root and mappings succeed.
fn answer(message: &Message) -> Message {
    match message {
        Message::Ping => Message::Pong,
        Message::Echo(body) => Message::Echo(body.clone()),
        Message::Stop => Message::Stopped,
        Message::RootConfig(_) => ready_message(),
        Message::Maps(_) => Message::Mapped,
        Message::Start => Message::Started,
        message => panic!("the host sent {message:?}, which only the guest sends"),
    }
}

/// Returns the encodings of `messages`, one after the other.
fn encode(messages: &[Message]) -> Vec<u8> {
    messages
        .iter()
        .flat_map(|message| message.encode().unwrap())
        .collect()
}

/// Returns the messages that `bytes` complete.
fn decode(decoder: &mut Decoder, mut bytes: &[u8]) -> Vec<Message> {
    let mut messages = Vec::new();
    while !bytes.is_empty() {
        let (consumed, message) = decoder.push(bytes).unwrap();
        bytes = &bytes[consumed..];
        messages.extend(message);
    }
    messages
}

/// Returns the longest body: with its header, a message that carries it spans five records.
fn longest() -> Vec<u8> {
    (0..=u8::MAX).cycle().take(MAX_BODY).collect()
}

/// Returns the guest's readiness, with the guest agent's build that the host program ships.
fn ready() -> Ready {
    Ready::new(GUEST_BUILD).unwrap()
}

/// Returns the message of the guest's [`ready`]ness.
fn ready_message() -> Message {
    Message::Ready(ready().encode())
}

/// Returns the body of a part of a mapping table, of one mapping.
fn maps_part() -> Vec<u8> {
    encode_maps(&[mapping("dir", "/srv/dir", true)])
        .unwrap()
        .remove(0)
}

/// Returns a mapping of the host's mapping table.
fn mapping(source: &str, target: &str, read_only: bool) -> HostMap {
    HostMap {
        source: source.to_owned(),
        target: target.to_owned(),
        read_only,
    }
}

/// Returns a mapping table of long paths, which travels in several parts.
fn long_table() -> Vec<HostMap> {
    let long = "p".repeat(MAX_MAP_PATH_BYTES - 6);
    (0..100)
        .map(|index| {
            let source = format!("{index:03}/{long}");
            mapping(&source, &format!("/{source}"), index % 3 == 0)
        })
        .collect()
}

/// Returns the root configuration that announces the mappings of `table`.
fn announcing(table: &[HostMap]) -> RootConfig {
    RootConfig {
        mapping_count: u32::try_from(table.len()).unwrap(),
        ..RootConfig::default()
    }
}

/// Runs the host's order with `client` in a thread, for the mapping table `table`: `ready`, with
/// the guest agent's build that the host program ships, then `start_sandbox`. The thread returns
/// the client with the guest's readiness, or how the order failed.
fn order(client: Client, table: Vec<HostMap>) -> JoinHandle<(Client, Result<Ready, OrderError>)> {
    spawn(client, move |client| {
        let ready = host::ready(client, &announcing(&table), GUEST_BUILD, deadline())?;
        host::start_sandbox(client, &table, deadline())?;
        Ok(ready)
    })
}

/// Checks that the host sent nothing more once its order ended: the host's stream ends at a record
/// boundary once its client, which `ordering` returns, drops. Returns how the order ended.
fn ended(
    mut broker: Broker,
    ordering: JoinHandle<(Client, Result<Ready, OrderError>)>,
) -> OrderError {
    let (client, ordered) = joined(ordering);
    drop(client);
    assert!(broker.read().is_none(), "the host sent more");
    ordered.unwrap_err()
}

/// Checks that a failure ended `client`: every later call fails.
fn assert_ended(client: &mut Client) {
    let received = client.receive(Some(deadline()));
    assert!(matches!(received, Err(ClientError::Ended)), "{received:?}");
    let sent = client.send(&Message::Ping, Some(deadline()));
    assert!(matches!(sent, Err(ClientError::Ended)), "{sent:?}");
    let pinged = client.ping(deadline());
    assert!(matches!(pinged, Err(ClientError::Ended)), "{pinged:?}");
}

#[test]
fn the_client_attaches_and_exchanges_messages_that_span_records() {
    // Echoes in one record, of nothing, of the largest record, and of the longest body, which
    // spans five records each way.
    let bodies = [
        b"one record".to_vec(),
        Vec::new(),
        vec![0x42; OUTER_MAX_PAYLOAD],
        longest(),
    ];
    for size in [7, 4096, OUTER_MAX_PAYLOAD] {
        let (mut broker, transport) = connect(FIRST, usize::MAX);
        let serving = thread::spawn(move || {
            broker.attach();
            let served = broker.serve(size);
            // The host's stream ends with the client, at a record boundary.
            assert!(broker.read().is_none());
            served
        });
        let mut client = Client::attach(transport, &CAPABILITY, deadline()).unwrap();
        assert_eq!(client.identity(), FIRST);
        client.ping(deadline()).unwrap();
        for body in &bodies {
            client.echo(body, deadline()).unwrap();
        }
        assert_eq!(client.stop(deadline()).unwrap(), None);
        drop(client);

        let mut expected = vec![Message::Ping];
        expected.extend(bodies.iter().cloned().map(Message::Echo));
        expected.push(Message::Stop);
        assert_eq!(joined(serving), expected, "records of {size} bytes");
    }
}

#[test]
fn the_client_reassembles_messages_from_partial_and_shared_records() {
    // In records of 7 bytes, most records end a message and start the next.
    let messages = [
        Message::Pong,
        Message::Echo(b"two".to_vec()),
        Message::Echo(Vec::new()),
        Message::Echo(vec![0x42; 100_000]),
        Message::Pong,
        Message::Stopped,
    ];
    let stream = encode(&messages);
    let cases = [
        (1, 4096),
        (5, 7),
        (OUTER_HEADER_LEN, OUTER_MAX_PAYLOAD),
        (4096, 7),
        (usize::MAX, OUTER_MAX_PAYLOAD),
    ];
    for (piece, size) in cases {
        let (mut broker, mut client) = attached(FIRST, piece);
        broker.send_data(&stream, size);
        for expected in &messages {
            let message = client.receive(Some(deadline())).unwrap();
            assert_eq!(
                &message, expected,
                "reads of {piece}, records of {size} bytes"
            );
        }
        // Nothing more arrives.
        let timed_out = client.receive(Some(Instant::now() + SHORT));
        let nothing = matches!(
            timed_out,
            Err(ClientError::Session(HostConnectionError::TimedOut))
        );
        assert!(nothing, "{timed_out:?}");
    }
}

#[test]
fn a_receive_that_times_out_keeps_the_partial_message() {
    let (mut broker, mut client) = attached(FIRST, usize::MAX);
    let echo = Message::Echo(vec![7; 1000]);
    let encoded = echo.encode().unwrap();

    // Half of the message, then the first bytes of the record with the rest.
    broker.send(RecordType::Data, &encoded[..500]);
    let rest = record(
        RecordType::Data,
        FIRST,
        broker.send_sequence,
        &encoded[500..],
    );
    broker.send_sequence += 1;
    broker.write(&rest[..20]);
    let timed_out = client.receive(Some(Instant::now() + SHORT));
    let kept = matches!(
        timed_out,
        Err(ClientError::Session(HostConnectionError::TimedOut))
    );
    assert!(kept, "{timed_out:?}");

    broker.write(&rest[20..]);
    assert_eq!(client.receive(Some(deadline())).unwrap(), echo);

    // The client stays usable.
    client.send(&Message::Ping, Some(deadline())).unwrap();
    assert_eq!(broker.answer(OUTER_MAX_PAYLOAD), Message::Ping);
    assert_eq!(client.receive(Some(deadline())).unwrap(), Message::Pong);
}

#[test]
fn a_send_that_fails_or_times_out_ends_the_client_with_what_it_holds() {
    // One record carries two messages: the client returns the first, and holds the second.
    let shared = encode(&[Message::Pong, Message::Echo(b"held".to_vec())]);

    // The broker stops reading, and the host's stream holds 128 bytes, so a send times out after
    // it wrote part of a record, which the session cannot resume.
    let (mut broker, mut client) = attach_over(connect_bounded(FIRST, usize::MAX, 128));
    broker.send(RecordType::Data, &shared);
    assert_eq!(client.receive(Some(deadline())).unwrap(), Message::Pong);
    let echo = Message::Echo(vec![0; 1000]);
    let timed_out = client.send(&echo, Some(Instant::now() + SHORT));
    let late = matches!(
        timed_out,
        Err(ClientError::Session(HostConnectionError::TimedOut))
    );
    assert!(late, "{timed_out:?}");
    assert_ended(&mut client);

    // The transport fails, as when the broker went away.
    let (mut broker, mut client) = attached(FIRST, usize::MAX);
    broker.send(RecordType::Data, &shared);
    assert_eq!(client.receive(Some(deadline())).unwrap(), Message::Pong);
    broker.from_host.close();
    let failed = client.send(&Message::Ping, Some(deadline()));
    let broken = matches!(
        &failed,
        Err(ClientError::Session(HostConnectionError::Io(error)))
            if error.kind() == io::ErrorKind::BrokenPipe
    );
    assert!(broken, "{failed:?}");
    assert_ended(&mut client);
}

#[test]
fn a_reset_or_a_disconnect_in_the_middle_of_a_message_drops_it() {
    let echo = Message::Echo(vec![1; 1000]).encode().unwrap();

    // Half of a message, then the broker resets the session to the instance's next epoch, which
    // ends the attachment.
    let (mut broker, mut client) = attached(FIRST, usize::MAX);
    broker.send(RecordType::Data, &echo[..500]);
    let next = Identity { epoch: 2, ..FIRST };
    broker.identity = next;
    broker.send(RecordType::Reset, &[]);
    let reset = client.receive(Some(deadline()));
    let ended = matches!(
        reset,
        Err(ClientError::Session(HostConnectionError::Reset(identity))) if identity == next
    );
    assert!(ended, "{reset:?}");
    assert_ended(&mut client);

    // The new epoch starts with a new message, in a new attachment: its first bytes complete
    // nothing of the earlier epoch's.
    let (mut broker, mut client) = attached(next, usize::MAX);
    client.send(&Message::Ping, Some(deadline())).unwrap();
    assert_eq!(broker.answer(OUTER_MAX_PAYLOAD), Message::Ping);
    assert_eq!(client.receive(Some(deadline())).unwrap(), Message::Pong);

    // Part of a header, then the broker ends the stream at a record boundary, as OpenVMM's broker
    // ends an attachment.
    broker.send(RecordType::Data, &echo[..HEADER_LEN - 1]);
    broker.close();
    let closed = client.receive(Some(deadline()));
    let ended = matches!(
        closed,
        Err(ClientError::Session(HostConnectionError::Closed))
    );
    assert!(ended, "{closed:?}");
    assert_ended(&mut client);

    // A broker restored from a snapshot has a new instance, which starts at epoch 1.
    let restored = Identity {
        instance_id: [0x7e; 16],
        epoch: 1,
    };
    let (mut broker, client) = attached(restored, usize::MAX);
    let pinging = spawn(client, |client| client.ping(deadline()));
    assert_eq!(broker.answer(7), Message::Ping);
    let (mut client, pinged) = joined(pinging);
    pinged.unwrap();

    // A stream that ends inside a record fails it.
    let data = record(RecordType::Data, restored, broker.send_sequence, &echo);
    broker.write(&data[..30]);
    broker.close();
    let truncated = client.receive(Some(deadline()));
    let ended = matches!(
        truncated,
        Err(ClientError::Session(HostConnectionError::Protocol(
            SessionError::Truncated { buffered: 30 }
        )))
    );
    assert!(ended, "{truncated:?}");
    assert_ended(&mut client);
}

#[test]
fn the_client_waits_for_the_guest_and_reports_refusals_while_attaching() {
    // The broker tells the host to wait until the guest acknowledges the session's reset, as often
    // as it takes.
    let (mut broker, transport) = connect(FIRST, usize::MAX);
    let attaching = attach(transport, deadline());
    broker.accept();
    for _ in 0..3 {
        broker.send(RecordType::Wait, &[]);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!attaching.is_finished(), "the client waits for READY");
    broker.send(RecordType::Ready, &[]);
    assert_eq!(joined(attaching).unwrap().identity(), FIRST);

    // The deadline passes while the broker waits for the guest. The broker writes its WAIT ahead,
    // so that the case doesn't depend on when the client's thread starts.
    let (mut broker, transport) = connect(FIRST, usize::MAX);
    broker.send(RecordType::Wait, &[]);
    let timed_out = joined(attach(transport, Instant::now() + SHORT));
    let late = matches!(
        timed_out,
        Err(ClientError::Session(HostConnectionError::TimedOut))
    );
    assert!(late, "{timed_out:?}");

    // The broker refuses the capability with an ERROR, at once or after a WAIT.
    for waits in [0, 1] {
        let (mut broker, transport) = connect(FIRST, usize::MAX);
        let attaching = attach(transport, deadline());
        broker.accept();
        for _ in 0..waits {
            broker.send(RecordType::Wait, &[]);
        }
        let code = ErrorCode::Authentication.code().to_le_bytes();
        broker.send(RecordType::Error, &code);
        let refused = joined(attaching);
        let rejected = matches!(
            refused,
            Err(ClientError::Session(HostConnectionError::Rejected(
                ErrorCode::Authentication
            )))
        );
        assert!(rejected, "after {waits} WAIT records: {refused:?}");
    }

    // OpenVMM's broker refuses it by ending the stream.
    let (mut broker, transport) = connect(FIRST, usize::MAX);
    let attaching = attach(transport, deadline());
    broker.accept();
    broker.close();
    let refused = joined(attaching);
    let closed = matches!(
        refused,
        Err(ClientError::Session(HostConnectionError::Closed))
    );
    assert!(closed, "{refused:?}");
}

#[test]
fn the_client_fails_on_records_that_the_protocol_does_not_allow() {
    let other_instance = Identity {
        instance_id: [0x7e; 16],
        ..FIRST
    };
    let next_epoch = Identity { epoch: 2, ..FIRST };
    let no_identity = Identity {
        instance_id: [0; 16],
        epoch: 0,
    };
    let code = |code: u32| code.to_le_bytes();
    let authentication = code(ErrorCode::Authentication.code());
    let wait = record(RecordType::Wait, FIRST, 0, &[]);

    // Records that the broker sends while it attaches the host, at its sequence 0.
    let attaching = [
        (
            "a WAIT that skips a sequence number",
            record(RecordType::Wait, FIRST, 1, &[]),
            SessionError::Sequence {
                expected: 0,
                actual: 1,
            },
        ),
        (
            "a READY that repeats a sequence number",
            [wait.clone(), record(RecordType::Ready, FIRST, 0, &[])].concat(),
            SessionError::Sequence {
                expected: 1,
                actual: 0,
            },
        ),
        (
            "a READY without an instance",
            record(
                RecordType::Ready,
                Identity {
                    epoch: 1,
                    ..no_identity
                },
                0,
                &[],
            ),
            SessionError::MissingIdentity(RecordType::Ready),
        ),
        (
            "a READY without an epoch",
            record(RecordType::Ready, Identity { epoch: 0, ..FIRST }, 0, &[]),
            SessionError::MissingIdentity(RecordType::Ready),
        ),
        (
            "a WAIT with a payload",
            with_length(RecordType::Wait, FIRST, 0, 4),
            SessionError::Record(RecordError::Length {
                record_type: RecordType::Wait,
                length: 4,
            }),
        ),
        (
            "a READY with a payload",
            with_length(RecordType::Ready, FIRST, 0, 1),
            SessionError::Record(RecordError::Length {
                record_type: RecordType::Ready,
                length: 1,
            }),
        ),
        (
            "an ERROR without a code",
            with_length(RecordType::Error, FIRST, 0, 0),
            SessionError::Record(RecordError::Length {
                record_type: RecordType::Error,
                length: 0,
            }),
        ),
        (
            "an ERROR with an unknown code",
            patched(
                record(RecordType::Error, FIRST, 0, &authentication),
                OUTER_HEADER_LEN,
                &code(2),
            ),
            SessionError::Record(RecordError::ErrorCode(2)),
        ),
        (
            "DATA before READY",
            record(RecordType::Data, FIRST, 0, b"x"),
            SessionError::Unexpected(RecordType::Data),
        ),
        (
            "a RESET before READY",
            record(RecordType::Reset, FIRST, 0, &[]),
            SessionError::Unexpected(RecordType::Reset),
        ),
        (
            "a guest's attachment",
            record(RecordType::GuestAttach, no_identity, 0, &[]),
            SessionError::Unexpected(RecordType::GuestAttach),
        ),
        (
            "a bootstrap record with an identity",
            patched(
                record(RecordType::HostAttach, no_identity, 0, &CAPABILITY),
                INSTANCE_OFFSET,
                &[1],
            ),
            SessionError::Record(RecordError::BootstrapIdentity(RecordType::HostAttach)),
        ),
        (
            "an ACK",
            record(
                RecordType::Ack,
                FIRST,
                0,
                &OUTER_MIN_RECEIVE_WINDOW.to_le_bytes(),
            ),
            SessionError::Unexpected(RecordType::Ack),
        ),
        (
            "a CREDIT",
            record(RecordType::Credit, FIRST, 0, &code(1)),
            SessionError::Unexpected(RecordType::Credit),
        ),
        (
            "an unknown record type",
            patched(wait.clone(), TYPE_OFFSET, &[0x7f]),
            SessionError::Record(RecordError::Type(0x7f)),
        ),
        (
            "nonzero flags",
            patched(wait.clone(), FLAGS_OFFSET, &[1]),
            SessionError::Record(RecordError::Flags(1)),
        ),
        (
            "another protocol version",
            patched(wait.clone(), VERSION_OFFSET, &2u16.to_le_bytes()),
            SessionError::Record(RecordError::Version(2)),
        ),
        (
            "a wrong magic",
            patched(wait, 0, b"NVXT"),
            SessionError::Record(RecordError::Magic(*b"NVXT")),
        ),
    ];
    for (name, bytes, expected) in attaching {
        let (mut broker, transport) = connect(FIRST, usize::MAX);
        let attaching = attach(transport, deadline());
        broker.accept();
        broker.write(&bytes);
        let refused = joined(attaching);
        let failed = matches!(
            &refused,
            Err(ClientError::Session(HostConnectionError::Protocol(error))) if *error == expected
        );
        assert!(failed, "{name}: {refused:?}");
    }

    // Records that the broker sends once it attached the host, at its sequence 2, after WAIT and
    // READY.
    let ping = Message::Ping.encode().unwrap();
    let data = |identity, sequence| record(RecordType::Data, identity, sequence, &ping);
    let attached_cases = [
        (
            "DATA that skips a sequence number",
            data(FIRST, 3),
            SessionError::Sequence {
                expected: 2,
                actual: 3,
            },
        ),
        (
            "DATA that repeats a sequence number",
            data(FIRST, 1),
            SessionError::Sequence {
                expected: 2,
                actual: 1,
            },
        ),
        (
            "DATA of another epoch",
            data(next_epoch, 2),
            SessionError::Epoch {
                expected: 1,
                actual: 2,
            },
        ),
        (
            "DATA of another instance",
            data(other_instance, 2),
            SessionError::Instance {
                expected: FIRST.instance_id,
                actual: other_instance.instance_id,
            },
        ),
        (
            "an empty DATA record",
            with_length(RecordType::Data, FIRST, 2, 0),
            SessionError::Record(RecordError::Length {
                record_type: RecordType::Data,
                length: 0,
            }),
        ),
        (
            // Refused at its header, before its payload arrives: the record parser never buffers a
            // payload beyond the limit.
            "a DATA header beyond the largest payload",
            with_length(RecordType::Data, FIRST, 2, OUTER_MAX_PAYLOAD as u32 + 1)
                [..OUTER_HEADER_LEN]
                .to_vec(),
            SessionError::Record(RecordError::Length {
                record_type: RecordType::Data,
                length: OUTER_MAX_PAYLOAD + 1,
            }),
        ),
        (
            "a RESET that repeats the epoch",
            record(RecordType::Reset, FIRST, 2, &[]),
            SessionError::StaleEpoch {
                current: 1,
                actual: 1,
            },
        ),
        (
            "a RESET of another instance",
            record(
                RecordType::Reset,
                Identity {
                    epoch: 2,
                    ..other_instance
                },
                2,
                &[],
            ),
            SessionError::Instance {
                expected: FIRST.instance_id,
                actual: other_instance.instance_id,
            },
        ),
        (
            "a RESET without an identity",
            record(RecordType::Reset, no_identity, 2, &[]),
            SessionError::MissingIdentity(RecordType::Reset),
        ),
        (
            "a RESET with a payload",
            with_length(RecordType::Reset, next_epoch, 2, 4),
            SessionError::Record(RecordError::Length {
                record_type: RecordType::Reset,
                length: 4,
            }),
        ),
        (
            "a WAIT after READY",
            record(RecordType::Wait, FIRST, 2, &[]),
            SessionError::Unexpected(RecordType::Wait),
        ),
        (
            "a second READY",
            record(RecordType::Ready, FIRST, 2, &[]),
            SessionError::Unexpected(RecordType::Ready),
        ),
        (
            "an ERROR after READY",
            record(RecordType::Error, FIRST, 2, &authentication),
            SessionError::Unexpected(RecordType::Error),
        ),
    ];
    for (name, bytes, expected) in attached_cases {
        let (broker, mut client) = attached(FIRST, usize::MAX);
        broker.write(&bytes);
        let refused = client.receive(Some(deadline()));
        let failed = matches!(
            &refused,
            Err(ClientError::Session(HostConnectionError::Protocol(error))) if *error == expected
        );
        assert!(failed, "{name}: {refused:?}");
        assert_ended(&mut client);
    }
}

#[test]
fn the_client_refuses_messages_that_its_framing_does_not_allow() {
    // A message is refused at its header, before its body arrives: the decoder never buffers a
    // body beyond the limit.
    let too_long = u32::try_from(MAX_BODY + 1).unwrap().to_le_bytes();
    let header = |length: [u8; 4], kind: u8| [&length[..], &[kind]].concat();
    let too_long_text = u32::try_from(MAX_TEXT_BODY + 1).unwrap().to_le_bytes();
    let cases = [
        (header([0; 4], 0), FrameError::Kind(0)),
        (header([0; 4], 28), FrameError::Kind(28)),
        (
            header(too_long, 3),
            FrameError::Length {
                kind: 3,
                length: MAX_BODY + 1,
            },
        ),
        (
            header([0xff; 4], 3),
            FrameError::Length {
                kind: 3,
                length: u32::MAX as usize,
            },
        ),
        (
            header(1u32.to_le_bytes(), 2),
            FrameError::Length { kind: 2, length: 1 },
        ),
        (
            header(too_long_text, 8),
            FrameError::Length {
                kind: 8,
                length: MAX_TEXT_BODY + 1,
            },
        ),
        (
            header(too_long_text, 7),
            FrameError::Length {
                kind: 7,
                length: MAX_TEXT_BODY + 1,
            },
        ),
        (
            header(1u32.to_le_bytes(), 23),
            FrameError::Length {
                kind: 23,
                length: 1,
            },
        ),
        (
            header(1u32.to_le_bytes(), 26),
            FrameError::Length {
                kind: 26,
                length: 1,
            },
        ),
    ];
    for (bytes, expected) in cases {
        let (mut broker, mut client) = attached(FIRST, usize::MAX);
        broker.send(RecordType::Data, &bytes);
        let refused = client.receive(Some(deadline()));
        let failed = matches!(&refused, Err(ClientError::Frame(error)) if *error == expected);
        assert!(failed, "{bytes:?}: {refused:?}");
        assert_ended(&mut client);
    }

    // The client takes the guest agent's messages, and refuses those that only the host program
    // sends.
    let key = Key::new(1, 1);
    let host_messages = [
        Message::Ping,
        Message::Stop,
        Message::RootConfig(RootConfig::default().encode()),
        Message::Probe(b"mounts".to_vec()),
        Message::Maps(maps_part()),
        Message::Start,
        HostMessage::Start(start_of(1)).encode(),
        HostMessage::Input(key, b"input").encode(),
        HostMessage::InputEnd(key).encode(),
        HostMessage::Cancel(key, Cause::Cancelled).encode(),
        Message::Activate(activation_record()),
    ];
    for message in host_messages {
        let (mut broker, mut client) = attached(FIRST, usize::MAX);
        broker.send(RecordType::Data, &message.encode().unwrap());
        let refused = client.receive(Some(deadline()));
        let failed = matches!(
            &refused,
            Err(ClientError::Unexpected(kind)) if *kind == message.kind()
        );
        assert!(failed, "{message:?}: {refused:?}");
        assert_ended(&mut client);
    }

    // The client refuses to send a message beyond the limit, sends nothing, and stays usable.
    let (mut broker, mut client) = attached(FIRST, usize::MAX);
    let too_long = vec![0; MAX_BODY + 1];
    let refused = client.send(&Message::Echo(too_long.clone()), Some(deadline()));
    let expected = FrameError::Length {
        kind: 3,
        length: MAX_BODY + 1,
    };
    assert!(
        matches!(&refused, Err(ClientError::Frame(error)) if *error == expected),
        "{refused:?}"
    );
    let refused = client.echo(&too_long, deadline());
    assert!(
        matches!(&refused, Err(ClientError::Frame(error)) if *error == expected),
        "{refused:?}"
    );
    let pinging = spawn(client, |client| client.ping(deadline()));
    // The host's first DATA record carries the Ping.
    assert_eq!(broker.answer(OUTER_MAX_PAYLOAD), Message::Ping);
    joined(pinging).1.unwrap();
}

#[test]
fn requests_check_their_answers_and_end_the_client_when_they_fail() {
    /// A request of the client.
    type Request = fn(&mut Client) -> Result<(), ClientError>;
    /// The kinds of a request and of its answer that a failure reports, or `None` for an echo of
    /// other bytes.
    type Kinds = Option<(u8, u8)>;
    let configure: Request = |client| {
        client
            .configure_root(&RootConfig::default(), deadline())
            .map(drop)
    };
    let probe: Request = |client| {
        let probe = Probe::parse("mounts").unwrap();
        client.probe(&probe, deadline()).map(drop)
    };
    let stop: Request = |client| client.stop(deadline()).map(drop);
    let map: Request = |client| client.map(&maps_part(), deadline()).map(drop);
    let start: Request = |client| client.start(deadline());
    let activate: Request = |client| client.activate(&activation_record(), deadline()).map(drop);
    let teardown = RootFailure::new(RootStage::Teardown, "busy").encode();
    // Each request, and answers that don't answer it.
    let cases: [(&str, Request, Vec<Message>, Kinds); 18] = [
        (
            "Ping answered with Stopped",
            |client| client.ping(deadline()),
            vec![Message::Stopped],
            Some((1, 5)),
        ),
        (
            "Echo answered with Pong",
            |client| client.echo(b"abc", deadline()),
            vec![Message::Pong],
            Some((3, 2)),
        ),
        (
            "Echo answered with other bytes",
            |client| client.echo(b"abc", deadline()),
            vec![Message::Echo(b"abd".to_vec())],
            None,
        ),
        (
            "Stop answered with Pong",
            stop,
            vec![Message::Pong],
            Some((4, 2)),
        ),
        (
            "Stop answered with an Echo",
            stop,
            vec![Message::Echo(Vec::new())],
            Some((4, 3)),
        ),
        (
            "Stop answered with Ready",
            stop,
            vec![ready_message()],
            Some((4, 7)),
        ),
        (
            "Stop answered with a teardown that failed, then Pong",
            stop,
            vec![Message::RootFailed(teardown.clone()), Message::Pong],
            Some((4, 2)),
        ),
        (
            "Stop answered with a teardown that failed, then another",
            stop,
            vec![
                Message::RootFailed(teardown.clone()),
                Message::RootFailed(teardown),
            ],
            Some((4, 8)),
        ),
        (
            "a root configuration answered with Pong",
            configure,
            vec![Message::Pong],
            Some((6, 2)),
        ),
        (
            "a root configuration answered with a Report",
            configure,
            vec![Message::Report(Vec::new())],
            Some((6, 10)),
        ),
        (
            "a probe answered with Ready",
            probe,
            vec![ready_message()],
            Some((9, 7)),
        ),
        (
            "a part of the mapping table answered with Pong",
            map,
            vec![Message::Pong],
            Some((22, 2)),
        ),
        (
            "a part of the mapping table answered with Started",
            map,
            vec![Message::Started],
            Some((22, 26)),
        ),
        (
            "a part of the mapping table answered with Ready",
            map,
            vec![ready_message()],
            Some((22, 7)),
        ),
        (
            "Start answered with Mapped",
            start,
            vec![Message::Mapped],
            Some((25, 23)),
        ),
        (
            "Start answered with a mapping's failure",
            start,
            vec![Message::MapFailed(MapFailure::new(0, "late").encode())],
            Some((25, 24)),
        ),
        (
            "an Activate answered with Started",
            activate,
            vec![Message::Started],
            Some((27, 26)),
        ),
        (
            "an Activate answered with a root's failure",
            activate,
            vec![Message::RootFailed(
                RootFailure::new(RootStage::Layout, "late").encode(),
            )],
            Some((27, 8)),
        ),
    ];
    for (name, request, replies, kinds) in cases {
        let (mut broker, client) = attached(FIRST, usize::MAX);
        let requesting = spawn(client, request);
        broker.messages(1);
        broker.send_data(&encode(&replies), OUTER_MAX_PAYLOAD);
        let (mut client, result) = joined(requesting);
        let error = result.unwrap_err();
        let failed = match (&error, kinds) {
            (ClientError::Reply { request, reply }, Some(kinds)) => (*request, *reply) == kinds,
            (ClientError::Echo, None) => true,
            _ => false,
        };
        assert!(failed, "{name}: {error:?}");
        assert_ended(&mut client);
    }

    // A request whose answer does not arrive in time ends the client, so that a late answer never
    // answers a later request.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let requesting = spawn(client, |client| client.ping(Instant::now() + SHORT));
    assert_eq!(broker.messages(1), [Message::Ping]);
    let (mut client, result) = joined(requesting);
    let late = matches!(
        result,
        Err(ClientError::Session(HostConnectionError::TimedOut))
    );
    assert!(late, "{result:?}");
    broker.send_data(&Message::Pong.encode().unwrap(), OUTER_MAX_PAYLOAD);
    assert_ended(&mut client);
}

#[test]
fn the_client_configures_a_layered_root_probes_it_and_learns_how_its_teardown_ended() {
    let (mut broker, client) = attached(FIRST, usize::MAX);

    // The guest assembles the root, and reports the guest agent's build.
    let configuring = spawn(client, |client| {
        client.configure_root(&RootConfig::default(), deadline())
    });
    let expected = Message::RootConfig(RootConfig::default().encode());
    assert_eq!(broker.messages(1), [expected]);
    broker.send_data(&ready_message().encode().unwrap(), OUTER_MAX_PAYLOAD);
    let (client, configured) = joined(configuring);
    assert_eq!(configured.unwrap(), RootReply::Ready(ready()));

    // A probe's facts, in records of 7 bytes, and a probe that failed.
    let mut facts = Facts::new();
    facts.push("root.type", "overlay");
    facts.push("root.options", "rw,nosuid,nodev");
    let probing = spawn(client, |client| {
        client.probe(&Probe::parse("mounts").unwrap(), deadline())
    });
    assert_eq!(broker.messages(1), [Message::Probe(b"mounts".to_vec())]);
    broker.send_data(&Message::Report(facts.encode()).encode().unwrap(), 7);
    let (client, probed) = joined(probing);
    assert_eq!(probed.unwrap(), ProbeReply::Report(facts));
    let probing = spawn(client, |client| {
        client.probe(&Probe::parse("mapping /a").unwrap(), deadline())
    });
    assert_eq!(broker.messages(1), [Message::Probe(b"mapping /a".to_vec())]);
    let problem = "the worker runs no probe named mapping";
    let failed = Message::ProbeFailed(problem.as_bytes().to_vec());
    broker.send_data(&failed.encode().unwrap(), OUTER_MAX_PAYLOAD);
    let (client, probed) = joined(probing);
    assert_eq!(probed.unwrap(), ProbeReply::Failed(problem.to_owned()));

    // The teardown failed, which the guest reports before it confirms the stop, in one record.
    let failure = RootFailure::new(RootStage::Teardown, "failed to unmount the overlay: busy");
    let stopping = spawn(client, |client| client.stop(deadline()));
    assert_eq!(broker.messages(1), [Message::Stop]);
    let replies = [Message::RootFailed(failure.encode()), Message::Stopped];
    broker.send_data(&encode(&replies), OUTER_MAX_PAYLOAD);
    let (_, stopped) = joined(stopping);
    assert_eq!(stopped.unwrap(), Some(failure));
}

#[test]
fn the_client_returns_the_stage_of_the_root_that_failed() {
    for stage in RootStage::ALL {
        let failure = RootFailure::new(stage, format!("the {stage} failed"));
        let (mut broker, client) = attached(FIRST, usize::MAX);
        let configuring = spawn(client, |client| {
            client.configure_root(&RootConfig::default(), deadline())
        });
        broker.messages(1);
        let reply = Message::RootFailed(failure.encode());
        broker.send_data(&reply.encode().unwrap(), OUTER_MAX_PAYLOAD);
        let (_, configured) = joined(configuring);
        assert_eq!(configured.unwrap(), RootReply::Failed(failure));
    }

    // A root configuration that the framing refuses fails before it is sent, and the client
    // stays usable.
    let (mut broker, mut client) = attached(FIRST, usize::MAX);
    let config = RootConfig {
        runtime: format!("/{}", "a".repeat(MAX_TEXT_BODY)),
        ..RootConfig::default()
    };
    let refused = client.configure_root(&config, deadline());
    let too_long = matches!(
        refused,
        Err(ClientError::Frame(FrameError::Length { kind: 6, .. }))
    );
    assert!(too_long, "{refused:?}");
    let pinging = spawn(client, |client| client.ping(deadline()));
    assert_eq!(broker.answer(OUTER_MAX_PAYLOAD), Message::Ping);
    joined(pinging).1.unwrap();
}

#[test]
fn answers_whose_bodies_do_not_decode_end_the_client() {
    /// A request of the client.
    type Request = fn(&mut Client) -> Result<(), ClientError>;
    let configure: Request = |client| {
        client
            .configure_root(&RootConfig::default(), deadline())
            .map(drop)
    };
    let probe: Request = |client| {
        let probe = Probe::parse("mounts").unwrap();
        client.probe(&probe, deadline()).map(drop)
    };
    let stop: Request = |client| client.stop(deadline()).map(drop);
    let map: Request = |client| client.map(&maps_part(), deadline()).map(drop);
    let activate: Request = |client| client.activate(&activation_record(), deadline()).map(drop);
    let mut partial_activation = ready().encode();
    partial_activation.extend([0; 4]);
    let cases: [(&str, Request, Message, &str); 12] = [
        (
            "a readiness without a build",
            configure,
            Message::Ready(Vec::new()),
            "the guest's readiness names no build",
        ),
        (
            "a readiness that ends before its build does",
            configure,
            Message::Ready(b"\x090.1.0".to_vec()),
            "the guest's readiness ends early",
        ),
        (
            "a readiness with a build that holds a space",
            configure,
            Message::Ready(b"\x030 1".to_vec()),
            "without a space: \"0 1\"",
        ),
        (
            "a root's failure without a stage",
            configure,
            Message::RootFailed(Vec::new()),
            "names no stage",
        ),
        (
            "a root's failure of an unknown stage",
            configure,
            Message::RootFailed(vec![9]),
            "the unknown stage 9",
        ),
        (
            "facts without a value",
            probe,
            Message::Report(b"root.type\n".to_vec()),
            "has no value",
        ),
        (
            "facts with an invalid escape",
            probe,
            Message::Report(b"root.type=a\\b\n".to_vec()),
            "has an invalid escape",
        ),
        (
            "a probe's failure that is not UTF-8 text",
            probe,
            Message::ProbeFailed(vec![0xff]),
            "the probe's failure is not UTF-8 text",
        ),
        (
            "a teardown's failure that is not UTF-8 text",
            stop,
            Message::RootFailed(vec![5, 0xff]),
            "is not UTF-8 text",
        ),
        (
            "a mapping's failure that ends before its count does",
            map,
            Message::MapFailed(vec![1, 0, 0]),
            "a failure of the mapping table ends early",
        ),
        (
            "a mapping's failure that is not UTF-8 text",
            map,
            Message::MapFailed(vec![0, 0, 0, 0, 0xff]),
            "a failure of the mapping table is not UTF-8 text",
        ),
        (
            "a clone's readiness with bytes after its build that are not an activation",
            activate,
            Message::Ready(partial_activation),
            "has 4 bytes after its build, which are not an activation",
        ),
    ];
    for (name, request, reply, problem) in cases {
        let (mut broker, client) = attached(FIRST, usize::MAX);
        let requesting = spawn(client, request);
        broker.messages(1);
        broker.send_data(&reply.encode().unwrap(), OUTER_MAX_PAYLOAD);
        let (mut client, result) = joined(requesting);
        let failed = matches!(
            &result,
            Err(ClientError::Body(error)) if error.problem().contains(problem)
        );
        assert!(failed, "{name}: {result:?}");
        assert_ended(&mut client);
    }
}

#[test]
fn the_host_orders_a_layered_root_ready_then_its_table_in_parts_then_start() {
    let long = long_table();
    assert!(encode_maps(&long).unwrap().len() >= 3);
    // Tables of no mapping, which sends no part, of one, of several in one part, and of several
    // parts.
    let tables = [
        Vec::new(),
        vec![mapping("0", "/srv/dir", true)],
        vec![
            mapping("0", "/srv/dir", true),
            mapping("1", "/srv/file.txt", true),
            mapping("0/inner", "/srv/dir/inner", false),
        ],
        long,
    ];
    for table in tables {
        let count = table.len();
        let parts = encode_maps(&table).unwrap();
        let (mut broker, client) = attached(FIRST, usize::MAX);
        let ordering = order(client, table.clone());
        // The root configuration announces the table, and the host waits for the guest's
        // readiness.
        let config = Message::RootConfig(announcing(&table).encode());
        assert_eq!(broker.messages(1), [config], "{count} mappings");
        broker.assert_quiet();
        broker.send_data(&ready_message().encode().unwrap(), OUTER_MAX_PAYLOAD);
        // Then each part of the table, once the guest applied the one before, then `Start`.
        for part in parts {
            assert_eq!(
                broker.messages(1),
                [Message::Maps(part)],
                "{count} mappings"
            );
            broker.assert_quiet();
            broker.send_data(&Message::Mapped.encode().unwrap(), OUTER_MAX_PAYLOAD);
        }
        broker.expect(Message::Start, &Message::Started);
        let (client, ordered) = joined(ordering);
        assert_eq!(ordered.unwrap(), ready(), "{count} mappings");

        // The client stays usable.
        let pinging = spawn(client, |client| client.ping(deadline()));
        assert_eq!(broker.answer(OUTER_MAX_PAYLOAD), Message::Ping);
        joined(pinging).1.unwrap();
    }

    // The client returns how the guest took a part of the table.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let applying = spawn(client, |client| client.map(&maps_part(), deadline()));
    broker.expect(Message::Maps(maps_part()), &Message::Mapped);
    let (client, mapped) = joined(applying);
    assert_eq!(mapped.unwrap(), MapReply::Mapped);
    let applying = spawn(client, |client| client.map(&maps_part(), deadline()));
    let failure = MapFailure::new(0, "the target /srv/dir is not a directory");
    broker.expect(
        Message::Maps(maps_part()),
        &Message::MapFailed(failure.encode()),
    );
    assert_eq!(joined(applying).1.unwrap(), MapReply::Failed(failure));

    // The whole order, a `Ping`, and a stop, in records of 7 bytes each way.
    let table = vec![
        mapping("0", "/srv/dir", true),
        mapping("0/inner", "/srv/dir/inner", false),
    ];
    let (mut broker, transport) = connect(FIRST, 7);
    let serving = thread::spawn(move || {
        broker.attach();
        let served = broker.serve(7);
        assert!(broker.read().is_none());
        served
    });
    let mut client = Client::attach(transport, &CAPABILITY, deadline()).unwrap();
    let readiness = host::ready(&mut client, &announcing(&table), GUEST_BUILD, deadline()).unwrap();
    assert_eq!(readiness.build(), GUEST_BUILD);
    host::start_sandbox(&mut client, &table, deadline()).unwrap();
    client.ping(deadline()).unwrap();
    assert_eq!(client.stop(deadline()).unwrap(), None);
    drop(client);
    let mut expected = vec![Message::RootConfig(announcing(&table).encode())];
    expected.extend(encode_maps(&table).unwrap().into_iter().map(Message::Maps));
    expected.extend([Message::Start, Message::Ping, Message::Stop]);
    assert_eq!(joined(serving), expected);
}

#[test]
fn the_host_sends_nothing_more_to_a_guest_agent_of_another_build_or_whose_root_failed() {
    let table = vec![mapping("0", "/srv/dir", true)];
    let config = || Message::RootConfig(announcing(&table).encode());

    // A guest agent of another build assembled the root: the host sends it neither the table nor
    // `Start`.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let ordering = order(client, table.clone());
    let another = Ready::new("another-build").unwrap();
    broker.expect(config(), &Message::Ready(another.encode()));
    let error = ended(broker, ordering);
    let other = matches!(
        &error,
        OrderError::Build { reported, expected }
            if reported == "another-build" && expected == GUEST_BUILD
    );
    assert!(other, "{error:?}");
    assert_eq!(
        error.to_string(),
        format!("the guest agent is of build another-build, not of the host's build {GUEST_BUILD}")
    );

    // The guest reports that a stage of the root failed, after which it ends.
    for stage in RootStage::ALL {
        let failure = RootFailure::new(stage, format!("the {stage} failed"));
        let (mut broker, client) = attached(FIRST, usize::MAX);
        let ordering = order(client, table.clone());
        broker.expect(config(), &Message::RootFailed(failure.encode()));
        let error = ended(broker, ordering);
        let failed = matches!(&error, OrderError::Root(reported) if *reported == failure);
        assert!(failed, "{stage}: {error:?}");
        assert_eq!(error.to_string(), failure.to_string());
    }

    // The client failed, as when the guest agent answers the root configuration with another
    // kind.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let ordering = order(client, table.clone());
    broker.expect(config(), &Message::Pong);
    let error = ended(broker, ordering);
    let failed = matches!(
        error,
        OrderError::Client(ClientError::Reply {
            request: 6,
            reply: 2
        })
    );
    assert!(failed, "{error:?}");

    // A table that does not encode fails before the host sends any of it, and the client stays
    // usable.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let too_many = vec![mapping("a", "/a", false); MAX_HOST_MAPPINGS + 1];
    let starting = spawn(client, move |client| {
        host::start_sandbox(client, &too_many, deadline())
    });
    let (client, started) = joined(starting);
    let refused = matches!(
        &started,
        Err(OrderError::Table(error))
            if error.problem().starts_with("the mapping table does not encode")
    );
    assert!(refused, "{started:?}");
    let pinging = spawn(client, |client| client.ping(deadline()));
    assert_eq!(broker.answer(OUTER_MAX_PAYLOAD), Message::Ping);
    joined(pinging).1.unwrap();
}

#[test]
fn a_mapping_that_the_guest_fails_ends_the_table_and_the_host_stops_the_guest() {
    let table = long_table();
    let parts = encode_maps(&table).unwrap();
    let config = Message::RootConfig(announcing(&table).encode());
    let teardown = RootFailure::new(RootStage::Teardown, "failed to unmount /srv/000: busy");
    // The guest fails the first, a middle, or the last part, then confirms the stop, after a
    // teardown that succeeded or failed.
    let cases = [
        (0, None),
        (parts.len() / 2, None),
        (parts.len() - 1, None),
        (parts.len() / 2, Some(teardown)),
    ];
    for (failing, teardown) in cases {
        let (mut broker, client) = attached(FIRST, usize::MAX);
        let ordering = order(client, table.clone());
        broker.expect(config.clone(), &ready_message());
        for part in &parts[..failing] {
            broker.expect(Message::Maps(part.clone()), &Message::Mapped);
        }
        // The guest applied the mappings before the failing part.
        let applied = decode_maps(&parts[failing]).unwrap().first;
        let failure = MapFailure::new(applied, "the target is not a directory");
        broker.expect(
            Message::Maps(parts[failing].clone()),
            &Message::MapFailed(failure.encode()),
        );
        // The host sends no more of the table, nor `Start`, but asks the guest to stop.
        assert_eq!(broker.messages(1), [Message::Stop], "part {failing}");
        let mut replies = Vec::new();
        replies.extend(
            teardown
                .iter()
                .map(|teardown| Message::RootFailed(teardown.encode())),
        );
        replies.push(Message::Stopped);
        broker.send_data(&encode(&replies), OUTER_MAX_PAYLOAD);
        let error = ended(broker, ordering);
        let mut description = failure.to_string();
        if let Some(teardown) = &teardown {
            description = format!("{description}; then {teardown}");
        }
        assert_eq!(error.to_string(), description);
        let OrderError::Mapping {
            failure: found,
            stop,
        } = &error
        else {
            panic!("part {failing}: {error:?}");
        };
        assert_eq!(*found, failure, "part {failing}");
        assert!(matches!(stop, Ok(found) if *found == teardown), "{stop:?}");
    }

    // A guest agent that does not confirm the stop, as when its session ends, fails the stop,
    // which the failure reports.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let ordering = order(client, table.clone());
    broker.expect(config, &ready_message());
    let failure = MapFailure::new(0, "the target is not a directory");
    broker.expect(
        Message::Maps(parts[0].clone()),
        &Message::MapFailed(failure.encode()),
    );
    assert_eq!(broker.messages(1), [Message::Stop]);
    broker.close();
    let error = ended(broker, ordering);
    let OrderError::Mapping {
        failure: found,
        stop,
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(*found, failure);
    assert!(matches!(stop, Err(ClientError::Session(_))), "{stop:?}");
    let description = error.to_string();
    assert!(
        description.starts_with(&format!(
            "{failure}; the guest agent did not confirm the stop: "
        )),
        "{description}"
    );
}

/// Returns the bytes that stand for a clone's activation record in its canonical encoding, which
/// the host's client carries in an `Activate` without decoding them.
fn activation_record() -> Vec<u8> {
    b"\x01the clone's activation record".to_vec()
}

/// Returns the activation that the guest of a clone reports in its `Ready`.
fn clone_activation() -> ReadyActivation {
    ReadyActivation {
        transaction: Some(TransactionId::from_bytes([0x4d; TransactionId::LEN])),
    }
}

/// Returns the `Ready` of a clone, which reports [`clone_activation`] after the guest agent's build
/// that the host program ships.
fn clone_ready() -> Message {
    Message::Ready(ready().with_activation(clone_activation()).encode())
}

/// Returns the order of a clone of the guest agent's build that the host program ships.
fn clone_order() -> CloneOrder<'static> {
    CloneOrder { build: GUEST_BUILD }
}

/// Runs the first step of a clone's order with `client` in a thread: sends [`activation_record`] in an
/// `Activate`. The thread returns the client with the activation that `Ready` reports, or how the
/// step failed.
fn activating(client: Client) -> JoinHandle<(Client, Result<Option<ReadyActivation>, OrderError>)> {
    spawn(client, |client| {
        clone_order().activate(client, &activation_record(), deadline())
    })
}

#[test]
fn a_clone_s_order_sends_its_record_first_then_starts_its_sandbox_as_after_a_cold_boot() {
    // The broker waits for the guest's acknowledgement of the restore's reset, the session's
    // bootstrap, before it attaches the host, which waits for it.
    let (mut broker, transport) = connect(FIRST, usize::MAX);
    let attaching = thread::spawn(move || clone_order().attach(transport, &CAPABILITY, deadline()));
    broker.accept();
    for _ in 0..3 {
        broker.send(RecordType::Wait, &[]);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!attaching.is_finished(), "the order waits for READY");
    broker.send(RecordType::Ready, &[]);
    let client = joined(attaching).unwrap();
    assert_eq!(client.identity(), FIRST);

    // The record is the session's first request, after which the host waits for `Ready`, which
    // arrives in records of 7 bytes.
    let activation = activating(client);
    assert_eq!(broker.messages(1), [Message::Activate(activation_record())]);
    broker.assert_quiet();
    broker.send_data(&clone_ready().encode().unwrap(), 7);
    let (client, activated) = joined(activation);
    assert_eq!(activated.unwrap(), Some(clone_activation()));

    // Then the table that the restore sequence gives the order, in its parts, each once the guest
    // applied the one before, and `Start`, which the sequence asks for once the table succeeded.
    let table = long_table();
    let parts = encode_maps(&table).unwrap();
    let mapping = spawn(client, move |client| {
        clone_order().map(client, &table, deadline())
    });
    for part in parts {
        broker.expect(Message::Maps(part), &Message::Mapped);
    }
    let (client, mapped) = joined(mapping);
    mapped.unwrap();
    broker.assert_quiet();
    let starting = spawn(client, |client| clone_order().start(client, deadline()));
    broker.expect(Message::Start, &Message::Started);
    let (client, started) = joined(starting);
    started.unwrap();
    drop(client);
    assert!(broker.read().is_none(), "the host sent more");

    // A `Ready` that reports no activation, as a cold boot's does, is returned as none, for the
    // restore sequence to refuse; `host::activate` returns the readiness with its build.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let activation = activating(client);
    broker.expect(Message::Activate(activation_record()), &ready_message());
    let (client, activated) = joined(activation);
    assert_eq!(activated.unwrap(), None);
    let activation = spawn(client, |client| {
        host::activate(client, &activation_record(), GUEST_BUILD, deadline())
    });
    broker.expect(Message::Activate(activation_record()), &clone_ready());
    let readiness = joined(activation).1.unwrap();
    assert_eq!(readiness.build(), GUEST_BUILD);
    assert_eq!(readiness.activation(), Some(clone_activation()));
}

#[test]
fn a_clone_s_order_ends_its_table_at_a_mapping_that_the_guest_fails_and_stops_the_guest() {
    // As after a cold boot, the guest fails a part of the table, once it applied the mappings
    // before it: the order sends no more of the table, and asks the guest to stop, and the restore
    // sequence, which the failure ends, sends no `Start`.
    let table = long_table();
    let parts = encode_maps(&table).unwrap();
    assert!(parts.len() > 2);
    let (mut broker, client) = attached(FIRST, usize::MAX);
    // The table follows the clone's `Ready`, which answers its record.
    let activation = activating(client);
    broker.expect(Message::Activate(activation_record()), &clone_ready());
    let (client, activated) = joined(activation);
    assert_eq!(activated.unwrap(), Some(clone_activation()));
    let mapping = spawn(client, move |client| {
        clone_order().map(client, &table, deadline())
    });
    broker.expect(Message::Maps(parts[0].clone()), &Message::Mapped);
    let applied = decode_maps(&parts[1]).unwrap().first;
    let failure = MapFailure::new(applied, "the target is not a directory");
    broker.expect(
        Message::Maps(parts[1].clone()),
        &Message::MapFailed(failure.encode()),
    );
    assert_eq!(broker.messages(1), [Message::Stop]);
    broker.send_data(&encode(&[Message::Stopped]), OUTER_MAX_PAYLOAD);
    let (client, mapped) = joined(mapping);
    let error = mapped.unwrap_err();
    let OrderError::Mapping {
        failure: found,
        stop,
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(*found, failure);
    assert!(matches!(stop, Ok(None)), "{stop:?}");
    // The guest's refusal of the table keeps the session, but does not tell whether the request's
    // mappings failed it, so the order takes it for no refusal of the request's.
    assert!(!clone_order().lost(&error));
    assert!(!clone_order().refused(&error));
    drop(client);
    assert!(broker.read().is_none(), "the host sent more");
}

#[test]
fn a_clone_s_order_maps_its_export_s_children_by_their_indices() {
    // A clone's table is a cold launch's: its sources name each path by the index of the export's
    // child that holds it, which the order sends in its part, as the guest decodes it, and which
    // the guest applies.
    let table = vec![
        mapping("0", "/data", true),
        mapping("1", "/notes.txt", false),
        mapping("0/work", "/work", false),
    ];
    let parts = encode_maps(&table).unwrap();
    assert_eq!(parts.len(), 1);
    let decoded = decode_maps(&parts[0]).unwrap();
    assert_eq!(decoded.first, 0);
    assert_eq!(
        decoded
            .maps
            .iter()
            .map(|map| (map.source.as_slice(), map.target.as_slice(), map.read_only))
            .collect::<Vec<_>>(),
        [
            (b"0".as_slice(), b"/data".as_slice(), true),
            (b"1".as_slice(), b"/notes.txt".as_slice(), false),
            (b"0/work".as_slice(), b"/work".as_slice(), false),
        ]
    );
    let (mut broker, client) = attached(FIRST, usize::MAX);
    // The table follows the clone's `Ready`, which answers its record.
    let activation = activating(client);
    broker.expect(Message::Activate(activation_record()), &clone_ready());
    let (client, activated) = joined(activation);
    assert_eq!(activated.unwrap(), Some(clone_activation()));
    let mapping = spawn(client, move |client| {
        clone_order().map(client, &table, deadline())
    });
    broker.expect(Message::Maps(parts[0].clone()), &Message::Mapped);
    let (client, mapped) = joined(mapping);
    mapped.unwrap();
    drop(client);
    assert!(broker.read().is_none(), "the host sent more");
}

#[test]
fn a_clone_s_session_that_ends_after_its_bootstrap_is_lost_and_the_next_takes_the_same_record() {
    let next = Identity { epoch: 2, ..FIRST };
    // The broker resets the session, which a broker announces, or ends its stream, as OpenVMM's
    // broker ends an attachment, while the host awaits `Ready`; or the transport fails as the
    // host sends the record.
    for ending in ["reset", "end of stream", "transport"] {
        let (mut broker, client) = attached(FIRST, usize::MAX);
        if ending == "transport" {
            broker.from_host.close();
        }
        let activation = activating(client);
        match ending {
            "reset" => {
                assert_eq!(broker.messages(1), [Message::Activate(activation_record())]);
                broker.identity = next;
                broker.send(RecordType::Reset, &[]);
            }
            "end of stream" => {
                assert_eq!(broker.messages(1), [Message::Activate(activation_record())]);
                broker.close();
            }
            _ => {}
        }
        let (mut client, activated) = joined(activation);
        let error = activated.unwrap_err();
        assert!(clone_order().lost(&error), "{ending}: {error:?}");
        assert_ended(&mut client);

        // The order's next session sends the same record, byte for byte, as its first request,
        // and the clone, which was ready, answers it with the same `Ready`.
        let (mut broker, client) = attached(next, usize::MAX);
        let activation = activating(client);
        broker.expect(Message::Activate(activation_record()), &clone_ready());
        let (_, activated) = joined(activation);
        assert_eq!(activated.unwrap(), Some(clone_activation()), "{ending}");
    }
}

#[test]
fn only_a_session_that_ended_is_lost_to_a_clone_s_order() {
    let not_lost = |error: &OrderError| !clone_order().lost(error);

    // The broker refuses the capability with an ERROR.
    let (mut broker, transport) = connect(FIRST, usize::MAX);
    let attaching = thread::spawn(move || clone_order().attach(transport, &CAPABILITY, deadline()));
    broker.accept();
    broker.send(
        RecordType::Error,
        &ErrorCode::Authentication.code().to_le_bytes(),
    );
    let error = joined(attaching).unwrap_err();
    let rejected = matches!(
        error,
        OrderError::Client(ClientError::Session(HostConnectionError::Rejected(
            ErrorCode::Authentication
        )))
    );
    assert!(rejected && not_lost(&error), "{error:?}");

    // The deadline passes while the broker waits for the guest.
    let (mut broker, transport) = connect(FIRST, usize::MAX);
    broker.send(RecordType::Wait, &[]);
    let attaching =
        thread::spawn(move || clone_order().attach(transport, &CAPABILITY, Instant::now() + SHORT));
    let error = joined(attaching).unwrap_err();
    let late = matches!(
        error,
        OrderError::Client(ClientError::Session(HostConnectionError::TimedOut))
    );
    assert!(late && not_lost(&error), "{error:?}");

    // The guest agent that activated the clone is of another build: the host sends it nothing
    // more.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let activation = activating(client);
    let another = Ready::new("another-build")
        .unwrap()
        .with_activation(clone_activation());
    broker.expect(
        Message::Activate(activation_record()),
        &Message::Ready(another.encode()),
    );
    let (client, activated) = joined(activation);
    drop(client);
    assert!(broker.read().is_none(), "the host sent more");
    let error = activated.unwrap_err();
    let other = matches!(
        &error,
        OrderError::Build { reported, expected }
            if reported == "another-build" && expected == GUEST_BUILD
    );
    assert!(other && not_lost(&error), "{error:?}");

    // `Ready` does not arrive in time.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let activation = spawn(client, |client| {
        clone_order().activate(client, &activation_record(), Instant::now() + SHORT)
    });
    assert_eq!(broker.messages(1), [Message::Activate(activation_record())]);
    let (mut client, activated) = joined(activation);
    let error = activated.unwrap_err();
    let late = matches!(
        error,
        OrderError::Client(ClientError::Session(HostConnectionError::TimedOut))
    );
    assert!(late && not_lost(&error), "{error:?}");
    assert_ended(&mut client);

    // The guest answers the record with another kind, or with a `Ready` whose activation does not
    // decode.
    let mut partial = ready().encode();
    partial.push(1);
    for reply in [Message::Pong, Message::Ready(partial)] {
        let (mut broker, client) = attached(FIRST, usize::MAX);
        let activation = activating(client);
        broker.expect(Message::Activate(activation_record()), &reply);
        let (mut client, activated) = joined(activation);
        let error = activated.unwrap_err();
        let failed = matches!(
            &error,
            OrderError::Client(ClientError::Reply {
                request: 27,
                reply: 2
            }) | OrderError::Client(ClientError::Body(_))
        );
        assert!(failed && not_lost(&error), "{reply:?}: {error:?}");
        assert_ended(&mut client);
    }

    // A record that the framing refuses fails before it is sent, and the client stays usable.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let activation = spawn(client, |client| {
        let too_long = vec![0; MAX_ACTIVATE_BODY + 1];
        clone_order().activate(client, &too_long, deadline())
    });
    let (client, activated) = joined(activation);
    let error = activated.unwrap_err();
    let expected = FrameError::Length {
        kind: 27,
        length: MAX_ACTIVATE_BODY + 1,
    };
    let refused =
        matches!(&error, OrderError::Client(ClientError::Frame(found)) if *found == expected);
    assert!(refused && not_lost(&error), "{error:?}");
    let pinging = spawn(client, |client| client.ping(deadline()));
    assert_eq!(broker.answer(OUTER_MAX_PAYLOAD), Message::Ping);
    joined(pinging).1.unwrap();
}

/// The chunk of the executions of the tests of the host's driver.
const CHUNK: u32 = 16 * 1024;

/// Returns the limits of the executions of the tests of the host's driver, as the host program
/// chooses them: chunks of 16 KiB, and frame-counted credit, of 4 frames of input, and of 8 frames
/// of output in one pool that standard output and error share.
fn limits() -> Limits {
    Limits::new(CHUNK, Window::frames(4), Window::frames(8))
        .unwrap()
        .shared_output()
}

/// Returns the timers of both sides of the executions of the tests of the host's driver.
fn timers() -> Timers {
    Timers::new(Duration::from_millis(300), Duration::from_secs(5))
}

/// Returns the start of the execution `id` of the first generation, of a program that the test's
/// guest plays.
fn start_of(id: u64) -> Start {
    Start::new(Key::new(id, 1), limits(), timers(), ["/bin/program"])
}

/// Returns the position of `stream` among [`Stream::ALL`].
fn position(stream: Stream) -> usize {
    match stream {
        Stream::Stdout => 0,
        Stream::Stderr => 1,
    }
}

/// What the program of an execution that the test's guest plays does.
#[derive(Clone, Debug)]
struct Workload {
    /// Whether the program starts, or why its launch failed.
    launch: Result<(), LaunchFailure>,
    /// What the program writes to its standard output and error, in the order of [`Stream::ALL`].
    output: [Vec<u8>; 2],
    /// How the program ends, once it wrote its output.
    status: Status,
}

/// The guest's side of an execution that the test's guest plays: the runner of NVX's execution
/// engine, which takes the program's output as its credit lets it, as the worker's does.
struct Played {
    /// The guest's side of the execution.
    runner: Runner,
    /// What the program does.
    workload: Workload,
    /// Whether the runner learned how the launch went.
    launched: bool,
    /// The bytes of each stream that the runner took.
    written: [usize; 2],
    /// Whether the program ended.
    ended: bool,
}

impl Played {
    /// Returns the guest's side of the execution of `runner`, whose program does `workload`.
    fn new(runner: Runner, workload: Workload) -> Self {
        Self {
            runner,
            workload,
            launched: false,
            written: [0, 0],
            ended: false,
        }
    }

    /// Takes every step that the program and the runner can take at `now`, as the worker takes
    /// them, and returns the runner's messages, as frames.
    fn step(&mut self, now: Instant) -> Vec<Message> {
        if !self.launched {
            self.launched = true;
            self.runner.launched(self.workload.launch);
            if self.workload.launch.is_err() {
                for stream in Stream::ALL {
                    self.runner.output_closed(stream);
                }
                self.runner.exited(now, None);
                self.ended = true;
            }
        }
        if !self.ended {
            for stream in Stream::ALL {
                let output = &self.workload.output[position(stream)];
                let written = &mut self.written[position(stream)];
                *written += self.runner.output(stream, &output[*written..]);
            }
            if self.written == self.workload.output.each_ref().map(Vec::len) {
                for stream in Stream::ALL {
                    self.runner.output_closed(stream);
                }
                self.runner.exited(now, Some(self.workload.status));
                self.ended = true;
            }
        }
        self.runner.tick(now);
        while let Some(request) = self.runner.next_request() {
            if let runner::Request::Settle = request {
                self.runner.settled(match self.workload.launch {
                    Ok(()) => Settlement::Settled,
                    Err(_) => Settlement::Failed,
                });
            }
        }
        let key = self.runner.key();
        let mut buffer = vec![0; self.runner.limits().chunk()];
        let mut messages = Vec::new();
        while let Some(message) = self.runner.next_message(&mut buffer) {
            if let Some(message) = GuestMessage::from_runner(key, message, &buffer) {
                messages.push(message.encode());
            }
        }
        messages
    }
}

/// Plays the guest's side of the host's executions until `count` of them finished: a runner for
/// each start, whose program does what `workload` says of the start. Takes the host's messages,
/// sends the runners' in DATA records of at most `size` bytes, and returns the starts, in order.
fn play(
    broker: &mut Broker,
    count: usize,
    size: usize,
    workload: impl Fn(&Start) -> Workload,
) -> Vec<Start> {
    let mut starts = Vec::new();
    let mut played = BTreeMap::<u64, Played>::new();
    loop {
        let now = Instant::now();
        let messages = played
            .values_mut()
            .flat_map(|played| played.step(now))
            .collect::<Vec<_>>();
        broker.send_data(&encode(&messages), size);
        if played.len() == count && played.values().all(|played| played.runner.is_finished()) {
            return starts;
        }
        for message in broker.take() {
            let decoded = HostMessage::decode(&message)
                .expect("the host sends messages about executions")
                .expect("the host's messages decode");
            let message = match decoded {
                HostMessage::Start(start) => {
                    let runner = Runner::new(start.key, start.limits, start.timers, now);
                    played.insert(start.key.id(), Played::new(runner, workload(&start)));
                    starts.push(start);
                    continue;
                }
                message => message,
            };
            let key = message.key();
            let runner = &mut played
                .get_mut(&key.id())
                .expect("the host started the execution")
                .runner;
            match message {
                HostMessage::Credit(key, pool, units) => runner.credit(key, pool, units),
                HostMessage::Input(key, bytes) => runner.input(key, bytes),
                HostMessage::InputEnd(key) => runner.end_input(key),
                HostMessage::Cancel(key, cause) => runner.cancel(now, key, cause),
                HostMessage::Start(_) => unreachable!("the start came first"),
            }
            .expect("the runner accepts the host's message");
        }
    }
}

/// Starts the executions of `starts` with `client`, each with its own timers, and runs them in a
/// thread until `deadline`, which returns the client and the driver.
fn run(
    client: Client,
    starts: Vec<Start>,
    deadline: Instant,
) -> JoinHandle<(Client, Result<Executions, ExecutionsError>)> {
    spawn(client, move |client| {
        let mut executions = Executions::new();
        for start in starts {
            let timers = start.timers;
            executions.start(client, start, timers, deadline)?;
        }
        executions.run(client, deadline)?;
        Ok(executions)
    })
}

#[test]
fn the_driver_runs_executions_at_once_and_collects_their_output_and_outcomes() {
    // The first program writes more to its standard output than the guest's credit holds, so that
    // its output flows as the host takes it, and a little to its standard error, then exits with
    // 3. The second one does not start. The guest's records carry its messages whole, or in
    // pieces that split them, and each read of the host's takes at most 4 KiB.
    let output = (0..=u8::MAX).cycle().take(300 * 1024).collect::<Vec<_>>();
    let written = Workload {
        launch: Ok(()),
        output: [output.clone(), b"error".to_vec()],
        status: Status::Exited(3),
    };
    let refused = Workload {
        launch: Err(LaunchFailure::new(0, 13)),
        output: [Vec::new(), Vec::new()],
        status: Status::Exited(0),
    };
    for size in [OUTER_MAX_PAYLOAD, 1000] {
        let (mut broker, client) = attached(FIRST, 4096);
        let running = run(client, vec![start_of(1), start_of(2)], deadline());
        let starts = play(&mut broker, 2, size, |start| match start.key.id() {
            1 => written.clone(),
            _ => refused.clone(),
        });
        assert_eq!(starts, [start_of(1), start_of(2)]);
        let (_, executions) = joined(running);
        let executions = executions.unwrap();
        assert!(executions.is_finished());
        let ids = executions
            .iter()
            .map(|execution| execution.start().key.id())
            .collect::<Vec<_>>();
        assert_eq!(ids, [1, 2]);

        let first = executions.get(1).unwrap();
        assert!(first.peer().has_started());
        assert_eq!(first.output(Stream::Stdout), output);
        assert_eq!(first.output(Stream::Stderr), b"error");
        let outcome = first.outcome().unwrap();
        assert_eq!(outcome.status(), Some(Status::Exited(3)));
        assert_eq!((outcome.cause(), outcome.launch_failure()), (None, None));
        assert!(outcome.is_reusable());
        for stream in Stream::ALL {
            assert!(!outcome.is_truncated(stream), "{stream:?}");
        }

        let second = executions.get(2).unwrap();
        assert!(!second.peer().has_started());
        let outcome = second.outcome().unwrap();
        assert_eq!(outcome.launch_failure(), Some(LaunchFailure::new(0, 13)));
        assert_eq!(outcome.status(), None);
        assert!(!outcome.is_reusable());
        for stream in Stream::ALL {
            assert!(second.output(stream).is_empty(), "{stream:?}");
        }
    }
}

#[test]
fn the_driver_refuses_starts_before_it_sends_them() {
    let (mut broker, mut client) = attached(FIRST, usize::MAX);
    let mut executions = Executions::new();
    // The guest ends its session on a start that it cannot decode.
    let undecodable = [
        start_of(1).with_working_directory("relative"),
        start_of(1).with_environment(["=value"]),
        Start::new(Key::new(1, 1), limits(), timers(), [""]),
    ];
    for start in undecodable {
        let refused = executions.start(&mut client, start.clone(), timers(), deadline());
        assert!(
            matches!(refused, Err(ExecutionsError::Start(_))),
            "{start:?}: {refused:?}"
        );
    }
    // A start whose input the host would send while it reads the guest's output, and starts whose
    // output windows would let the guest send many messages ahead of its credit for the host's.
    let refused = executions.start(&mut client, start_of(1).with_input(), timers(), deadline());
    assert!(
        matches!(refused, Err(ExecutionsError::Input)),
        "{refused:?}"
    );
    let program = ["/bin/program"];
    for output in [
        Window::bytes(64 * 1024),
        Window::frames(MAX_OUTPUT_FRAMES + 1),
    ] {
        let limits = Limits::new(CHUNK, Window::frames(4), output).unwrap();
        let start = Start::new(Key::new(1, 1), limits, timers(), program);
        let refused = executions.start(&mut client, start, timers(), deadline());
        assert!(
            matches!(refused, Err(ExecutionsError::Window)),
            "{output:?}: {refused:?}"
        );
    }
    // A second start of an ID, and a start beyond the executions that the guest runs at once,
    // after starts of which one has the widest output windows that the driver takes.
    let most = u64::try_from(MAX_EXECUTIONS).unwrap();
    let mut starts = (1..=most).map(start_of).collect::<Vec<_>>();
    let widest = Limits::new(CHUNK, Window::frames(4), Window::frames(MAX_OUTPUT_FRAMES)).unwrap();
    starts[0] = Start::new(Key::new(1, 1), widest, timers(), program);
    for start in &starts {
        executions
            .start(&mut client, start.clone(), timers(), deadline())
            .unwrap();
    }
    let refused = executions.start(&mut client, start_of(1), timers(), deadline());
    assert!(
        matches!(refused, Err(ExecutionsError::Duplicate(1))),
        "{refused:?}"
    );
    let refused = executions.start(&mut client, start_of(most + 1), timers(), deadline());
    assert!(matches!(refused, Err(ExecutionsError::Full)), "{refused:?}");
    // The guest received those starts alone, and the client stays usable.
    let sent = starts
        .into_iter()
        .map(|start| HostMessage::Start(start).encode())
        .collect::<Vec<_>>();
    assert_eq!(broker.messages(MAX_EXECUTIONS), sent);
    let pinging = spawn(client, |client| client.ping(deadline()));
    assert_eq!(broker.answer(OUTER_MAX_PAYLOAD), Message::Ping);
    joined(pinging).1.unwrap();

    // A driver runs one batch: once it ran, it starts nothing more.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let running = run(client, vec![start_of(1)], deadline());
    let refused = Workload {
        launch: Err(LaunchFailure::new(0, 11)),
        output: [Vec::new(), Vec::new()],
        status: Status::Exited(0),
    };
    let played = play(&mut broker, 1, OUTER_MAX_PAYLOAD, |_| refused.clone());
    assert_eq!(played, [start_of(1)]);
    let (mut client, executions) = joined(running);
    let mut executions = executions.unwrap();
    assert!(executions.is_finished());
    let refused = executions.start(&mut client, start_of(2), timers(), deadline());
    assert!(matches!(refused, Err(ExecutionsError::Ran)), "{refused:?}");
}

#[test]
fn the_driver_ends_its_run_on_a_message_that_breaks_the_rules() {
    /// A check of the failure that ended a run.
    type Check = fn(&ExecutionsError) -> bool;
    let key = Key::new(1, 1);
    let key_body = [key.id().to_le_bytes(), key.generation().to_le_bytes()].concat();
    let cases: [(&str, Vec<Message>, Check); 4] = [
        (
            "a message about an execution that the host did not start",
            vec![GuestMessage::Started(Key::new(2, 1)).encode()],
            |error| matches!(error, ExecutionsError::Unknown(key) if *key == Key::new(2, 1)),
        ),
        (
            "a message about no execution",
            vec![Message::Pong],
            |error| matches!(error, ExecutionsError::Unexpected(kind) if *kind == Message::Pong.kind()),
        ),
        (
            "output of a stream that does not exist",
            vec![Message::ExecOutput([&key_body[..], &[9], b"x"].concat())],
            |error| matches!(error, ExecutionsError::Body(_)),
        ),
        (
            "an outcome before the ends of the streams",
            vec![
                GuestMessage::Started(key).encode(),
                GuestMessage::Terminal(key, Outcome::new(Some(Status::Exited(0)))).encode(),
            ],
            |error| {
                matches!(
                    error,
                    ExecutionsError::Violation(failed, Violation::Order) if *failed == Key::new(1, 1)
                )
            },
        ),
    ];
    for (name, messages, check) in cases {
        let (mut broker, client) = attached(FIRST, usize::MAX);
        let running = run(client, vec![start_of(1)], deadline());
        broker.messages(1);
        broker.send_data(&encode(&messages), OUTER_MAX_PAYLOAD);
        let (_, ran) = joined(running);
        let failed = ran.as_ref().err().is_some_and(check);
        assert!(failed, "{name}: {:?}", ran.err());
    }

    // The end of the guest's stream ends the run too.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let running = run(client, vec![start_of(1)], deadline());
    broker.messages(1);
    broker.close();
    let (_, ran) = joined(running);
    let closed = matches!(
        ran,
        Err(ExecutionsError::Client(ClientError::Session(
            HostConnectionError::Closed
        )))
    );
    assert!(closed, "{:?}", ran.err());
}

#[test]
fn the_driver_fires_the_timers_of_its_executions_while_the_guest_says_nothing() {
    // The execution's timeout passes while the guest sends nothing: the host cancels it, waits for
    // its outcome for the grace and the settlement time, then gives up on it, and fences it.
    let (grace, settlement, timeout) = (
        Duration::from_millis(50),
        Duration::from_millis(100),
        Duration::from_millis(200),
    );
    let timed = Timers::new(grace, settlement).timeout(timeout);
    let key = Key::new(1, 1);
    let start = Start::new(key, limits(), timed, ["/bin/program"]);
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let started = Instant::now();
    let running = run(client, vec![start.clone()], deadline());
    // The start, the credit of the output's pool, then the cancellation.
    let messages = broker.messages(3);
    assert_eq!(messages[0], HostMessage::Start(start).encode());
    assert_eq!(
        messages[2],
        HostMessage::Cancel(key, Cause::TimedOut).encode()
    );
    let (_, executions) = joined(running);
    assert!(started.elapsed() >= timeout + grace + settlement);
    let executions = executions.unwrap();
    let outcome = executions.get(1).unwrap().outcome().unwrap();
    assert_eq!(outcome.cause(), Some(Cause::TimedOut));
    assert_eq!(outcome.status(), None);
    assert!(!outcome.is_reusable());

    // A run whose deadline passes before its executions finish fails, in a receive, which leaves
    // the client usable: the guest takes the start's credit, then answers a ping.
    let (mut broker, client) = attached(FIRST, usize::MAX);
    let running = run(client, vec![start_of(1)], Instant::now() + SHORT);
    broker.messages(1);
    let (client, ran) = joined(running);
    assert!(
        matches!(ran, Err(ExecutionsError::TimedOut)),
        "{:?}",
        ran.err()
    );
    broker.messages(1);
    let pinging = spawn(client, |client| client.ping(deadline()));
    assert_eq!(broker.answer(OUTER_MAX_PAYLOAD), Message::Ping);
    joined(pinging).1.unwrap();
}

#[test]
fn the_driver_s_sends_wait_for_a_guest_that_stops_reading_until_the_deadline_of_its_run() {
    // The guest takes the start, then stops reading, and the host's stream holds 16 bytes, so the
    // driver's next send, the credit of the output's pool, waits for the transport until the
    // run's deadline, whose timeout ends the client. The run's deadline counts from the start's
    // send, which the guest took.
    let (mut broker, client) = attach_over(connect_bounded(FIRST, usize::MAX, 16));
    let running = spawn(client, |client| {
        let mut executions = Executions::new();
        executions.start(client, start_of(1), timers(), deadline())?;
        let started = Instant::now();
        let ran = executions.run(client, started + SHORT);
        Ok::<_, ExecutionsError>((started.elapsed(), ran))
    });
    assert_eq!(
        broker.messages(1),
        [HostMessage::Start(start_of(1)).encode()]
    );
    let (mut client, result) = joined(running);
    let (elapsed, ran) = result.unwrap();
    assert!(elapsed >= SHORT, "{elapsed:?}");
    assert!(
        matches!(ran, Err(ExecutionsError::TimedOut)),
        "{:?}",
        ran.err()
    );
    assert_ended(&mut client);
}

#[test]
fn a_stop_skips_the_guest_s_late_messages_about_the_executions_that_the_driver_gave_up_on() {
    // The execution's timeout passes while the guest sends nothing, so the driver gives up on it,
    // and finishes it without the guest's messages about it, which arrive once the host stops the
    // guest agent in the same attachment.
    let timed = Timers::new(Duration::from_millis(50), Duration::from_millis(100))
        .timeout(Duration::from_millis(200));
    let key = Key::new(1, 1);
    let start = Start::new(key, limits(), timed, ["/bin/program"]);
    let given_up = || {
        let (mut broker, client) = attached(FIRST, usize::MAX);
        let running = run(client, vec![start.clone()], deadline());
        // The start, the credit of the output's pool, then the cancellation.
        broker.messages(3);
        let (client, executions) = joined(running);
        let executions = executions.unwrap();
        let outcome = executions.get(1).unwrap().outcome().unwrap();
        assert_eq!(outcome.status(), None);
        (broker, client, executions)
    };
    let late = [
        GuestMessage::Output(key, Stream::Stdout, b"late").encode(),
        GuestMessage::End(key, Stream::Stdout).encode(),
        GuestMessage::Terminal(key, Outcome::new(Some(Status::Exited(0)))).encode(),
    ];
    let teardown = || RootFailure::new(RootStage::Teardown, "busy");

    // The guest's late messages, then `Stopped`, and, in records of 7 bytes, late messages before
    // and after a teardown that failed, which the stop returns.
    let cases = [
        ([&late[..], &[Message::Stopped]].concat(), None),
        (
            [
                &late[..1],
                &[Message::RootFailed(teardown().encode())],
                &late[1..],
                &[Message::Stopped],
            ]
            .concat(),
            Some(teardown()),
        ),
    ];
    for ((replies, expected), size) in cases.into_iter().zip([OUTER_MAX_PAYLOAD, 7]) {
        let (mut broker, client, executions) = given_up();
        let stopping = spawn(client, move |client| {
            client.stop_skipping(deadline(), |message| executions.is_about(message))
        });
        assert_eq!(broker.messages(1), [Message::Stop]);
        broker.send_data(&encode(&replies), size);
        let (_, stopped) = joined(stopping);
        assert_eq!(stopped.unwrap(), expected);
    }

    // `stop` takes the first late message for its answer, and neither skips a message about an
    // execution that the driver did not start, of another ID, or of its ID in another epoch: each
    // ends the client.
    let others = [
        GuestMessage::Terminal(Key::new(2, 1), Outcome::new(Some(Status::Exited(0)))).encode(),
        GuestMessage::Started(Key::new(1, 2)).encode(),
    ];
    let cases = [(false, late[0].clone())]
        .into_iter()
        .chain(others.into_iter().map(|other| (true, other)));
    for (skipping, reply) in cases {
        let (mut broker, client, executions) = given_up();
        let stopping = spawn(client, move |client| {
            if skipping {
                client.stop_skipping(deadline(), |message| executions.is_about(message))
            } else {
                client.stop(deadline())
            }
        });
        assert_eq!(broker.messages(1), [Message::Stop]);
        broker.send_data(
            &encode(&[reply.clone(), Message::Stopped]),
            OUTER_MAX_PAYLOAD,
        );
        let (mut client, stopped) = joined(stopping);
        let error = stopped.unwrap_err();
        let kinds = (Message::Stop.kind(), reply.kind());
        assert!(
            matches!(error, ClientError::Reply { request, reply } if (request, reply) == kinds),
            "{skipping}: {error:?}"
        );
        assert_ended(&mut client);
    }
}
