//! The adapter's host program attaches to the control session between the host and the guest, and
//! exchanges the adapter's own framing with the guest agent, through public APIs only.
//!
//! A test broker plays the broker's host-facing end of the session, and the guest agent behind it,
//! with the public record API of `nvx`, over an in-memory transport, so that the tests run on Linux
//! and on Windows. The tests cover the attachment and its bootstrap identity, waits and refusals,
//! messages that span records and records that share messages, partial records, record lengths,
//! ERROR payloads, sequence and identity errors, resets and disconnects in the middle of a message,
//! timeouts, sends that fail, the answers that requests check, the bounded buffer of the adapter's
//! decoder, and the requests of a layered root: its configuration, its probes, and a stop whose
//! teardown failed.
#![cfg(feature = "host")]

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nvx::host::nvxs::HostConnectionError;
use nvx::host::platform::Transport;
use nvx::protocol::nvxs::{
    ErrorCode, Identity, Record, RecordError, RecordParser, RecordType, SessionError, encode_record,
};
use nvx::protocol::{
    CAPABILITY_LEN, HostMap, OUTER_HEADER_LEN, OUTER_MAX_PAYLOAD, OUTER_MIN_RECEIVE_WINDOW,
};
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::frame::{Decoder, FrameError, HEADER_LEN, MAX_BODY, MAX_TEXT_BODY, Message};
use runtime_adapter::host::{Client, ClientError};
use runtime_adapter::probe::{Facts, Probe, ProbeReply};
use runtime_adapter::root::{Ready, RootConfig, RootFailure, RootReply, RootStage, encode_maps};

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

/// Returns the guest agent's answer to `message`.
fn answer(message: &Message) -> Message {
    match message {
        Message::Ping => Message::Pong,
        Message::Echo(body) => Message::Echo(body.clone()),
        Message::Stop => Message::Stopped,
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
    let table = [HostMap {
        source: "dir".to_owned(),
        target: "/srv/dir".to_owned(),
        read_only: true,
    }];
    encode_maps(&table).unwrap().remove(0)
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
        (header([0; 4], 27), FrameError::Kind(27)),
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
    let host_messages = [
        Message::Ping,
        Message::Stop,
        Message::RootConfig(RootConfig::default().encode()),
        Message::Probe(b"mounts".to_vec()),
        Message::Maps(maps_part()),
        Message::Start,
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
    let teardown = RootFailure::new(RootStage::Teardown, "busy").encode();
    // Each request, and answers that don't answer it.
    let cases: [(&str, Request, Vec<Message>, Kinds); 11] = [
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
    let cases: [(&str, Request, Message, &str); 9] = [
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
