//! The adapter's guest agent serves the control session between the host and the guest with the
//! adapter's own framing, through public APIs only.
//!
//! A test broker plays the broker's end of the guest's session over a Unix socket pair, with the
//! public record API of `nvx`: for the guest agent's `serve` in a thread, and for the
//! `runtime-adapter-guest` binary, whose standard input and output are the socket. The tests cover
//! messages that span records, partial records, resets in the middle of a message, disconnects,
//! record lengths, bootstrap identity, ERROR payloads, sequence and identity errors, and the
//! bounded buffer of the adapter's decoder. Through a relay to a pseudo-terminal, they also cover
//! the guest agent on a tty, which it puts in raw mode, as it serves a VM's control tty.
//!
//! The worker of a layered root runs its session with fake root operations, which record the
//! worker's calls and fail as a test asks: the tests cover the order of its stages, the host export
//! that it mounts, the mapping table that it applies in order, in parts, which outlives a reset, a
//! table of zero mappings, which does no work, the parts that it refuses, a mapping that fails and
//! latches the root, the start of the sandbox, and the executions and starts that it refuses out of
//! order, each stage that fails, the account check of a sandbox's workload identity, the requests
//! that it serves once the root is ready, its stop, the messages that it refuses, and a broker that
//! closes only its end of the stream, which still receives the worker's replies. The binary's PID 1
//! of a layered root, and its worker, run on a pseudo-terminal too, on the test's host, whose
//! kernel command line names no layered root, where PID 1 also collects the statuses that real-time
//! signals leave, whatever the names of its children.
//!
//! On x86_64, the worker of a clone, which a restore of a template's snapshot started, resumes the
//! session that the template's VM attached before its capture, active in the epoch of its broker's
//! first reset, with the same fake root operations, whose application of an activation record
//! mirrors the guest's: the tests cover the reset that the restore queued, the one `Activate` that
//! the clone takes before it is ready, the application of its record, the `Ready` that reports the
//! activation, retries of the `Activate`, the order that the worker serves once the clone is
//! ready, the status of each class of the activation's failures, which the worker answers with
//! nothing, before any side effect where the class allows, its deadline, also once the stream
//! takes nothing more, the record's workload identity, as which every execution runs, the
//! mapping table of the record's export, which the worker applies from the export that the
//! application mounted, while it leaves the endpoint of the network's proxy to the host, and the
//! announcer of the listeners of a record whose network publishes them, whose end ends the clone,
//! which the fake stands for with a pipe that the test closes.
//!
//! The worker's executions run behind the test broker too, whose host side drives the engine's
//! peers through the host program's own `exec::HostExecution`: the tests cover several executions
//! at once, byte-exact, a stalled sink and a full stream to the host, which stall only what waits
//! on them, executions that take turns at the stream, timeouts, cancellations, and stops, the
//! starts, messages, and probes that the worker refuses, generations and resets, the descendants
//! that a program leaves in its process group, and executions on a tty. The fake root prepares each
//! execution's entry, as the worker describes it, in a directory of the tests' own that holds
//! account databases, and then spawns the program without entering a root, which needs privileges,
//! in the tests' own root and identity: the tests cover the identity that exactly one source names,
//! the sandbox's or the start's, the entry's inputs, the publication of listeners that the root
//! configuration names, which refuses the capabilities that could forge a listener announcement
//! while listeners are published, a program's working directory and environment, and the starts
//! that the worker refuses for their identities. Where the tests can create cgroups, each fake root
//! gives the worker a workload cgroup of the test's own, below which the executions run in cgroups
//! of their own, and settle: the tests then also cover a descendant that leaves its program's
//! process group, which the settlement ends, and an execution that does not settle, which keeps its
//! place until the host stops the worker. Elsewhere, the programs run in process groups alone.
#![cfg(all(feature = "guest", target_os = "linux"))]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{self, PollFd, PollFlags, PollTimeout};
use nix::pty;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{self, Id, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use nvx::exec::{Cause, Key, LaunchFailure, Limits, Outcome, Pool, Status, Stream, Timers, Window};
use nvx::guest::cgroup::Layout;
use nvx::guest::entry::{self, Capabilities, Capability, Prepared, Publication};
use nvx::guest::mappings::{Access, Mapping};
use nvx::guest::nvxs::GuestConnectionError;
use nvx::guest::spawn::{self, Spawned};
use nvx::protocol::export::{EXPORT_DIRECTORY, ExportMode, HostExport};
use nvx::protocol::nvxs::{
    ErrorCode, Identity, Record, RecordError, RecordParser, RecordType, SessionError, encode_record,
};
use nvx::protocol::root::{RootImage, RootLayout, RootUpper};
use nvx::protocol::{
    HostMap, MAP_ENTRY_HEADER_LEN, MAPS_HEADER_LEN, OUTER_MAX_PAYLOAD, encode_maps_payloads,
};
use nvx::uuid::Uuid;
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::exec::{
    self, AccountMode, GuestMessage, HostExecution, HostMessage, MAX_CHUNK, Start,
};
use runtime_adapter::frame::{Decoder, FrameError, HEADER_LEN, MAX_BODY, Message};
use runtime_adapter::guest::{self, NO_ROOT, RECEIVE_WINDOW, ServeError, Served};
use runtime_adapter::probe::{Facts, Probe};
use runtime_adapter::root::{
    JournalPolicy, Listeners, MapFailure, OverlayMode, Ready, RootConfig, RootFailure, RootStage,
    encode_maps,
};
use runtime_adapter::worker::{
    self, Entrance, EntryOperations, MAX_EXECUTIONS, MapError, RootOperations, Worked,
};

/// Identity of the broker's first epoch.
const FIRST: Identity = Identity {
    instance_id: [0x5c; 16],
    epoch: 1,
};

/// Longest wait for a record from the guest, or for the guest to end.
const TIMEOUT: Duration = Duration::from_secs(30);

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

/// The broker's end of the guest's session. It resets the guest once the guest attaches, numbers
/// its records to the guest from 0 in each epoch, and sends DATA only while the guest's receive
/// window has room. It checks the identity and sequence number of each of the guest's records,
/// and panics on any record that the protocol does not allow.
struct Broker {
    /// Stream to the guest.
    stream: UnixStream,
    /// The guest's records, which a thread reads as they arrive, so that the guest never waits to
    /// write while the broker writes; then `None`, once the guest closed its stream at a record
    /// boundary.
    records: Receiver<Option<Record>>,
    /// Whether that thread stops reading the guest's stream.
    paused: Arc<AtomicBool>,
    /// Identity of the current epoch.
    identity: Identity,
    /// Sequence number of the broker's next record.
    send_sequence: u64,
    /// Sequence number of the guest's next record.
    receive_sequence: u64,
    /// Bytes for which the guest's receive window has room.
    room: usize,
    /// Decoder of the guest's messages.
    decoder: Decoder,
    /// Whether the guest closed its stream, as the broker found while it took records within a
    /// wait.
    closed: bool,
}

impl Broker {
    /// Creates the broker on its end of a socket pair.
    fn new(stream: UnixStream) -> Self {
        let reader = stream.try_clone().unwrap();
        let (sender, records) = mpsc::channel();
        let paused = Arc::new(AtomicBool::new(false));
        let reading = Arc::clone(&paused);
        thread::spawn(move || read_records(reader, &sender, &reading));
        Self {
            stream,
            records,
            paused,
            identity: FIRST,
            send_sequence: 0,
            receive_sequence: 0,
            room: 0,
            decoder: Decoder::new(),
            closed: false,
        }
    }

    /// Takes the guest's attachment, then starts the epoch of `identity`.
    fn attach(&mut self, identity: Identity) {
        let record = self.read().expect("the guest attaches");
        assert_eq!(record.record_type(), RecordType::GuestAttach);
        let header = (record.instance_id(), record.epoch(), record.sequence());
        assert_eq!(header, ([0; 16], 0, 0));
        self.reset(identity);
    }

    /// Starts the epoch of `identity` with a RESET, and takes the guest's acknowledgement.
    fn reset(&mut self, identity: Identity) {
        self.identity = identity;
        self.send_sequence = 0;
        self.receive_sequence = 0;
        self.room = 0;
        self.send(RecordType::Reset, &[]);
        let ack = self.expect();
        assert_eq!(ack.record_type(), RecordType::Ack);
        assert_eq!(ack.window(), Some(RECEIVE_WINDOW));
        self.room = RECEIVE_WINDOW as usize;
    }

    /// Sends a record of the current epoch at the broker's next sequence number.
    fn send(&mut self, record_type: RecordType, payload: &[u8]) {
        let record = record(record_type, self.identity, self.send_sequence, payload);
        self.send_sequence += 1;
        self.write(&record);
    }

    /// Writes bytes to the guest. A guest that stops reading fails the test once `TIMEOUT` passes,
    /// instead of hanging it: the socket's buffer can fill before the guest's receive window does.
    fn write(&mut self, mut bytes: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
        while !bytes.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "the guest reads the broker's bytes in time"
            );
            self.stream.set_write_timeout(Some(remaining)).unwrap();
            match self.stream.write(bytes) {
                Ok(count) => bytes = &bytes[count..],
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => panic!("cannot write to the guest: {error}"),
            }
        }
    }

    /// Ends the broker's stream to the guest.
    fn close(&self) {
        self.stream.shutdown(Shutdown::Write).unwrap();
    }

    /// Stops reading the guest's stream, as a broker whose host takes nothing, until
    /// [`resume`](Self::resume), so that the stream fills up. Meanwhile, the broker may send only
    /// what the guest's receive window has room for.
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// Reads the guest's stream again.
    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    /// Returns the guest's next record, or `None` once the guest closed its stream at a record
    /// boundary.
    fn read(&mut self) -> Option<Record> {
        self.records
            .recv_timeout(TIMEOUT)
            .expect("the guest sends a record or ends its stream in time")
    }

    /// Reads the guest's next record, and checks that it carries the epoch's identity and the
    /// guest's next sequence number.
    fn expect(&mut self) -> Record {
        let record = self.read().expect("the guest sends a record");
        self.check(&record);
        record
    }

    /// Checks that `record`, the guest's next, carries the epoch's identity and the guest's next
    /// sequence number.
    fn check(&mut self, record: &Record) {
        let header = (record.instance_id(), record.epoch(), record.sequence());
        let expected = (
            self.identity.instance_id,
            self.identity.epoch,
            self.receive_sequence,
        );
        assert_eq!(header, expected, "{record:?}");
        self.receive_sequence += 1;
    }

    /// Takes the guest's next record: a CREDIT, which makes room in its window, or DATA, whose
    /// complete messages it returns.
    fn take(&mut self) -> Vec<Message> {
        let record = self.expect();
        self.accept(&record)
    }

    /// Takes the guest's next record as [`take`](Self::take) does, if one arrives within `wait`,
    /// and notes whether the guest closed its stream.
    fn take_within(&mut self, wait: Duration) -> Option<Vec<Message>> {
        let record = match self.records.recv_timeout(wait) {
            Ok(Some(record)) => record,
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.closed = true;
                return None;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return None,
        };
        self.check(&record);
        Some(self.accept(&record))
    }

    /// Takes `record`, the guest's next in the epoch: a CREDIT, which makes room in its window,
    /// or DATA, whose complete messages it returns.
    fn accept(&mut self, record: &Record) -> Vec<Message> {
        match record.record_type() {
            RecordType::Credit => {
                self.room += record.credit().unwrap() as usize;
                assert!(
                    self.room <= RECEIVE_WINDOW as usize,
                    "the guest credited more than it received"
                );
                Vec::new()
            }
            RecordType::Data => decode(&mut self.decoder, record.payload()),
            record_type => panic!("the guest sent {record_type} in an active epoch"),
        }
    }

    /// Starts the epoch of `identity` with a RESET while the guest may still send records of the
    /// earlier epoch, which it drops with the earlier epoch's partial message, and takes the
    /// guest's acknowledgement.
    fn reset_in_flight(&mut self, identity: Identity) {
        let earlier = (self.identity.instance_id, self.identity.epoch);
        self.identity = identity;
        self.send_sequence = 0;
        self.room = 0;
        self.send(RecordType::Reset, &[]);
        loop {
            let record = self.read().expect("the guest acknowledges the reset");
            if record.record_type() == RecordType::Ack {
                let header = (record.instance_id(), record.epoch(), record.sequence());
                assert_eq!(header, (identity.instance_id, identity.epoch, 0));
                assert_eq!(record.window(), Some(RECEIVE_WINDOW));
                break;
            }
            assert_eq!(
                (record.instance_id(), record.epoch()),
                earlier,
                "{record:?}"
            );
        }
        self.receive_sequence = 1;
        self.room = RECEIVE_WINDOW as usize;
        self.decoder.clear();
    }

    /// Sends the host's `bytes` in DATA records of at most `size` bytes, each once the guest's
    /// window has room for it, and returns the messages that the guest sent meanwhile.
    fn send_data(&mut self, bytes: &[u8], size: usize) -> Vec<Message> {
        let mut messages = Vec::new();
        for chunk in bytes.chunks(size) {
            while self.room < chunk.len() {
                messages.extend(self.take());
            }
            self.room -= chunk.len();
            self.send(RecordType::Data, chunk);
        }
        messages
    }

    /// Sends `messages` in DATA records of at most `size` bytes, and returns the guest's next
    /// `replies` messages.
    fn exchange(&mut self, messages: &[Message], size: usize, replies: usize) -> Vec<Message> {
        let mut received = self.send_data(&encode(messages), size);
        while received.len() < replies {
            received.extend(self.take());
        }
        assert_eq!(received.len(), replies, "{received:?}");
        received
    }

    /// Takes the guest's credit for everything that the broker sent, so that no record of the
    /// epoch is in flight.
    fn settle(&mut self) {
        while self.room < RECEIVE_WINDOW as usize {
            assert_eq!(self.take(), [], "the guest sent no message");
        }
    }
}

/// Reads the guest's records from `stream` and sends them, then `None` once the guest closed its
/// stream at a record boundary. It reads nothing while `paused` holds.
fn read_records(mut stream: UnixStream, records: &Sender<Option<Record>>, paused: &AtomicBool) {
    let mut parser = RecordParser::new();
    let mut buffer = [0; 4096];
    loop {
        while paused.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        let wanted = parser.needed().min(buffer.len());
        let count = match stream.read(&mut buffer[..wanted]) {
            Ok(count) => count,
            // The guest closed its end before it read everything that the broker wrote, as after
            // a record that it refused. Linux returns what the guest wrote first.
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => 0,
            Err(error) => panic!("cannot read the guest's stream: {error}"),
        };
        if count == 0 {
            assert_eq!(
                parser.buffered(),
                0,
                "the guest ended its stream inside a record"
            );
            let _ = records.send(None);
            return;
        }
        let (consumed, record) = parser.push(&buffer[..count]).unwrap();
        assert_eq!(consumed, count);
        // A test that ended takes no more records.
        if record.is_some_and(|record| records.send(Some(record)).is_err()) {
            return;
        }
    }
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

/// Returns an echo of the longest body: with its header, the message spans five records.
fn longest() -> Message {
    Message::Echo((0..=u8::MAX).cycle().take(MAX_BODY).collect())
}

/// Starts the guest agent's `serve` in a thread, on one end of a socket pair, and returns the
/// broker on the other end with the thread.
fn start() -> (Broker, JoinHandle<Result<Served, ServeError>>) {
    let (broker, guest) = UnixStream::pair().unwrap();
    let serving = thread::spawn(move || guest::serve(guest));
    (Broker::new(broker), serving)
}

/// Returns what `thread` returned, and fails the test if it does not end in time, so that a guest
/// that keeps serving fails the test instead of hanging it.
fn joined<T>(thread: JoinHandle<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    while !thread.is_finished() {
        assert!(Instant::now() < deadline, "the guest agent ends in time");
        thread::sleep(Duration::from_millis(10));
    }
    thread.join().unwrap()
}

/// Starts the guest agent, attaches it in the first epoch, sends it `bytes`, and returns the
/// error that ended serving.
fn refused(bytes: &[u8]) -> ServeError {
    let (mut broker, serving) = start();
    broker.attach(FIRST);
    broker.write(bytes);
    let error = joined(serving).unwrap_err();
    // The guest ended its stream.
    while let Some(record) = broker.read() {
        assert_eq!(record.record_type(), RecordType::Credit, "{error:?}");
    }
    error
}

#[test]
fn the_guest_agent_answers_messages_that_span_records() {
    let (mut broker, serving) = start();
    broker.attach(FIRST);
    let replies = broker.exchange(&[Message::Ping], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [Message::Pong]);

    // Messages in records of several sizes. The longest spans five records each way, and the
    // guest's window holds a quarter of it, so the rest arrives as the guest credits it.
    let messages = [
        Message::Ping,
        Message::Echo(b"two".to_vec()),
        longest(),
        Message::Echo(Vec::new()),
        Message::Ping,
    ];
    let expected = [
        Message::Pong,
        Message::Echo(b"two".to_vec()),
        longest(),
        Message::Echo(Vec::new()),
        Message::Pong,
    ];
    for size in [7, 4096, OUTER_MAX_PAYLOAD] {
        let replies = broker.exchange(&messages, size, expected.len());
        assert_eq!(replies, expected, "records of {size} bytes");
    }

    // A record whose bytes arrive one at a time.
    broker.settle();
    let ping = Message::Ping.encode().unwrap();
    let bytes = record(RecordType::Data, FIRST, broker.send_sequence, &ping);
    broker.send_sequence += 1;
    broker.room -= ping.len();
    for byte in bytes {
        broker.write(&[byte]);
        thread::sleep(Duration::from_millis(1));
    }
    let mut replies = Vec::new();
    while replies.is_empty() {
        replies = broker.take();
    }
    assert_eq!(replies, [Message::Pong]);

    let replies = broker.exchange(&[Message::Stop], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [Message::Stopped]);
    assert_eq!(joined(serving).unwrap(), Served::Stopped);
    assert!(broker.read().is_none());
}

#[test]
fn a_reset_starts_the_new_epoch_with_a_new_message() {
    let (mut broker, serving) = start();
    broker.attach(FIRST);
    let echo = Message::Echo(vec![1; 1000]).encode().unwrap();

    // Half of a message, then the host's connection drops, which starts the instance's next
    // epoch. The guest drops the partial message.
    assert_eq!(broker.send_data(&echo[..500], OUTER_MAX_PAYLOAD), []);
    broker.settle();
    broker.reset(Identity { epoch: 2, ..FIRST });
    let after = Message::Echo(b"after".to_vec());
    let replies = broker.exchange(std::slice::from_ref(&after), OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [after]);

    // A broker restored from a snapshot has a new instance, which starts at epoch 1, here in
    // the middle of a header.
    assert_eq!(
        broker.send_data(&echo[..HEADER_LEN - 1], OUTER_MAX_PAYLOAD),
        []
    );
    broker.settle();
    broker.reset(Identity {
        instance_id: [0x7e; 16],
        epoch: 1,
    });
    let replies = broker.exchange(&[Message::Ping, Message::Stop], OUTER_MAX_PAYLOAD, 2);
    assert_eq!(replies, [Message::Pong, Message::Stopped]);
    assert_eq!(joined(serving).unwrap(), Served::Stopped);
}

#[test]
fn the_guest_agent_ends_with_its_stream() {
    // A stream that ends at a record boundary closes the session.
    let (mut broker, serving) = start();
    broker.attach(FIRST);
    let replies = broker.exchange(&[Message::Ping], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [Message::Pong]);
    broker.close();
    assert_eq!(joined(serving).unwrap(), Served::Closed);
    assert!(broker.read().is_none());

    // One that ends inside a record fails it.
    let (mut broker, serving) = start();
    broker.attach(FIRST);
    let ping = record(RecordType::Data, FIRST, 1, &Message::Ping.encode().unwrap());
    broker.write(&ping[..ping.len() - 1]);
    broker.close();
    let error = joined(serving).unwrap_err();
    let truncated = matches!(
        &error,
        ServeError::Session(GuestConnectionError::Protocol(SessionError::Truncated { buffered }))
            if *buffered == ping.len() - 1
    );
    assert!(truncated, "{error:?}");
}

#[test]
fn the_guest_agent_fails_on_records_that_the_protocol_does_not_allow() {
    let ping = Message::Ping.encode().unwrap();
    let data = |identity, sequence| record(RecordType::Data, identity, sequence, &ping);
    let next_epoch = Identity { epoch: 2, ..FIRST };
    let other_instance = Identity {
        instance_id: [0x7e; 16],
        ..FIRST
    };
    let patched = |mut bytes: Vec<u8>, offset: usize, patch: &[u8]| {
        bytes[offset..offset + patch.len()].copy_from_slice(patch);
        bytes
    };
    // A RESET with a payload, an empty DATA record, and a GUEST_ATTACH with an identity, which
    // the record API cannot encode.
    let mut reset_with_payload = patched(
        record(RecordType::Reset, next_epoch, 1, &[]),
        LENGTH_OFFSET,
        &4u32.to_le_bytes(),
    );
    reset_with_payload.extend_from_slice(&[0; 4]);
    let mut empty_data = patched(data(FIRST, 1), LENGTH_OFFSET, &0u32.to_le_bytes());
    empty_data.truncate(empty_data.len() - ping.len());
    let attach_with_identity = patched(
        record(
            RecordType::GuestAttach,
            Identity {
                instance_id: [0; 16],
                epoch: 0,
            },
            0,
            &[],
        ),
        INSTANCE_OFFSET,
        &[1],
    );
    let error_record = |code: u32| {
        let payload = ErrorCode::Authentication.code().to_le_bytes();
        patched(
            record(RecordType::Error, FIRST, 1, &payload),
            LENGTH_OFFSET + 4,
            &code.to_le_bytes(),
        )
    };
    let wrong_magic = patched(data(FIRST, 1), 0, b"NVXT");
    let cases = [
        (
            "a skipped sequence number",
            data(FIRST, 2),
            SessionError::Sequence {
                expected: 1,
                actual: 2,
            },
        ),
        (
            "a repeated sequence number",
            data(FIRST, 0),
            SessionError::Sequence {
                expected: 1,
                actual: 0,
            },
        ),
        (
            "DATA of another epoch",
            data(next_epoch, 1),
            SessionError::Epoch {
                expected: 1,
                actual: 2,
            },
        ),
        (
            "DATA of another instance",
            data(other_instance, 1),
            SessionError::Instance {
                expected: FIRST.instance_id,
                actual: other_instance.instance_id,
            },
        ),
        (
            "a RESET that repeats the epoch",
            record(RecordType::Reset, FIRST, 1, &[]),
            SessionError::StaleEpoch {
                current: 1,
                actual: 1,
            },
        ),
        (
            "a RESET without an identity",
            record(
                RecordType::Reset,
                Identity {
                    instance_id: [0; 16],
                    epoch: 0,
                },
                1,
                &[],
            ),
            SessionError::MissingIdentity(RecordType::Reset),
        ),
        (
            "a RESET with a payload",
            reset_with_payload,
            SessionError::Record(RecordError::Length {
                record_type: RecordType::Reset,
                length: 4,
            }),
        ),
        (
            "an empty DATA record",
            empty_data,
            SessionError::Record(RecordError::Length {
                record_type: RecordType::Data,
                length: 0,
            }),
        ),
        (
            "a bootstrap record with an identity",
            attach_with_identity,
            SessionError::Record(RecordError::BootstrapIdentity(RecordType::GuestAttach)),
        ),
        (
            "an ERROR record with an unknown code",
            error_record(2),
            SessionError::Record(RecordError::ErrorCode(2)),
        ),
        (
            "an ERROR record",
            error_record(ErrorCode::Authentication.code()),
            SessionError::Unexpected(RecordType::Error),
        ),
        (
            "an ACK record",
            record(RecordType::Ack, FIRST, 1, &RECEIVE_WINDOW.to_le_bytes()),
            SessionError::Unexpected(RecordType::Ack),
        ),
        (
            "an unknown record type",
            patched(data(FIRST, 1), TYPE_OFFSET, &[0x7f]),
            SessionError::Record(RecordError::Type(0x7f)),
        ),
        (
            "nonzero flags",
            patched(data(FIRST, 1), FLAGS_OFFSET, &[1]),
            SessionError::Record(RecordError::Flags(1)),
        ),
        (
            "another protocol version",
            patched(data(FIRST, 1), VERSION_OFFSET, &2u16.to_le_bytes()),
            SessionError::Record(RecordError::Version(2)),
        ),
        (
            "a wrong magic",
            wrong_magic,
            SessionError::Record(RecordError::Magic(*b"NVXT")),
        ),
    ];
    for (name, bytes, expected) in cases {
        let error = refused(&bytes);
        let failed = matches!(
            &error,
            ServeError::Session(GuestConnectionError::Protocol(error)) if *error == expected
        );
        assert!(failed, "{name}: {error:?}");
    }
}

#[test]
fn the_guest_agent_refuses_messages_that_its_framing_does_not_allow() {
    // A message is refused at its header, before its body arrives: the decoder never buffers a
    // body beyond the limit.
    let too_long = u32::try_from(MAX_BODY + 1).unwrap().to_le_bytes();
    let header = |length: [u8; 4], kind: u8| {
        let mut header = length.to_vec();
        header.push(kind);
        header
    };
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
            header(1u32.to_le_bytes(), 1),
            FrameError::Length { kind: 1, length: 1 },
        ),
    ];
    for (message, expected) in cases {
        let error = refused(&record(RecordType::Data, FIRST, 1, &message));
        let failed = matches!(&error, ServeError::Frame(error) if *error == expected);
        assert!(failed, "{message:?}: {error:?}");
    }

    // The guest agent answers requests, and refuses the messages that only it sends, and those
    // about a layered root, which its VM does not have: its configuration, a part of its mapping
    // table, and the start of its workloads, a clone's activation, and executions, which only the
    // worker of a layered root runs.
    let refused_messages = [
        Message::Pong,
        Message::Stopped,
        ready(),
        Message::RootFailed(vec![1]),
        Message::Report(Vec::new()),
        Message::ProbeFailed(Vec::new()),
        Message::Mapped,
        Message::MapFailed(MapFailure::new(0, "refused").encode()),
        Message::Started,
        Message::RootConfig(RootConfig::default().encode()),
        Message::Maps(maps_part()),
        Message::Start,
        Message::Activate(Vec::new()),
        Message::ExecStart(Vec::new()),
        Message::ExecCredit(Vec::new()),
        Message::ExecTerminal(Vec::new()),
    ];
    for message in refused_messages {
        let bytes = record(RecordType::Data, FIRST, 1, &message.encode().unwrap());
        let error = refused(&bytes);
        let failed = matches!(&error, ServeError::Unexpected(kind) if *kind == message.kind());
        assert!(failed, "{message:?}: {error:?}");
    }
}

#[test]
fn the_guest_agent_of_a_vm_without_a_layered_root_answers_probes_with_a_failure() {
    let (mut broker, serving) = start();
    broker.attach(FIRST);
    let messages = [
        Message::Probe(b"mounts".to_vec()),
        Message::Ping,
        Message::Stop,
    ];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 3);
    let expected = [
        Message::ProbeFailed(NO_ROOT.as_bytes().to_vec()),
        Message::Pong,
        Message::Stopped,
    ];
    assert_eq!(replies, expected);
    assert_eq!(joined(serving).unwrap(), Served::Stopped);
}

/// Starts the guest binary's `serve` with one end of a socket pair as its standard input and
/// output, and returns the broker on the other end with the process.
fn start_binary() -> (Broker, GuestProcess) {
    let (broker, guest) = UnixStream::pair().unwrap();
    let output = guest.try_clone().unwrap();
    let child = GuestProcess::spawn(
        Command::new(env!("CARGO_BIN_EXE_runtime-adapter-guest"))
            .arg(runtime_adapter::GUEST_SERVE_COMMAND)
            .stdin(Stdio::from(OwnedFd::from(guest)))
            .stdout(Stdio::from(OwnedFd::from(output)))
            .stderr(Stdio::piped()),
    );
    (Broker::new(broker), child)
}

/// A guest binary that a test started, in a process group of its own, which also holds the
/// processes that the binary starts, such as those of PID 1 of a layered root. The binary's status
/// is collected only once its group has been killed: until then, the binary keeps its process ID,
/// even after it exits, and the group keeps that ID, which no other process can take. Dropping it
/// kills the group and reaps the binary, so that a test that fails while they run leaves none of
/// them running, even if the binary exited first.
struct GuestProcess(Option<Child>);

/// How a guest binary that a test started failed to finish, with what it wrote. Either way, its
/// process group was killed, and its status collected.
#[derive(Debug)]
enum Unfinished {
    /// The binary did not exit in time.
    Late(Output),
    /// The binary exited, but left these processes of its group running.
    Orphans(Output, Vec<u32>),
}

impl GuestProcess {
    /// Starts the guest binary with `command`, in a process group of its own.
    fn spawn(command: &mut Command) -> Self {
        Self(Some(command.process_group(0).spawn().unwrap()))
    }

    /// Returns the guest binary's process ID, which is the ID of its process group.
    fn id(&self) -> u32 {
        self.0.as_ref().expect("the process was not reaped").id()
    }

    /// Returns whether the guest binary has exited, without collecting its status.
    fn exited(&self) -> bool {
        let pid = Pid::from_raw(i32::try_from(self.id()).unwrap());
        let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
        !matches!(
            wait::waitid(Id::Pid(pid), flags).unwrap(),
            WaitStatus::StillAlive
        )
    }

    /// Returns the guest binary's output once it exits, and every process that it started with
    /// it. Otherwise kills its process group, and fails the test instead of hanging it.
    fn finished(self) -> Output {
        self.finished_within(TIMEOUT)
            .unwrap_or_else(|unfinished| panic!("the guest binary did not finish: {unfinished:?}"))
    }

    /// Returns the guest binary's output once it exits within `timeout`, and no other process of
    /// its group outlives it. Otherwise kills its process group, whose processes may all hold the
    /// binary's output open, and fails with what the binary wrote.
    fn finished_within(mut self, timeout: Duration) -> Result<Output, Unfinished> {
        let deadline = Instant::now() + timeout;
        while !self.exited() {
            if Instant::now() >= deadline {
                self.kill();
                return Err(Unfinished::Late(self.reaped()));
            }
            thread::sleep(Duration::from_millis(10));
        }
        let orphans = running_in_group(self.id());
        self.kill();
        let output = self.reaped();
        if orphans.is_empty() {
            Ok(output)
        } else {
            Err(Unfinished::Orphans(output, orphans))
        }
    }

    /// Kills the guest binary's process group, which is still the binary's, because its status
    /// has not been collected.
    fn kill(&self) {
        let group = Pid::from_raw(i32::try_from(self.id()).unwrap());
        let _ = signal::killpg(group, Signal::SIGKILL);
    }

    /// Collects the guest binary's status, and its output, which ends once every process that
    /// holds it has ended.
    fn reaped(&mut self) -> Output {
        let child = self.0.take().expect("the process was not reaped");
        child.wait_with_output().unwrap()
    }
}

impl Drop for GuestProcess {
    fn drop(&mut self) {
        if self.0.is_some() {
            self.kill();
            let _ = self.0.take().unwrap().wait();
        }
    }
}

/// Returns the processes of the process group `group` that have not exited: all but zombies.
fn running_in_group(group: u32) -> Vec<u32> {
    let group = group.to_string();
    let mut running = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        // A process may end while the scan reads it. The state comes first, then the parent's
        // process ID, then the process group.
        let Some(fields) = stat_fields(pid) else {
            continue;
        };
        if let [state, _, of, ..] = fields.as_slice()
            && *of == group
            && !matches!(state.as_str(), "Z" | "X")
        {
            running.push(pid);
        }
    }
    running
}

/// Returns the fields of the `stat` of the process `id` that follow the command's name, from its
/// state on, or `None` once the process is gone. The name, which may hold spaces, parentheses, and
/// any other byte, and so need not be UTF-8, ends at the last `)`, and the fields are ASCII.
fn stat_fields(id: u32) -> Option<Vec<String>> {
    let stat = fs::read(format!("/proc/{id}/stat")).ok()?;
    let end = stat.iter().rposition(|&byte| byte == b')')?;
    let fields = std::str::from_utf8(&stat[end + 1..]).ok()?;
    Some(fields.split_whitespace().map(str::to_owned).collect())
}

#[test]
fn the_guest_binary_serves_the_session_on_its_standard_streams() {
    // The host stops the guest.
    let (mut broker, child) = start_binary();
    broker.attach(FIRST);
    let echo = Message::Echo(vec![0x42; OUTER_MAX_PAYLOAD]);
    let messages = [Message::Ping, echo.clone(), Message::Stop];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 3);
    assert_eq!(replies, [Message::Pong, echo, Message::Stopped]);
    let output = child.finished();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");

    // The broker closes the stream at a record boundary.
    let (mut broker, child) = start_binary();
    broker.attach(FIRST);
    broker.close();
    let output = child.finished();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");

    // The host sends a malformed message.
    let (mut broker, child) = start_binary();
    broker.attach(FIRST);
    broker.send_data(&[0, 0, 0, 0, 28], OUTER_MAX_PAYLOAD);
    let output = child.finished();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        diagnostic,
        "runtime-adapter-guest: unknown message kind 28\n"
    );
}

/// A pseudo-terminal whose tty the guest agent serves, opening it by its path, and whose master
/// end a relay joins to a socket pair, on whose other end the broker plays its end of the session,
/// as for the other tests.
struct Terminal {
    /// The path of the tty.
    path: PathBuf,
    /// The test's own descriptor of the tty. Reads of the master end fail once no descriptor of
    /// the tty is open, so it keeps the relay up until the guest agent opens the tty.
    _tty: OwnedFd,
}

/// Opens a pseudo-terminal, and returns it with the broker at the other end of its relay.
///
/// Its descriptors are closed on exec, so that no process that a test starts holds its master
/// end, which would keep the tty from hanging up once the test ends.
fn terminal() -> (Broker, Terminal) {
    let master = pty::posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC).unwrap();
    pty::grantpt(&master).unwrap();
    pty::unlockpt(&master).unwrap();
    let path = PathBuf::from(pty::ptsname_r(&master).unwrap());
    // The standard library opens files, and duplicates descriptors, close-on-exec.
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(OFlag::O_NOCTTY.bits())
        .open(&path)
        .unwrap();
    let (broker, relay) = UnixStream::pair().unwrap();
    let (mut from_broker, mut to_broker) = (relay.try_clone().unwrap(), relay);
    let master = File::from(OwnedFd::from(master));
    let (mut to_guest, mut from_guest) = (master.try_clone().unwrap(), master);
    thread::spawn(move || io::copy(&mut from_broker, &mut to_guest));
    thread::spawn(move || {
        let _ = io::copy(&mut from_guest, &mut to_broker);
        let _ = to_broker.shutdown(Shutdown::Write);
    });
    let terminal = Terminal {
        path,
        _tty: OwnedFd::from(tty),
    };
    (Broker::new(broker), terminal)
}

#[test]
fn the_guest_agent_serves_a_tty_in_raw_mode() {
    let (mut broker, terminal) = terminal();
    let path = terminal.path.clone();
    let serving = thread::spawn(move || guest::serve_terminal(&path));
    broker.attach(FIRST);
    // Every byte value, which a tty that was not in raw mode would translate or act on: carriage
    // returns and newlines, and the characters of signals, flow control, line editing, and the end
    // of a file. The confirmation of the stop reaches the broker before serving returns.
    let replies = broker.exchange(&[longest(), Message::Stop], OUTER_MAX_PAYLOAD, 2);
    assert_eq!(replies, [longest(), Message::Stopped]);
    assert_eq!(joined(serving).unwrap(), Served::Stopped);
}

#[test]
fn the_guest_binary_serves_a_tty_in_place_of_the_managed_agent() {
    // Unless it is PID 1, `init` exits with the status with which PID 1 would end the VM.
    let (mut broker, terminal) = terminal();
    let child = GuestProcess::spawn(
        Command::new(env!("CARGO_BIN_EXE_runtime-adapter-guest"))
            .arg(runtime_adapter::GUEST_INIT_COMMAND)
            .arg(&terminal.path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    broker.attach(FIRST);
    let replies = broker.exchange(&[Message::Ping, Message::Stop], OUTER_MAX_PAYLOAD, 2);
    assert_eq!(replies, [Message::Pong, Message::Stopped]);
    let output = child.finished();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");

    // A path that is no tty fails at once: one that cannot be opened, and one that has no
    // terminal settings.
    for (path, diagnostic, error) in [
        ("/", "cannot open /", 21),
        ("/dev/null", "cannot put /dev/null in raw mode", 25),
    ] {
        let child = GuestProcess::spawn(
            Command::new(env!("CARGO_BIN_EXE_runtime-adapter-guest"))
                .args([runtime_adapter::GUEST_INIT_COMMAND, path])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        );
        let output = child.finished();
        assert_eq!(output.status.code(), Some(1), "{path}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with(&format!("runtime-adapter-guest: {diagnostic}: "))
                && stderr.ends_with(&format!(" (os error {error})\n")),
            "{path}: {stderr}"
        );
    }
}

/// An operation of a layered root, which a [`FakeRoot`] fails when a test asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    /// Reading the kernel command line.
    CommandLine,
    /// The bootstrap.
    Bootstrap,
    /// The assembly.
    Assemble,
    /// A clone's reread of its GPT disk's partition table, before its assembly.
    #[cfg(target_arch = "x86_64")]
    Reread,
    /// A clone's generation of the machine ID that its record asks for, once its root's account
    /// was checked.
    #[cfg(target_arch = "x86_64")]
    MachineId,
    /// A clone's setting of its record's hostname and machine ID for every execution, last.
    #[cfg(target_arch = "x86_64")]
    View,
    /// A clone's write of the resolver file that names its record's DNS server, once it mounted
    /// its record's export, if any.
    #[cfg(target_arch = "x86_64")]
    Resolver,
    /// A clone's start of the announcer of its workload's listeners, which its record's network
    /// publishes, once it wrote the resolver file, if any.
    #[cfg(target_arch = "x86_64")]
    Announce,
    /// The host export's mount: the one that the kernel command line names, or a clone's record's.
    MountExport,
    /// A probe.
    Probe,
    /// The teardown.
    Teardown,
}

/// A call of a [`FakeRoot`]'s operations, with what the worker passed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    /// The kernel command line was read.
    CommandLine,
    /// The bootstrap ran with the configuration.
    Bootstrap(RootConfig),
    /// The assembly ran with the configuration and the layout.
    Assemble(RootConfig, RootLayout),
    /// The host export was mounted.
    MountExport(HostExport),
    /// The mappings were applied, in order.
    Map(Vec<Mapping>),
    /// The entry of an execution, or of the sandbox's workload identity, was prepared.
    Prepare(Entrance),
    /// The probe ran.
    Probe(Probe),
    /// The teardown ran.
    Teardown,
    /// A clone's activation record was applied with the inputs that its template captured.
    #[cfg(target_arch = "x86_64")]
    Apply(
        Box<(
            nvx::activation::ActivationRecord,
            nvx::guest::clone::RootTemplate,
        )>,
    ),
}

/// Root operations that record the worker's calls, and fail as a test asks.
#[derive(Debug)]
struct FakeRoot {
    /// The kernel command line that the operations read.
    command_line: Vec<u8>,
    /// The operation that fails, and its problem, if any.
    failing: Option<(Operation, &'static str)>,
    /// The target of the mapping that the operations refuse, after those before it, and the
    /// problem, if any.
    refusing: Option<(PathBuf, &'static str)>,
    /// Whether the operations refused a mapping, after which they refuse every mapping, as a
    /// latched root does.
    latched: bool,
    /// How long the assembly takes.
    assembly: Duration,
    /// The gate that each probe, and each of a clone's applications, waits on, once it recorded
    /// its call, until the test opens it, if any.
    gate: Option<Receiver<()>>,
    /// The worker's calls, in order.
    calls: Vec<Call>,
    /// The deadline of each of a clone's applications, in order, until which it waits for the
    /// devices at the latest.
    #[cfg(target_arch = "x86_64")]
    deadlines: Vec<Option<Instant>>,
    /// The workload cgroup of the test's own, below which the worker runs its executions in
    /// cgroups of their own, where the test can create cgroups.
    workload: Option<Workload>,
    /// The read end of a pipe whose write end the test holds, which the application takes as the
    /// announcer that it starts, if the test made one with [`ending`](Self::ending).
    ending: Option<OwnedFd>,
    /// The announcer that a clone's application started, for a record whose network publishes
    /// the workload's listeners, until the worker collected its end: the read end of a pipe,
    /// which polls readable, hung up, once its write end closes, as an announcer's pidfd polls
    /// readable once the announcer ended, with that write end, unless the test holds it.
    announcer: Option<(OwnedFd, Option<OwnedFd>)>,
}

impl FakeRoot {
    /// Returns operations whose kernel command line names `layout`.
    fn booting(layout: &RootLayout) -> Self {
        let command_line = format!("console=hvc0 nvx_sandbox=1 {}", layout.tokens().join(" "));
        Self::reading(command_line.as_bytes())
    }

    /// Returns operations whose kernel command line names `layout` and the host export `export`,
    /// as OpenVMM appends its tokens.
    fn exporting(layout: &RootLayout, export: &HostExport) -> Self {
        let command_line = format!(
            "console=hvc0 nvx_sandbox=1 {} virtfs_dir={EXPORT_DIRECTORY} virtfs_tag={} \
             virtfs_mode={} virtfs_aggregate=1",
            layout.tokens().join(" "),
            export.tag,
            export.mode,
        );
        Self::reading(command_line.as_bytes())
    }

    /// Returns operations whose kernel command line is `command_line`, with a workload cgroup of
    /// the test's own where the test can create cgroups.
    fn reading(command_line: &[u8]) -> Self {
        let workload =
            cgroups().then(|| Workload::new().expect("the test creates a workload cgroup"));
        Self {
            command_line: command_line.to_vec(),
            failing: None,
            refusing: None,
            latched: false,
            assembly: Duration::ZERO,
            gate: None,
            calls: Vec::new(),
            #[cfg(target_arch = "x86_64")]
            deadlines: Vec::new(),
            workload,
            ending: None,
            announcer: None,
        }
    }

    /// Returns operations of the layout that the worker assembles, whose `operation` fails with
    /// `problem`.
    fn failing(operation: Operation, problem: &'static str) -> Self {
        Self {
            failing: Some((operation, problem)),
            ..Self::booting(&layout())
        }
    }

    /// Makes the operations refuse the mapping at `target` with `problem`, once they applied the
    /// mappings before it, and refuse every mapping after it.
    fn refusing(mut self, target: &str, problem: &'static str) -> Self {
        self.refusing = Some((PathBuf::from(target), problem));
        self
    }

    /// Makes the assembly take `assembly`.
    fn assembling_for(mut self, assembly: Duration) -> Self {
        self.assembly = assembly;
        self
    }

    /// Makes the announcer that a clone's application starts end once the test drops the returned
    /// write end of its pipe.
    fn ending(mut self) -> (Self, OwnedFd) {
        let (read, write) = announcer_pipe();
        self.ending = Some(read);
        (self, write)
    }

    /// Makes each probe, and each of a clone's applications, wait until the test opens the gate,
    /// which the returned sender opens for one at a time.
    fn gated(mut self) -> (Self, Sender<()>) {
        let (open, gate) = mpsc::channel();
        self.gate = Some(gate);
        (self, open)
    }

    /// Fails if `operation` fails.
    fn result(&self, operation: Operation) -> Result<(), String> {
        match self.failing {
            Some((failing, problem)) if failing == operation => Err(problem.to_owned()),
            _ => Ok(()),
        }
    }
}

impl RootOperations for FakeRoot {
    fn command_line(&mut self) -> Result<Vec<u8>, String> {
        self.calls.push(Call::CommandLine);
        self.result(Operation::CommandLine)?;
        Ok(self.command_line.clone())
    }

    fn bootstrap(&mut self, config: &RootConfig) -> Result<(), String> {
        self.calls.push(Call::Bootstrap(config.clone()));
        self.result(Operation::Bootstrap)
    }

    fn assemble(&mut self, config: &RootConfig, layout: &RootLayout) -> Result<(), String> {
        self.calls.push(Call::Assemble(config.clone(), *layout));
        thread::sleep(self.assembly);
        self.result(Operation::Assemble)
    }

    fn mount_export(&mut self, export: &HostExport) -> Result<(), String> {
        self.calls.push(Call::MountExport(export.clone()));
        self.result(Operation::MountExport)
    }

    fn map(&mut self, mappings: &[Mapping]) -> Result<(), MapError> {
        self.calls.push(Call::Map(mappings.to_vec()));
        if self.latched {
            return Err(MapError {
                applied: 0,
                problem: "the root is latched".to_owned(),
            });
        }
        let Some((target, problem)) = &self.refusing else {
            return Ok(());
        };
        let refused = mappings
            .iter()
            .position(|mapping| Path::new("/").join(mapping.target()) == *target);
        match refused {
            Some(applied) => {
                self.latched = true;
                Err(MapError {
                    applied,
                    problem: (*problem).to_owned(),
                })
            }
            None => Ok(()),
        }
    }

    fn cgroups(&self) -> Option<Layout> {
        self.workload.as_ref().map(|workload| workload.0.clone())
    }

    fn probe(&mut self, probe: &Probe) -> Result<Facts, String> {
        self.calls.push(Call::Probe(probe.clone()));
        if let Some(gate) = &self.gate {
            gate.recv_timeout(TIMEOUT)
                .map_err(|_| "the test kept the gate shut".to_owned())?;
        }
        self.result(Operation::Probe)?;
        Ok(found(probe))
    }

    fn teardown(&mut self) -> Result<(), String> {
        self.calls.push(Call::Teardown);
        self.announcer = None;
        self.result(Operation::Teardown)
    }

    fn announcer(&self) -> Option<BorrowedFd<'_>> {
        self.announcer.as_ref().map(|(read, _)| read.as_fd())
    }

    /// Returns the announcer's end once the write end of its pipe closed, which collects it.
    fn announcer_ended(&mut self) -> Option<String> {
        let (read, _) = self.announcer.as_ref()?;
        let mut polls = [PollFd::new(read.as_fd(), PollFlags::POLLIN)];
        poll::poll(&mut polls, PollTimeout::ZERO).unwrap();
        let ended = polls[0].revents().is_some_and(|events| !events.is_empty());
        ended.then(|| {
            self.announcer = None;
            "the test ended the announcer".to_owned()
        })
    }

    /// Applies the record as `nvx::guest::clone::apply` does, without a root, once the test opened
    /// the gate, if any: for a GPT disk, it stands for the kernel's reread of the disk's partition
    /// table first, which fails as the kernel does while a partition is open if the test fails
    /// [`Operation::Reread`]; it then takes as long as the assembly, but no longer than until
    /// `deadline`, as the waits for the devices do, and checks the account of the record's workload
    /// identity, if it has one, in existing-account mode, in the [accounts' root](accounts), which
    /// stands for the root that it assembled. It then stands for the generation of the machine ID
    /// that the record asks for, if any, the mount of the record's export through the template's
    /// host filesystem device, if it has one, the write of the resolver file that names the DNS
    /// server of the record's network, if it names one, the start of the announcer of the
    /// workload's listeners, if the network publishes them, and the setting of the record's
    /// hostname and machine ID for every execution, which fail if the test fails
    /// [`Operation::MachineId`], [`Operation::MountExport`], [`Operation::Resolver`],
    /// [`Operation::Announce`], and [`Operation::View`]. A failure after the start stops the
    /// announcer.
    #[cfg(target_arch = "x86_64")]
    fn apply(
        &mut self,
        record: &nvx::activation::ActivationRecord,
        template: &nvx::guest::clone::RootTemplate,
        deadline: Option<Instant>,
    ) -> Result<Option<entry::Identity>, nvx::guest::clone::ApplyError> {
        use nvx::activation::{self, MachineIdMode};
        use nvx::guest::clone::{ApplyError, NetworkError};
        use nvx::guest::ingress::StartError;
        use nvx::guest::root::{DeviceStep, Error, Mount, Role};

        let applied = (record.clone(), template.clone());
        self.calls.push(Call::Apply(Box::new(applied)));
        self.deadlines.push(deadline);
        if let Some(gate) = &self.gate {
            gate.recv_timeout(TIMEOUT)
                .expect("the test opens the gate in time");
        }
        let configuration = record.configuration();
        if matches!(configuration.image, nvx::activation::Image::Gpt { .. }) {
            self.result(Operation::Reread).map_err(|problem| {
                ApplyError::Gpt(Error::Device {
                    role: Role::Distro,
                    step: DeviceStep::Reread,
                    device: PathBuf::from("/dev/vda"),
                    error: io::Error::other(problem),
                })
            })?;
        }
        let assembly = match deadline {
            Some(deadline) => self
                .assembly
                .min(deadline.saturating_duration_since(Instant::now())),
            None => self.assembly,
        };
        thread::sleep(assembly);
        let workload = &configuration.workload;
        let identity = workload
            .identity
            .map(|identity| entry::Identity::new(identity.uid, identity.gid));
        if let Some(identity) = &identity
            && let Err(error) = entry::Entry::new(accounts(), identity.clone()).prepare()
        {
            return Err(ApplyError::Account {
                error,
                teardown: None,
            });
        }
        if workload.machine_id == MachineIdMode::Generated {
            self.result(Operation::MachineId)
                .map_err(|problem| ApplyError::MachineId {
                    error: io::Error::other(problem),
                    teardown: None,
                })?;
        }
        if configuration.export.is_some() {
            self.result(Operation::MountExport)
                .map_err(|problem| ApplyError::Export {
                    error: Error::Mount {
                        mount: Mount::Export {
                            tag: template.filesystem_tag.clone(),
                        },
                        error: io::Error::other(problem),
                    },
                    teardown: None,
                })?;
        }
        let network = configuration.network.as_ref();
        if network.is_some_and(|network| network.dns.is_some()) {
            self.result(Operation::Resolver)
                .map_err(|problem| ApplyError::Network {
                    error: NetworkError::Write(io::Error::other(problem)),
                    teardown: None,
                })?;
        }
        if network.is_some_and(|network| network.publication == activation::Publication::Published)
        {
            if template.gateway.is_none() {
                return Err(ApplyError::Network {
                    error: NetworkError::Gateway,
                    teardown: None,
                });
            }
            self.result(Operation::Announce)
                .map_err(|problem| ApplyError::Network {
                    error: NetworkError::Announcer(StartError::Spawn(io::Error::other(problem))),
                    teardown: None,
                })?;
            self.announcer = Some(match self.ending.take() {
                Some(read) => (read, None),
                None => {
                    let (read, write) = announcer_pipe();
                    (read, Some(write))
                }
            });
        }
        self.result(Operation::View).map_err(|problem| {
            self.announcer = None;
            ApplyError::View {
                error: entry::Error::Root(io::Error::other(problem)),
                teardown: None,
            }
        })?;
        Ok(identity)
    }
}

impl EntryOperations for FakeRoot {
    /// Prepares the entry into the [accounts' root](accounts), whose account databases it reads.
    fn prepare(&mut self, entrance: &Entrance) -> Result<Prepared, entry::Error> {
        self.calls.push(Call::Prepare(entrance.clone()));
        entrance.entry(accounts()).prepare()
    }

    /// Spawns `command` without entering a root, which needs privileges: the program runs in the
    /// test's own root, as the test's own identity, in the process group that it leads.
    fn spawn(&mut self, command: spawn::Command, _prepared: Prepared) -> io::Result<Spawned> {
        command.spawn()
    }
}

/// The user and group IDs of the workload's account in the [accounts' root](accounts).
const WORKLOAD: (u32, u32) = (1000, 1000);

/// The user and group IDs of an account in the [accounts' root](accounts) whose primary group
/// has no group entry.
const ORPHAN: (u32, u32) = (2000, 2000);

/// The `/etc/passwd` of the [accounts' root](accounts).
const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\n\
                      workload:x:1000:1000:workload:/home/workload:/bin/sh\n\
                      orphan:x:2000:2000:orphan:/home/orphan:/bin/sh\n";

/// The `/etc/group` of the [accounts' root](accounts).
const GROUP: &str = "root:x:0:\nstaff:x:50:\nworkload:x:1000:\n";

/// Returns a pipe that stands for the pidfd of a clone's announcer: its read end, which polls
/// readable once the write end closes, and its write end. Both close on exec, so that no program
/// that a test spawns holds the write end, and keeps the announcer from ending.
fn announcer_pipe() -> (OwnedFd, OwnedFd) {
    nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap()
}

/// Returns the directory that stands for the assembled root of every fake root when the worker
/// prepares an entry: one of the tests' own, below the target directory, for the tests' user,
/// with the account databases [`PASSWD`] and [`GROUP`], and nothing else.
fn accounts() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let uid = nix::unistd::getuid();
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("accounts-{uid}"));
        let etc = root.join("etc");
        fs::create_dir_all(&etc).unwrap();
        for (name, contents) in [("passwd", PASSWD), ("group", GROUP)] {
            let path = etc.join(name);
            if fs::read_to_string(&path).is_ok_and(|current| current == contents) {
                continue;
            }
            // Another test process may read the database meanwhile, so it is replaced whole.
            let staged = etc.join(format!("{name}.{}", std::process::id()));
            fs::write(&staged, contents).unwrap();
            fs::rename(&staged, &path).unwrap();
        }
        root
    })
}

/// Returns the facts that a [`FakeRoot`] finds with `probe`.
fn found(probe: &Probe) -> Facts {
    let mut facts = Facts::new();
    facts.push("name", probe.name());
    facts.push("arguments", probe.arguments().join(" "));
    facts
}

/// Returns whether the tests can create cgroups, as where they run as root in a cgroup v2
/// hierarchy with the `pids` controller. Each fake root then has a [`Workload`] cgroup of its
/// own, below which the worker runs its executions in cgroups, and otherwise none, so that the
/// worker runs each program in a process group alone.
fn cgroups() -> bool {
    static CGROUPS: OnceLock<bool> = OnceLock::new();
    *CGROUPS.get_or_init(|| Workload::new().is_some())
}

/// A workload cgroup of a test's own, in the real hierarchy, which it removes, with whatever is
/// left in it, when it drops.
#[derive(Debug)]
struct Workload(Layout);

impl Workload {
    /// Creates a new workload cgroup below the root of the hierarchy at `/sys/fs/cgroup`, with the
    /// test's own cgroup as the agent's, or returns `None` where the test cannot create one, or
    /// the hierarchy lacks the `pids` controller, which the cgroups of executions need.
    fn new() -> Option<Self> {
        static CREATED: AtomicU64 = AtomicU64::new(0);
        let root = Path::new("/sys/fs/cgroup");
        let controllers = fs::read_to_string(root.join("cgroup.controllers")).ok()?;
        if !controllers
            .split_whitespace()
            .any(|controller| controller == "pids")
        {
            return None;
        }
        let own = fs::read_to_string("/proc/self/cgroup").ok()?;
        let own = own.lines().find_map(|line| line.strip_prefix("0::"))?;
        let agent = root.join(own.trim_start_matches('/'));
        let number = CREATED.fetch_add(1, Ordering::Relaxed);
        let name = format!("runtime-adapter-test-{}-{number}", std::process::id());
        let layout = Layout::new(agent, root.join(name)).ok()?;
        fs::create_dir(layout.workload()).ok()?;
        Some(Self(layout))
    }
}

impl Drop for Workload {
    /// Ends whatever is left in the workload cgroup, waits until it is empty, then removes the
    /// cgroups of the executions, and the workload cgroup.
    fn drop(&mut self) {
        let workload = self.0.workload();
        let _ = fs::write(workload.join("cgroup.kill"), b"1");
        let deadline = Instant::now() + TIMEOUT;
        while fs::read(workload.join("pids.current")).is_ok_and(|count| count != b"0\n")
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        for entry in fs::read_dir(workload).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let _ = fs::remove_dir(entry.path());
            }
        }
        let _ = fs::remove_dir(workload);
    }
}

/// Returns a layout that the worker assembles: an EROFS image, with the identity that the guest
/// expects of it, over an ext4 scratch.
fn layout() -> RootLayout {
    RootLayout {
        image: RootImage::erofs(Uuid::parse("0a490905-41be-ed5f-984e-63eb7e5e20b1")),
        upper: RootUpper::Scratch,
    }
}

/// The thread of a worker's session, which returns its root operations, with how the session
/// ended.
type Working = JoinHandle<(FakeRoot, Result<Worked, ServeError>)>;

/// Starts the worker's session in a thread, with the root operations `root`, on one end of a
/// socket pair, and returns the broker on the other end with the thread.
fn start_worker(root: FakeRoot) -> (Broker, Working) {
    let (broker, guest) = UnixStream::pair().unwrap();
    let working = thread::spawn(move || {
        let mut root = root;
        let worked = worker::work(guest, &mut root);
        (root, worked)
    });
    (Broker::new(broker), working)
}

/// Returns the default root configuration's message.
fn configuration() -> Message {
    Message::RootConfig(RootConfig::default().encode())
}

/// Returns the worker's answer to a root configuration once it assembled the root: its
/// readiness, with the guest agent's build.
fn ready() -> Message {
    Message::Ready(Ready::new(GUEST_BUILD).unwrap().encode())
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

/// Returns the host export that the tests' kernel command lines name: a read-write export.
fn export() -> HostExport {
    HostExport {
        tag: "hostfs".to_owned(),
        mode: ExportMode::ReadWrite,
    }
}

/// Returns the default root configuration, which announces `count` mappings.
fn announcing(count: u32) -> RootConfig {
    RootConfig {
        mapping_count: count,
        ..RootConfig::default()
    }
}

/// Returns the configuration of a sandbox whose executions run as the workload identity
/// [`WORKLOAD`], whose account the [accounts' root](accounts) holds.
fn sandbox() -> RootConfig {
    RootConfig {
        workload_identity: Some(WORKLOAD),
        ..RootConfig::default()
    }
}

/// Returns a mapping table of `count` mappings: the export's entry `NNN` at `/srv/table/NNN`, for
/// each index NNN, read-only for the even ones.
fn table(count: usize) -> Vec<HostMap> {
    (0..count)
        .map(|index| HostMap {
            source: format!("{index:03}"),
            target: format!("/srv/table/{index:03}"),
            read_only: index % 2 == 0,
        })
        .collect()
}

/// Returns the bodies of the parts that carry `table`, a [`table`] of up to 1000 mappings, in
/// order, each of at most `per_part` mappings, as `nvx::protocol::encode_maps_payloads` splits it.
fn parts(table: &[HostMap], per_part: usize) -> Vec<Vec<u8>> {
    // The table's mappings have paths of the same lengths.
    let entry = MAP_ENTRY_HEADER_LEN + table[0].source.len() + table[0].target.len();
    encode_maps_payloads(table, MAPS_HEADER_LEN + per_part * entry).unwrap()
}

/// Returns the mappings that the worker applies for `maps`, entries of a mapping table.
fn mappings(maps: &[HostMap]) -> Vec<Mapping> {
    maps.iter()
        .map(|map| {
            let access = if map.read_only {
                Access::ReadOnly
            } else {
                Access::ReadWrite
            };
            Mapping::new(&map.source, &map.target, access).unwrap()
        })
        .collect()
}

/// Returns the worker's `MapFailed`, once it applied `applied` of the table's mappings, with
/// `problem`.
fn map_failed(applied: u32, problem: &str) -> Message {
    Message::MapFailed(MapFailure::new(applied, problem).encode())
}

/// Returns the calls of a [`FakeRoot`] of the exporting kind until the worker answered `Ready` to
/// `config`: it read the kernel command line, bootstrapped the guest, assembled the root, and
/// mounted the host export.
fn exported(config: &RootConfig) -> Vec<Call> {
    vec![
        Call::CommandLine,
        Call::Bootstrap(config.clone()),
        Call::Assemble(config.clone(), layout()),
        Call::MountExport(export()),
    ]
}

/// Returns the probe that `text` names.
fn probe(text: &str) -> Probe {
    Probe::parse(text).unwrap()
}

#[test]
fn the_worker_assembles_the_root_serves_it_and_tears_it_down_on_stop() {
    let (mut broker, working) = start_worker(FakeRoot::booting(&layout()));
    broker.attach(FIRST);
    let replies = broker.exchange(&[configuration()], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [ready()]);

    // It serves pings, echoes, and probes, in records of several sizes.
    let messages = [Message::Ping, longest(), Message::Probe(b"mounts".to_vec())];
    let expected = [
        Message::Pong,
        longest(),
        Message::Report(found(&probe("mounts")).encode()),
    ];
    for size in [7, OUTER_MAX_PAYLOAD] {
        let replies = broker.exchange(&messages, size, expected.len());
        assert_eq!(replies, expected, "records of {size} bytes");
    }

    // Its root outlives a reset of the session, as when the host reattaches. A probe that the
    // framing of probes refuses, or that fails, is answered with its problem.
    broker.settle();
    broker.reset(Identity { epoch: 2, ..FIRST });
    let messages = [
        Message::Probe(b"hold /run".to_vec()),
        Message::Probe(b"Mounts".to_vec()),
    ];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 2);
    assert_eq!(
        replies[0],
        Message::Report(found(&probe("hold /run")).encode())
    );
    let Message::ProbeFailed(problem) = &replies[1] else {
        panic!("{replies:?}");
    };
    let problem = String::from_utf8_lossy(problem);
    assert!(problem.contains("the probe's name \"Mounts\""), "{problem}");

    let replies = broker.exchange(&[Message::Stop], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [Message::Stopped]);
    let (root, worked) = joined(working);
    let worked = worked.unwrap();
    assert_eq!((&worked, worked.status()), (&Worked::Stopped, 0));
    assert!(broker.read().is_none(), "the worker ended its stream");
    let config = RootConfig::default();
    let mut expected = vec![
        Call::CommandLine,
        Call::Bootstrap(config.clone()),
        Call::Assemble(config, layout()),
    ];
    expected.extend([probe("mounts"), probe("mounts"), probe("hold /run")].map(Call::Probe));
    expected.push(Call::Teardown);
    assert_eq!(root.calls, expected);
}

#[test]
fn the_worker_assembles_an_image_over_ram_in_the_overlay_mode_of_its_configuration() {
    let ram = RootLayout {
        upper: RootUpper::Ram,
        ..layout()
    };
    let config = RootConfig {
        overlay_mode: Some(OverlayMode::UntrustedLowers),
        ..RootConfig::default()
    };
    let (mut broker, working) = start_worker(FakeRoot::booting(&ram));
    broker.attach(FIRST);
    let messages = [Message::RootConfig(config.encode()), Message::Stop];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 2);
    assert_eq!(replies, [ready(), Message::Stopped]);
    let (root, worked) = joined(working);
    assert_eq!(worked.unwrap(), Worked::Stopped);
    let expected = [
        Call::CommandLine,
        Call::Bootstrap(config.clone()),
        Call::Assemble(config, ram),
        Call::Teardown,
    ];
    assert_eq!(root.calls, expected);
}

#[test]
fn the_worker_assembles_a_gpt_disk_with_the_first_partition_bound_of_its_configuration() {
    let gpt = RootLayout {
        image: RootImage::gpt(Uuid::parse("6c1f5e0a-2b7d-4c39-9e81-3a5d0f7b2c64")),
        upper: RootUpper::Scratch,
    };
    let config = RootConfig {
        first_partition_bound: Some(4096),
        ..RootConfig::default()
    };
    let (mut broker, working) = start_worker(FakeRoot::booting(&gpt));
    broker.attach(FIRST);
    let messages = [Message::RootConfig(config.encode()), Message::Stop];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 2);
    assert_eq!(replies, [ready(), Message::Stopped]);
    let (root, worked) = joined(working);
    assert_eq!(worked.unwrap(), Worked::Stopped);
    let expected = [
        Call::CommandLine,
        Call::Bootstrap(config.clone()),
        Call::Assemble(config, gpt),
        Call::Teardown,
    ];
    assert_eq!(root.calls, expected);
}

#[test]
fn the_worker_reports_the_stage_that_failed_and_ends_with_its_status() {
    let config = RootConfig::default();
    let read = || vec![Call::CommandLine];
    let bootstrapped = || vec![Call::CommandLine, Call::Bootstrap(config.clone())];
    let ram = RootLayout {
        upper: RootUpper::Ram,
        ..layout()
    };
    // The boot tokens of the layout that the worker assembles, after which a command line may name
    // more.
    let tokens = format!("console=hvc0 nvx_sandbox=1 {}", layout().tokens().join(" "));
    let lacking = RootConfig {
        workload_identity: Some((4242, 4242)),
        ..RootConfig::default()
    };
    let cases = [
        (
            "a configuration of another version",
            FakeRoot::booting(&layout()),
            b"version=2\n".to_vec(),
            RootStage::Configuration,
            "the root configuration has version \"2\"",
            Vec::new(),
        ),
        (
            "a relative runtime directory",
            FakeRoot::booting(&layout()),
            RootConfig {
                runtime: "run/relative".to_owned(),
                ..RootConfig::default()
            }
            .encode(),
            RootStage::Configuration,
            "names the runtime \"run/relative\", which is not absolute",
            Vec::new(),
        ),
        (
            "a kernel command line that cannot be read",
            FakeRoot::failing(Operation::CommandLine, "cannot read /proc/cmdline"),
            config.encode(),
            RootStage::Layout,
            "cannot read /proc/cmdline",
            read(),
        ),
        (
            "no layered root",
            FakeRoot::reading(b"console=hvc0 quiet"),
            config.encode(),
            RootStage::Layout,
            "the kernel command line names no layered root",
            read(),
        ),
        (
            "invalid boot tokens",
            FakeRoot::reading(b"nvx_layer=distro,0xd0003000,erofs"),
            config.encode(),
            RootStage::Layout,
            "the kernel command line has invalid boot tokens: ",
            read(),
        ),
        (
            "a bound of the first partition, for an EROFS image",
            FakeRoot::booting(&layout()),
            RootConfig {
                first_partition_bound: Some(4096),
                ..RootConfig::default()
            }
            .encode(),
            RootStage::Layout,
            "the root configuration bounds the first partition, but the boot tokens name an EROFS \
             image",
            read(),
        ),
        (
            "the size of a scratch, for a RAM upper",
            FakeRoot::booting(&ram),
            RootConfig {
                scratch_sectors: Some(131_072),
                ..RootConfig::default()
            }
            .encode(),
            RootStage::Layout,
            "configures a scratch, but the boot tokens keep the upper in RAM",
            read(),
        ),
        (
            "the journal policy of a scratch, for a RAM upper",
            FakeRoot::booting(&ram),
            RootConfig {
                scratch_journal: Some(JournalPolicy::Allowed),
                ..RootConfig::default()
            }
            .encode(),
            RootStage::Layout,
            "configures a scratch, but the boot tokens keep the upper in RAM",
            read(),
        ),
        (
            "an incomplete host export",
            FakeRoot::reading(format!("{tokens} virtfs_tag=hostfs").as_bytes()),
            config.encode(),
            RootStage::Layout,
            "the kernel command line has invalid tokens of the host export: the host export is \
             incomplete",
            read(),
        ),
        (
            "the token of a live share",
            FakeRoot::reading(format!("{tokens} nvx_share=0:/srv/share:ro").as_bytes()),
            config.encode(),
            RootStage::Layout,
            "the kernel command line has invalid tokens of the host export: the nvx_share kernel \
             parameter binds a live share",
            read(),
        ),
        (
            "mappings without a host export",
            FakeRoot::booting(&layout()),
            announcing(2).encode(),
            RootStage::Layout,
            "the root configuration announces 2 mappings, but the kernel command line names no \
             host export to map from",
            read(),
        ),
        (
            "a bootstrap that failed",
            FakeRoot::failing(Operation::Bootstrap, "failed to create the agent's cgroup"),
            config.encode(),
            RootStage::Bootstrap,
            "failed to create the agent's cgroup",
            bootstrapped(),
        ),
        (
            "an assembly that failed",
            FakeRoot::failing(Operation::Assemble, "the distro layer's UUID is wrong"),
            config.encode(),
            RootStage::Assembly,
            "the distro layer's UUID is wrong",
            [
                bootstrapped(),
                vec![Call::Assemble(config.clone(), layout())],
            ]
            .concat(),
        ),
        (
            "a host export that failed to mount",
            FakeRoot {
                failing: Some((
                    Operation::MountExport,
                    "failed to mount the host export hostfs: No such device (os error 19)",
                )),
                ..FakeRoot::exporting(&layout(), &export())
            },
            config.encode(),
            RootStage::Assembly,
            "failed to mount the host export hostfs",
            exported(&config),
        ),
        (
            "a workload identity whose account the root lacks",
            FakeRoot::booting(&layout()),
            lacking.encode(),
            RootStage::Identity,
            "cannot enter the root as the workload identity 4242:4242: the account is \
             unavailable: no passwd entry has the user ID",
            vec![
                Call::CommandLine,
                Call::Bootstrap(lacking.clone()),
                Call::Assemble(lacking.clone(), layout()),
                Call::Prepare(entrance(entry::Identity::new(4242, 4242))),
            ],
        ),
    ];
    for (name, root, body, stage, problem, calls) in cases {
        let (mut broker, working) = start_worker(root);
        broker.attach(FIRST);
        let replies = broker.exchange(&[Message::RootConfig(body)], OUTER_MAX_PAYLOAD, 1);
        let [Message::RootFailed(reported)] = replies.as_slice() else {
            panic!("{name}: {replies:?}");
        };
        let reported = RootFailure::decode(reported).unwrap();
        assert_eq!(reported.stage(), stage, "{name}: {reported}");
        assert!(reported.problem().contains(problem), "{name}: {reported}");
        // It ends its stream after the failure.
        assert!(broker.read().is_none(), "{name}");
        let (root, worked) = joined(working);
        let worked = worked.unwrap();
        assert_eq!(worked, Worked::Failed(reported), "{name}");
        assert_eq!(worked.status(), stage.exit_status(), "{name}");
        assert_eq!(root.calls, calls, "{name}");
    }
}

/// Returns the entrance of an execution as `identity`, with the default hostname and published
/// listeners, as a configuration that names no publication has them.
fn entrance(identity: entry::Identity) -> Entrance {
    Entrance {
        identity,
        hostname: entry::DEFAULT_HOSTNAME.as_bytes().to_vec(),
        publication: Publication::Published,
    }
}

/// Returns the entrances that the worker prepared through `root`, in order.
fn entrances(root: &FakeRoot) -> Vec<Entrance> {
    root.calls
        .iter()
        .filter_map(|call| match call {
            Call::Prepare(entrance) => Some(entrance.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_worker_checks_the_account_of_the_sandbox_s_workload_identity_in_the_assembled_root() {
    // The check's entry takes the default hostname, as every execution's does.
    let (mut broker, working) = start_worker(FakeRoot::booting(&layout()));
    broker.attach(FIRST);
    let config = sandbox();
    let messages = [Message::RootConfig(config.encode()), Message::Stop];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 2);
    assert_eq!(replies, [ready(), Message::Stopped]);
    let (root, worked) = joined(working);
    assert_eq!(worked.unwrap(), Worked::Stopped);
    let checked = entrance(entry::Identity::new(WORKLOAD.0, WORKLOAD.1));
    let expected = [
        Call::CommandLine,
        Call::Bootstrap(config.clone()),
        Call::Assemble(config, layout()),
        Call::Prepare(checked),
        Call::Teardown,
    ];
    assert_eq!(root.calls, expected);
}

#[test]
fn the_worker_reports_a_teardown_that_failed_before_it_confirms_the_stop() {
    let problem = "failed to unmount the overlay: Device or resource busy (os error 16)";
    let (mut broker, working) = start_worker(FakeRoot::failing(Operation::Teardown, problem));
    broker.attach(FIRST);
    let replies = broker.exchange(&[configuration(), Message::Stop], OUTER_MAX_PAYLOAD, 3);
    let failure = RootFailure::new(RootStage::Teardown, problem);
    let expected = [
        ready(),
        Message::RootFailed(failure.encode()),
        Message::Stopped,
    ];
    assert_eq!(replies, expected);
    let (root, worked) = joined(working);
    let worked = worked.unwrap();
    assert_eq!(worked, Worked::Failed(failure));
    assert_eq!(worked.status(), 15);
    assert_eq!(root.calls.last(), Some(&Call::Teardown));
}

#[test]
fn the_worker_accepts_only_its_configuration_or_a_stop_before_its_root() {
    // A stop ends the worker without a root.
    let (mut broker, working) = start_worker(FakeRoot::booting(&layout()));
    broker.attach(FIRST);
    let replies = broker.exchange(&[Message::Stop], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [Message::Stopped]);
    let (root, worked) = joined(working);
    assert_eq!(worked.unwrap(), Worked::Stopped);
    assert_eq!(root.calls, []);

    // Any other message ends it with an error, as do the messages that only the guest sends, and
    // a clone's activation, which a VM that boots cold does not take.
    let refused = [
        Message::Ping,
        Message::Echo(b"early".to_vec()),
        Message::Probe(b"mounts".to_vec()),
        Message::Maps(maps_part()),
        Message::Start,
        Message::Activate(Vec::new()),
        Message::Pong,
        ready(),
        Message::Report(Vec::new()),
        Message::Mapped,
        Message::MapFailed(MapFailure::new(0, "refused").encode()),
        Message::Started,
    ];
    for message in refused {
        let (mut broker, working) = start_worker(FakeRoot::booting(&layout()));
        broker.attach(FIRST);
        broker.send_data(&message.encode().unwrap(), OUTER_MAX_PAYLOAD);
        let (root, worked) = joined(working);
        let failed =
            matches!(&worked, Err(ServeError::Unexpected(kind)) if *kind == message.kind());
        assert!(failed, "{message:?}: {worked:?}");
        assert_eq!(root.calls, [], "{message:?}");
    }

    // Once the root is ready, a second configuration ends it with an error too, as does a clone's
    // activation, and the worker leaves the root that it assembled to its operations.
    for refused in [configuration(), Message::Activate(Vec::new())] {
        let (mut broker, working) = start_worker(FakeRoot::booting(&layout()));
        broker.attach(FIRST);
        let replies = broker.exchange(&[configuration()], OUTER_MAX_PAYLOAD, 1);
        assert_eq!(replies, [ready()]);
        broker.send_data(&refused.encode().unwrap(), OUTER_MAX_PAYLOAD);
        let (root, worked) = joined(working);
        let failed =
            matches!(&worked, Err(ServeError::Unexpected(kind)) if *kind == refused.kind());
        assert!(failed, "{refused:?}: {worked:?}");
        assert!(!root.calls.contains(&Call::Teardown), "{:?}", root.calls);
    }

    // So does a broker that closes the stream.
    let (mut broker, working) = start_worker(FakeRoot::booting(&layout()));
    broker.attach(FIRST);
    let replies = broker.exchange(&[configuration()], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [ready()]);
    broker.close();
    let (root, worked) = joined(working);
    let worked = worked.unwrap();
    assert_eq!((&worked, worked.status()), (&Worked::Closed, 1));
    assert!(!root.calls.contains(&Call::Teardown), "{:?}", root.calls);
}

#[test]
fn a_broker_that_closes_only_its_end_of_the_stream_receives_what_the_worker_queued() {
    // The worker probes until the test opens the gate, by when the broker sent a ping and closed
    // its end of the stream, so that the worker reads the end of the stream right after it
    // queued the replies. They reach the broker before the worker ends its stream.
    let (root, gate) = FakeRoot::booting(&layout()).gated();
    let (mut broker, working) = start_worker(root);
    broker.attach(FIRST);
    let replies = broker.exchange(&[configuration()], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [ready()]);
    let messages = [Message::Probe(b"mounts".to_vec()), Message::Ping];
    let mut replies = broker.send_data(&encode(&messages), OUTER_MAX_PAYLOAD);
    broker.close();
    gate.send(()).unwrap();
    while let Some(record) = broker.read() {
        broker.check(&record);
        replies.extend(broker.accept(&record));
    }
    let report = Message::Report(found(&probe("mounts")).encode());
    assert_eq!(replies, [report, Message::Pong]);
    let (_, worked) = joined(working);
    let worked = worked.unwrap();
    assert_eq!((&worked, worked.status()), (&Worked::Closed, 1));
}

#[test]
fn the_worker_mounts_the_export_applies_the_mapping_table_in_order_and_starts_the_sandbox() {
    // A table of five mappings, in three parts. The host reattaches after the first, which resets
    // the session: the worker's order outlives the reset, as its root does.
    let table = table(5);
    let parts = parts(&table, 2);
    assert_eq!(parts.len(), 3);
    let config = announcing(5);
    let (mut broker, working) = start_worker(FakeRoot::exporting(&layout(), &export()));
    broker.attach(FIRST);
    let messages = [
        Message::RootConfig(config.encode()),
        Message::Maps(parts[0].clone()),
    ];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 2);
    assert_eq!(replies, [ready(), Message::Mapped]);
    broker.settle();
    broker.reset(Identity { epoch: 2, ..FIRST });
    let messages = [
        Message::Maps(parts[1].clone()),
        Message::Maps(parts[2].clone()),
        Message::Start,
        Message::Ping,
    ];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 4);
    let expected = [
        Message::Mapped,
        Message::Mapped,
        Message::Started,
        Message::Pong,
    ];
    assert_eq!(replies, expected);

    // Once the sandbox started, the worker serves on, and tears the root down on a stop.
    let messages = [Message::Probe(b"mounts".to_vec()), Message::Stop];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 2);
    let report = Message::Report(found(&probe("mounts")).encode());
    assert_eq!(replies, [report, Message::Stopped]);
    let (root, worked) = joined(working);
    assert_eq!(worked.unwrap(), Worked::Stopped);
    let mut expected = exported(&config);
    expected.extend([
        Call::Map(mappings(&table[..2])),
        Call::Map(mappings(&table[2..4])),
        Call::Map(mappings(&table[4..])),
        Call::Probe(probe("mounts")),
        Call::Teardown,
    ]);
    assert_eq!(root.calls, expected);
}

#[test]
fn a_table_of_zero_mappings_does_no_work() {
    // The worker maps nothing, and the host starts the sandbox at once, whether or not the kernel
    // command line names a host export, which the worker mounts all the same.
    let config = RootConfig::default();
    let cases = [
        (FakeRoot::booting(&layout()), None),
        (FakeRoot::exporting(&layout(), &export()), Some(export())),
    ];
    for (root, mounted) in cases {
        let (mut broker, working) = start_worker(root);
        broker.attach(FIRST);
        let messages = [configuration(), Message::Start, Message::Stop];
        let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 3);
        let expected = [ready(), Message::Started, Message::Stopped];
        assert_eq!(replies, expected, "{mounted:?}");
        let (root, worked) = joined(working);
        assert_eq!(worked.unwrap(), Worked::Stopped, "{mounted:?}");
        let mut expected = vec![
            Call::CommandLine,
            Call::Bootstrap(config.clone()),
            Call::Assemble(config.clone(), layout()),
        ];
        expected.extend(mounted.map(Call::MountExport));
        expected.push(Call::Teardown);
        assert_eq!(root.calls, expected);
    }
}

#[test]
fn a_failed_mapping_latches_the_root_and_holds_the_sandbox_back() {
    // The root refuses the table's fourth mapping, in its second part, once it applied the three
    // before it, and latches. The worker answers with their count, then refuses the third part
    // without applying it, and still serves the host, which tears the VM down.
    let table = table(5);
    let parts = parts(&table, 2);
    let problem = "failed to mount the mapping at /srv/table/003: the target is reserved";
    let refusing = || FakeRoot::exporting(&layout(), &export()).refusing("/srv/table/003", problem);
    let config = announcing(5);
    let (mut broker, working) = start_worker(refusing());
    broker.attach(FIRST);
    let mut messages = vec![Message::RootConfig(config.encode())];
    messages.extend(parts.iter().cloned().map(Message::Maps));
    messages.extend([Message::Ping, Message::Stop]);
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 6);
    let expected = [
        ready(),
        Message::Mapped,
        map_failed(3, problem),
        map_failed(3, "the mapping table failed before this part"),
        Message::Pong,
        Message::Stopped,
    ];
    assert_eq!(replies, expected);
    let (root, worked) = joined(working);
    assert_eq!(worked.unwrap(), Worked::Stopped);
    assert!(root.latched);
    let mut expected = exported(&config);
    expected.extend([
        Call::Map(mappings(&table[..2])),
        Call::Map(mappings(&table[2..4])),
        Call::Teardown,
    ]);
    assert_eq!(root.calls, expected);

    // Nor does the worker start the sandbox then: the start ends its session, and the worker
    // leaves the root to its operations.
    let (mut broker, working) = start_worker(refusing());
    broker.attach(FIRST);
    let messages = [
        Message::RootConfig(config.encode()),
        Message::Maps(parts[0].clone()),
        Message::Maps(parts[1].clone()),
    ];
    let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 3);
    assert_eq!(replies[2], map_failed(3, problem));
    broker.send_data(&Message::Start.encode().unwrap(), OUTER_MAX_PAYLOAD);
    let (root, worked) = joined(working);
    assert!(
        matches!(worked, Err(ServeError::Unexpected(25))),
        "{worked:?}"
    );
    assert!(!root.calls.contains(&Call::Teardown), "{:?}", root.calls);
}

#[test]
fn the_worker_refuses_parts_that_do_not_continue_the_table() {
    // The worker refuses such a part as a whole, at the mapping that it expects next, without
    // applying any of the part's mappings, and the start then.
    let table = table(4);
    let parts = parts(&table, 2);
    let maps = |index: usize| Message::Maps(parts[index].clone());
    let cases = [
        (
            "a part that skips mappings",
            4,
            vec![maps(1)],
            0,
            "a part of the mapping table starts at its mapping 2, but the worker expects its \
             mapping 0 next",
        ),
        (
            "a part that repeats mappings",
            4,
            vec![maps(0), maps(0)],
            2,
            "a part of the mapping table starts at its mapping 0, but the worker expects its \
             mapping 2 next",
        ),
        (
            "a part past the mappings that the configuration announced",
            3,
            vec![maps(0), maps(1)],
            2,
            "a part of the mapping table ends it at 4 mappings, past the 3 that the root \
             configuration announced",
        ),
        (
            "a part when the configuration announced no mapping",
            0,
            vec![maps(0)],
            0,
            "a part of the mapping table ends it at 2 mappings, past the 0 that the root \
             configuration announced",
        ),
        (
            "a part that does not decode",
            4,
            vec![Message::Maps(vec![0; 3])],
            0,
            "a part of the mapping table does not decode: ",
        ),
        (
            "a part without a mapping",
            4,
            vec![Message::Maps(vec![0; MAPS_HEADER_LEN])],
            0,
            "a part of the mapping table holds no mapping",
        ),
    ];
    for (name, announced, messages, applied, problem) in cases {
        let config = announcing(announced);
        let (mut broker, working) = start_worker(FakeRoot::exporting(&layout(), &export()));
        broker.attach(FIRST);
        let configuration = [Message::RootConfig(config.encode())];
        let replies = broker.exchange(&configuration, OUTER_MAX_PAYLOAD, 1);
        assert_eq!(replies, [ready()], "{name}");
        let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, messages.len());
        let (refused, mapped) = replies.split_last().unwrap();
        assert!(
            mapped.iter().all(|reply| *reply == Message::Mapped),
            "{name}: {replies:?}"
        );
        let Message::MapFailed(body) = refused else {
            panic!("{name}: {replies:?}");
        };
        let failure = MapFailure::decode(body).unwrap();
        assert_eq!(failure.applied(), applied, "{name}: {failure}");
        assert!(failure.problem().starts_with(problem), "{name}: {failure}");
        broker.send_data(&Message::Start.encode().unwrap(), OUTER_MAX_PAYLOAD);
        let (root, worked) = joined(working);
        assert!(
            matches!(worked, Err(ServeError::Unexpected(25))),
            "{name}: {worked:?}"
        );
        // The first part is the only one that the worker applies before it refuses one.
        let mut expected = exported(&config);
        expected.extend(mapped.iter().map(|_| Call::Map(mappings(&table[..2]))));
        assert_eq!(root.calls, expected, "{name}");
    }
}

#[test]
fn the_worker_refuses_executions_and_the_start_out_of_order() {
    // Each refused message ends the session once the worker answered those before it, and the
    // worker leaves the root to its operations.
    let table = table(4);
    let parts = parts(&table, 2);
    let maps = |index: usize| Message::Maps(parts[index].clone());
    let execution = HostMessage::Start(start_of(1, shell("exit 0"))).encode();
    let credit = HostMessage::Credit(Key::new(1, 1), Pool::Output, 1).encode();
    let cases = [
        ("an execution before the table", vec![], execution.clone()),
        ("credit of an execution before the table", vec![], credit),
        (
            "an execution once part of the table is applied",
            vec![maps(0)],
            execution.clone(),
        ),
        (
            "an execution once the table is applied, before the start",
            vec![maps(0), maps(1)],
            execution,
        ),
        ("a start before the table", vec![], Message::Start),
        (
            "a start once part of the table is applied",
            vec![maps(0)],
            Message::Start,
        ),
        (
            "a second start",
            vec![maps(0), maps(1), Message::Start],
            Message::Start,
        ),
        (
            "a part of the table once the sandbox started",
            vec![maps(0), maps(1), Message::Start],
            maps(1),
        ),
    ];
    for (name, before, refused) in cases {
        let (mut broker, working) = start_worker(FakeRoot::exporting(&layout(), &export()));
        broker.attach(FIRST);
        let mut messages = vec![Message::RootConfig(announcing(4).encode())];
        messages.extend(before);
        let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, messages.len());
        let expected = messages
            .iter()
            .map(|message| match message {
                Message::RootConfig(_) => ready(),
                Message::Maps(_) => Message::Mapped,
                _ => Message::Started,
            })
            .collect::<Vec<_>>();
        assert_eq!(replies, expected, "{name}");
        broker.send_data(&refused.encode().unwrap(), OUTER_MAX_PAYLOAD);
        let (root, worked) = joined(working);
        let failed =
            matches!(&worked, Err(ServeError::Unexpected(kind)) if *kind == refused.kind());
        assert!(failed, "{name}: {worked:?}");
        assert!(
            !root.calls.contains(&Call::Teardown),
            "{name}: {:?}",
            root.calls
        );
    }
}

/// Starts the guest binary with `arguments`, its standard input closed and its output piped.
fn spawn_binary(arguments: &[&std::ffi::OsStr]) -> GuestProcess {
    spawn_binary_at(
        Path::new(env!("CARGO_BIN_EXE_runtime-adapter-guest")),
        arguments,
    )
}

/// Starts the guest binary at `binary` with `arguments`, its standard input closed and its output
/// piped.
fn spawn_binary_at(binary: &Path, arguments: &[&std::ffi::OsStr]) -> GuestProcess {
    GuestProcess::spawn(
        Command::new(binary)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
}

#[test]
fn the_guest_binary_runs_pid_1_and_the_worker_of_a_layered_root() {
    use std::ffi::OsStr;

    let init = OsStr::new(runtime_adapter::GUEST_INIT_COMMAND);
    let layered = OsStr::new(runtime_adapter::GUEST_LAYERED_MODE);
    let worker = OsStr::new(runtime_adapter::GUEST_WORKER_COMMAND);

    // PID 1 starts the worker on the tty, which reports that the test's host boots no layered
    // root. Unless it is PID 1, PID 1 then ends its unrelated child, and exits with the worker's
    // status, with which PID 1 would end the VM.
    let (mut broker, pty) = terminal();
    let child = spawn_binary(&[init, pty.path.as_os_str(), layered]);
    broker.attach(FIRST);
    let replies = broker.exchange(&[configuration()], OUTER_MAX_PAYLOAD, 1);
    let [Message::RootFailed(reported)] = replies.as_slice() else {
        panic!("{replies:?}");
    };
    let reported = RootFailure::decode(reported).unwrap();
    assert_eq!(reported.stage(), RootStage::Layout, "{reported}");
    let output = child.finished();
    assert_eq!(output.status.code(), Some(12), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("runtime-adapter-guest: {reported}\n")
    );

    // The worker alone, with and without the unrelated child's process ID, stops before it
    // configures a root.
    for sibling in [None, Some(OsStr::new("1"))] {
        let (mut broker, pty) = terminal();
        let mut arguments = vec![worker, pty.path.as_os_str()];
        arguments.extend(sibling);
        let child = spawn_binary(&arguments);
        broker.attach(FIRST);
        let replies = broker.exchange(&[Message::Stop], OUTER_MAX_PAYLOAD, 1);
        assert_eq!(replies, [Message::Stopped]);
        let output = child.finished();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    }

    // A worker that cannot open its tty fails at once, and PID 1 exits with its status.
    for arguments in [
        [init, OsStr::new("/"), layered],
        [worker, OsStr::new("/"), OsStr::new("7")],
    ] {
        let output = spawn_binary(&arguments).finished();
        assert_eq!(output.status.code(), Some(1), "{arguments:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with("runtime-adapter-guest: cannot open /: "),
            "{arguments:?}: {stderr}"
        );
    }

    // The unrelated child sleeps until a signal ends it.
    let idle = spawn_binary(&[OsStr::new(runtime_adapter::GUEST_IDLE_COMMAND)]);
    thread::sleep(Duration::from_millis(200));
    assert!(!idle.exited());
    drop(idle);

    // Anything else is a usage error.
    let usage: [&[&str]; 5] = [
        &["init", "/dev/null", "blockless"],
        &["worker"],
        &["worker", "/dev/null", "sibling"],
        &["worker", "/dev/null", "1", "2"],
        &["idle", "now"],
    ];
    for arguments in usage {
        let arguments = arguments.iter().map(OsStr::new).collect::<Vec<_>>();
        let output = spawn_binary(&arguments).finished();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
    }
}

/// Starts PID 1 of a layered root on the tty of `pty`, and waits until its unrelated child and
/// its worker, which waits for the broker, run too, all of them with the binary's output. Returns
/// the binary, and the processes of its group.
fn layered_pid_1(pty: &Terminal) -> (GuestProcess, Vec<u32>) {
    layered_pid_1_at(Path::new(env!("CARGO_BIN_EXE_runtime-adapter-guest")), pty)
}

/// Starts PID 1 of a layered root as `layered_pid_1` does, from the guest binary at `binary`,
/// from which PID 1 starts its children too.
fn layered_pid_1_at(binary: &Path, pty: &Terminal) -> (GuestProcess, Vec<u32>) {
    use std::ffi::OsStr;

    let child = spawn_binary_at(
        binary,
        &[
            OsStr::new(runtime_adapter::GUEST_INIT_COMMAND),
            pty.path.as_os_str(),
            OsStr::new(runtime_adapter::GUEST_LAYERED_MODE),
        ],
    );
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let running = running_in_group(child.id());
        if running.len() == 3 {
            return (child, running);
        }
        assert!(
            Instant::now() < deadline,
            "PID 1 did not start its children: {running:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Ends PID 1 of a layered root, which `layered_pid_1` started, before its children, as a crash
/// would, and waits until it exited.
fn crash(pid_1: &GuestProcess) {
    signal::kill(
        Pid::from_raw(i32::try_from(pid_1.id()).unwrap()),
        Signal::SIGKILL,
    )
    .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while !pid_1.exited() {
        assert!(Instant::now() < deadline, "PID 1 did not end");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until no process of the process group `group` runs.
fn ended(group: u32) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let running = running_in_group(group);
        if running.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "{running:?} still run");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_test_that_gives_up_on_pid_1_ends_every_process_that_it_started() {
    // Giving up kills the whole group, so that the output, which every process of the group
    // holds, ends, and none of the processes runs on.
    let (_broker, pty) = terminal();
    let (child, _) = layered_pid_1(&pty);
    let group = child.id();
    let unfinished = child.finished_within(Duration::from_millis(100));
    let Err(Unfinished::Late(output)) = unfinished else {
        panic!("{unfinished:?}");
    };
    assert_eq!(
        output.status.signal(),
        Some(Signal::SIGKILL as i32),
        "{output:?}"
    );
    ended(group);
}

#[test]
fn a_test_ends_every_process_that_outlives_pid_1() {
    // PID 1 ends before its children, which hold its output. The test fails, and kills them,
    // rather than wait for the output to end.
    let (_broker, pty) = terminal();
    let (child, mut children) = layered_pid_1(&pty);
    let group = child.id();
    crash(&child);
    let unfinished = child.finished_within(TIMEOUT);
    let Err(Unfinished::Orphans(output, mut orphans)) = unfinished else {
        panic!("{unfinished:?}");
    };
    assert_eq!(
        output.status.signal(),
        Some(Signal::SIGKILL as i32),
        "{output:?}"
    );
    children.retain(|&pid| pid != group);
    children.sort_unstable();
    orphans.sort_unstable();
    assert_eq!(orphans, children);
    ended(group);

    // A test that fails once PID 1 ended kills them too.
    let (_broker, pty) = terminal();
    let (child, _) = layered_pid_1(&pty);
    let group = child.id();
    crash(&child);
    drop(child);
    ended(group);
}

/// Returns which of `processes`, those of PID 1's group that `layered_pid_1` returned, runs the
/// guest binary's `command`, once it runs it: a process may not have started the binary yet.
fn running_command(processes: &[u32], command: &str) -> u32 {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let found = processes.iter().copied().find(|id| {
            let arguments = fs::read(format!("/proc/{id}/cmdline")).unwrap_or_default();
            arguments.split(|&byte| byte == 0).nth(1) == Some(command.as_bytes())
        });
        if let Some(id) = found {
            return id;
        }
        assert!(Instant::now() < deadline, "no process runs {command}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Sends the real-time signal `signal`, which `nix` does not name, to the processes `ids` at once.
fn signal_real_time(ids: &[u32], signal: u32) {
    let ids = ids.iter().map(u32::to_string).collect::<Vec<_>>();
    let kill = format!("kill -{signal} {}", ids.join(" "));
    let status = Command::new("/bin/sh")
        .args(["-c", &kill])
        .status()
        .unwrap();
    assert!(status.success(), "{kill}: {status}");
}

#[test]
fn pid_1_collects_what_real_time_signals_leave_and_supervises_on() {
    // `nix` decodes no real-time signal in a status. One ends PID 1's unrelated child here, as it
    // may end a process that PID 1 adopted, such as one that an execution left: PID 1 collects
    // its status, and goes on with the worker, which serves the host until the host stops it.
    let (mut broker, pty) = terminal();
    let (child, processes) = layered_pid_1(&pty);
    let sibling = running_command(&processes, runtime_adapter::GUEST_IDLE_COMMAND);
    signal_real_time(&[sibling], 40);
    reaped(sibling);
    broker.attach(FIRST);
    let replies = broker.exchange(&[Message::Stop], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [Message::Stopped]);
    let output = child.finished();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");

    // One that ends the worker, alone or as it ends the unrelated child too, ends PID 1 with the
    // status that a shell reports for the worker.
    for with_sibling in [false, true] {
        let (_broker, pty) = terminal();
        let (child, processes) = layered_pid_1(&pty);
        let worker = running_command(&processes, runtime_adapter::GUEST_WORKER_COMMAND);
        let sibling = running_command(&processes, runtime_adapter::GUEST_IDLE_COMMAND);
        let ended = if with_sibling {
            vec![sibling, worker]
        } else {
            vec![worker]
        };
        signal_real_time(&ended, 40);
        let output = child.finished();
        assert_eq!(
            output.status.code(),
            Some(128 + 40),
            "{with_sibling}: {output:?}"
        );
        assert!(output.stderr.is_empty(), "{with_sibling}: {output:?}");
    }
}

#[test]
fn pid_1_finds_the_children_that_real_time_signals_end_whatever_their_names() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // The kernel names a process after the file name of its program, cut to 15 bytes, which need
    // not be UTF-8: a file name may hold any byte, and a cut may split a character. PID 1 starts
    // its children from its own program, here a link to the guest binary whose name is no UTF-8.
    // It still finds its unrelated child once a real-time signal ended it, and supervises on.
    let directory =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("pid-1-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let binary = directory.join(OsStr::from_bytes(b"guest-\xff"));
    // A hard link rather than a copy, which a child that another test starts could hold open for
    // writing, and so keep from running.
    fs::hard_link(env!("CARGO_BIN_EXE_runtime-adapter-guest"), &binary).unwrap();
    let (mut broker, pty) = terminal();
    let (child, processes) = layered_pid_1_at(&binary, &pty);
    let sibling = running_command(&processes, runtime_adapter::GUEST_IDLE_COMMAND);
    let stat = fs::read(format!("/proc/{sibling}/stat")).unwrap();
    let lossy = String::from_utf8_lossy(&stat);
    assert!(std::str::from_utf8(&stat).is_err(), "{lossy}");
    signal_real_time(&[sibling], 40);
    reaped(sibling);
    broker.attach(FIRST);
    let replies = broker.exchange(&[Message::Stop], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [Message::Stopped]);
    let output = child.finished();
    fs::remove_dir_all(&directory).unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

/// The grace of pending output of both sides of the executions in these tests.
const GRACE: Duration = Duration::from_millis(300);
/// The settlement time of both sides of the executions in these tests.
const SETTLEMENT: Duration = Duration::from_secs(5);
/// The chunk of the executions in these tests.
const CHUNK: u32 = 16 * 1024;

/// Returns the limits of the executions in these tests, as the worker's adapter chooses them:
/// chunks of 16 KiB, and frame-counted credit, of 4 frames of input, and of 8 frames of output
/// in one pool that standard output and error share.
fn shared_limits() -> Limits {
    Limits::new(CHUNK, Window::frames(4), Window::frames(8))
        .unwrap()
        .shared_output()
}

/// Returns the timers of both sides of the executions in these tests.
fn timers() -> Timers {
    Timers::new(GRACE, SETTLEMENT)
}

/// Returns the start of the execution `id` of the first generation, with the limits and timers of
/// these tests, of the program that `arguments` names.
fn start_of(id: u64, arguments: Vec<Vec<u8>>) -> Start {
    Start::new(Key::new(id, 1), shared_limits(), timers(), arguments)
}

/// Returns the arguments that run `/bin/sh -c script`.
fn shell(script: &str) -> Vec<Vec<u8>> {
    vec![
        b"/bin/sh".to_vec(),
        b"-c".to_vec(),
        script.as_bytes().to_vec(),
    ]
}

/// Returns the output of `seq first last`.
fn lines(first: usize, last: usize) -> Vec<u8> {
    (first..=last)
        .map(|number| format!("{number}\n"))
        .collect::<String>()
        .into_bytes()
}

/// Waits until no process of the process group `group` runs, as once an execution ended the
/// program that leads it.
fn gone(group: u32) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let running = running_in_group(group);
        if running.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "{running:?} still run");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until the process `id`, which ended, is no zombie of the test's process any more, as
/// once the worker collected its status.
fn collected(id: u32) {
    let parent = std::process::id().to_string();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        // The state comes first, then the parent's process ID.
        let zombie = stat_fields(id).is_some_and(
            |fields| matches!(fields.as_slice(), [state, of, ..] if state == "Z" && *of == parent),
        );
        if !zombie {
            return;
        }
        assert!(Instant::now() < deadline, "{id} stays a zombie");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until the process `id` is gone, as once it ended and the worker collected its status:
/// `/proc` no longer lists it.
fn reaped(id: u32) {
    let deadline = Instant::now() + TIMEOUT;
    while fs::exists(format!("/proc/{id}")).unwrap_or(true) {
        assert!(Instant::now() < deadline, "{id} is still there");
        thread::sleep(Duration::from_millis(10));
    }
}

/// What the tests read of the host's side of one of the worker's executions, a
/// [`HostExecution`], besides what it tells itself.
trait Taken {
    /// Returns the bytes of each stream that the sink took, in the order of [`Stream::ALL`].
    fn outputs(&self) -> [Vec<u8>; 2];

    /// Returns the process ID that the program printed on its first line, that of the process
    /// group that it leads.
    fn group(&self) -> u32;
}

impl Taken for HostExecution {
    fn outputs(&self) -> [Vec<u8>; 2] {
        Stream::ALL.map(|stream| self.output(stream).to_vec())
    }

    fn group(&self) -> u32 {
        let output = String::from_utf8_lossy(self.output(Stream::Stdout));
        output.lines().next().unwrap().parse().unwrap()
    }
}

/// The host of a test of the worker's executions: the broker, the worker's thread, the host's
/// side of each execution, by ID, and the guest's other messages.
struct Host {
    /// The broker's end of the guest's session.
    broker: Broker,
    /// The worker's thread, until the host stops the worker.
    working: Option<Working>,
    /// The host's side of each execution, by ID.
    executions: BTreeMap<u64, HostExecution>,
    /// The guest's messages about anything but executions, in order.
    replies: Vec<Message>,
    /// Whether the host stops the worker, which then ends its stream.
    stopping: bool,
}

impl Host {
    /// Starts a worker whose root operations succeed, and returns the host, once the worker
    /// assembled its root for a [sandbox](sandbox) whose executions run as its workload identity.
    fn new() -> Self {
        Self::configured(FakeRoot::booting(&layout()), &sandbox())
    }

    /// Starts a worker with the root operations `root`, and returns the host, once the worker
    /// assembled its root as `config` configures it.
    fn configured(root: FakeRoot, config: &RootConfig) -> Self {
        let (broker, working) = start_worker(root);
        Self::ready(broker, working, config)
    }

    /// Attaches the session of the worker that runs in `working` in the first epoch, has it
    /// assemble its root as `config` configures it, starts the sandbox, of no mapping, and returns
    /// the host.
    fn ready(mut broker: Broker, working: Working, config: &RootConfig) -> Self {
        broker.attach(FIRST);
        let messages = [Message::RootConfig(config.encode()), Message::Start];
        let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, 2);
        assert_eq!(replies, [ready(), Message::Started]);
        Self {
            broker,
            working: Some(working),
            executions: BTreeMap::new(),
            replies: Vec::new(),
            stopping: false,
        }
    }

    /// Starts the execution of `start`, whose source hands over `input` if the start says that
    /// the host sends the program's input, and sends `after` right after the start.
    fn start_then(&mut self, start: Start, input: Vec<u8>, after: &[Message]) {
        let mut messages = vec![HostMessage::Start(start.clone()).encode()];
        messages.extend_from_slice(after);
        let execution = HostExecution::new(start, timers(), input, Instant::now());
        self.executions
            .insert(execution.start().key.id(), execution);
        self.send(&messages);
    }

    /// Starts the execution of `start`, as [`start_then`](Self::start_then) does.
    fn start(&mut self, start: Start, input: Vec<u8>) {
        self.start_then(start, input, &[]);
    }

    /// Returns the host's side of the execution `id`.
    fn execution(&mut self, id: u64) -> &mut HostExecution {
        self.executions.get_mut(&id).expect("the host started it")
    }

    /// Returns the outcome of the execution `id`, once the host's side finished.
    fn outcome(&self, id: u64) -> Option<Outcome> {
        self.executions[&id].outcome()
    }

    /// Sends `messages`, and hands over what the guest sent meanwhile.
    fn send(&mut self, messages: &[Message]) {
        if !messages.is_empty() {
            let received = self.broker.send_data(&encode(messages), OUTER_MAX_PAYLOAD);
            self.dispatch(received);
        }
    }

    /// Hands each of the guest's `messages` about an execution to the host's side of the
    /// execution, and keeps the others.
    fn dispatch(&mut self, messages: Vec<Message>) {
        let now = Instant::now();
        for message in messages {
            let Some(decoded) = GuestMessage::decode(&message) else {
                self.replies.push(message);
                continue;
            };
            let decoded = decoded.expect("the guest's message about an execution decodes");
            let execution = self
                .executions
                .get_mut(&decoded.key().id())
                .expect("the guest's message names an execution that the host started");
            execution
                .deliver(&decoded, now)
                .expect("the peer accepts the guest's message");
        }
    }

    /// Takes what the guest sent, waiting a little if it sent nothing, then takes every step of
    /// the host's side of each execution, and sends their messages, unless the host stops the
    /// worker, which ends its stream once it confirmed the stop.
    fn pump(&mut self) {
        let mut wait = Duration::from_millis(5);
        while let Some(messages) = self.broker.take_within(wait) {
            self.dispatch(messages);
            wait = Duration::ZERO;
        }
        if self.broker.closed && !self.stopping {
            let worked = self.working.take().map(|working| joined(working).1);
            panic!("the worker ended its stream: {worked:?}");
        }
        let now = Instant::now();
        let messages = self
            .executions
            .values_mut()
            .flat_map(|execution| execution.serve(now))
            .collect::<Vec<_>>();
        if !self.stopping {
            self.send(&messages);
        }
    }

    /// Pumps until `done` holds, and fails if it does not in time.
    fn run_until(&mut self, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(self) {
            if Instant::now() >= deadline {
                let ended = self.working.take().filter(JoinHandle::is_finished);
                panic!(
                    "the executions did not move on in time; the worker: {:?}",
                    ended.map(|working| working.join().unwrap().1)
                );
            }
            self.pump();
        }
    }

    /// Stops the worker, and returns its root operations with how its session ended.
    fn stop(mut self) -> (FakeRoot, Result<Worked, ServeError>) {
        self.stopping = true;
        self.send(&[Message::Stop]);
        self.run_until(|host| host.replies.contains(&Message::Stopped));
        joined(self.working.take().unwrap())
    }
}

impl Drop for Host {
    /// Ends the worker's session, as once a test failed before it stopped the worker, even while
    /// the broker pauses: the broker reads the guest's stream again, and shuts its socket down,
    /// so that the worker, whose stream ended, drops its executions, which ends their programs.
    /// Once the worker ended, this changes nothing.
    fn drop(&mut self) {
        self.broker.resume();
        let _ = self.broker.stream.shutdown(Shutdown::Both);
    }
}

/// Checks that the execution `id` ended on its own with `status`, losing no byte, and settled if
/// its program ran in a cgroup of its own, where the tests can create cgroups: otherwise it is
/// fenced, as nothing proves that the descendants of a program without a cgroup are gone.
fn check_lossless(host: &Host, id: u64, status: Status) {
    let outcome = host.outcome(id).unwrap();
    assert_eq!(outcome.status(), Some(status), "{id}");
    assert_eq!(outcome.cause(), None, "{id}");
    assert_eq!(outcome.launch_failure(), None, "{id}");
    for stream in Stream::ALL {
        assert!(!outcome.is_truncated(stream), "{id}: {stream:?}");
    }
    assert_eq!(outcome.is_reusable(), cgroups(), "{id}");
    let peer = host.executions[&id].peer();
    assert!(peer.high_water() <= peer.memory_bound(), "{id}");
}

#[test]
fn the_worker_runs_executions_at_once_with_frame_counted_credit_and_one_output_pool() {
    // Three executions stream at once through windows of 4 and 8 frames of 16 KiB, in which
    // standard output and error charge one pool: standard output of more than a MiB, 1.5 MiB of
    // input through a program that copies it, and both streams of a program that fails. Each
    // stream arrives byte-exact, and the host's ping is answered as they stream.
    let mut host = Host::new();
    let input = (0..3 * 512 * 1024)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    host.start(start_of(1, shell("seq 1 300000")), Vec::new());
    host.start(
        start_of(2, vec![b"/bin/cat".to_vec()]).with_input(),
        input.clone(),
    );
    let both = "seq 1 30000; seq 30001 60000 >&2; exit 7";
    host.start(start_of(3, shell(both)), Vec::new());
    host.run_until(|host| !host.executions[&1].output(Stream::Stdout).is_empty());
    host.send(&[Message::Ping]);
    host.run_until(|host| {
        host.replies.contains(&Message::Pong) && (1..=3).all(|id| host.outcome(id).is_some())
    });
    assert_eq!(host.replies, [Message::Pong]);
    assert_eq!(
        host.executions[&1].outputs(),
        [lines(1, 300_000), Vec::new()]
    );
    assert_eq!(host.executions[&2].outputs(), [input, Vec::new()]);
    assert_eq!(
        host.executions[&3].outputs(),
        [lines(1, 30_000), lines(30_001, 60_000)]
    );
    check_lossless(&host, 1, Status::Exited(0));
    check_lossless(&host, 2, Status::Exited(0));
    check_lossless(&host, 3, Status::Exited(7));
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn a_stalled_sink_stalls_only_its_own_execution_while_timers_cancellations_and_control_go_on() {
    // The host's sink of the first execution takes nothing, so its window runs out, and its
    // program blocks once its pipe is full.
    let mut host = Host::new();
    host.start(start_of(1, shell("exec cat /dev/zero")), Vec::new());
    host.execution(1).stall(true);
    // Another execution streams to its end meanwhile, a third one's timeout ends it in time, and
    // the host's ping is answered.
    host.start(start_of(2, shell("seq 1 200000")), Vec::new());
    let timeout = Duration::from_millis(500);
    let timed = Start::new(
        Key::new(3, 1),
        shared_limits(),
        timers().timeout(timeout),
        shell("exec sleep 60"),
    );
    let started = Instant::now();
    host.start(timed, Vec::new());
    host.send(&[Message::Ping]);
    host.run_until(|host| {
        host.replies.contains(&Message::Pong)
            && host.outcome(2).is_some()
            && host.outcome(3).is_some()
    });
    assert_eq!(
        host.executions[&2].outputs(),
        [lines(1, 200_000), Vec::new()]
    );
    check_lossless(&host, 2, Status::Exited(0));
    let timed = host.outcome(3).unwrap();
    assert_eq!(timed.cause(), Some(Cause::TimedOut));
    assert!(started.elapsed() < timeout + GRACE + Duration::from_secs(2));
    let stalled = &host.executions[&1];
    assert!(stalled.outcome().is_none() && stalled.peer().buffered() > 0);
    assert!(stalled.output(Stream::Stdout).is_empty());

    // The stalled execution's cancellation ends it within its grace, with its output truncated.
    let cancelled = Instant::now();
    host.execution(1).peer_mut().cancel(cancelled);
    host.run_until(|host| host.outcome(1).is_some());
    assert!(cancelled.elapsed() < GRACE + Duration::from_secs(2));
    let outcome = host.outcome(1).unwrap();
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    assert!(outcome.is_truncated(Stream::Stdout));
    assert!(!outcome.is_truncated(Stream::Stderr));
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn the_worker_refuses_probes_while_it_runs_executions() {
    // Each probe waits until the test opens the gate, which it keeps shut while the execution
    // runs: a probe that ran then would hold back the execution's timeout.
    let (root, gate) = FakeRoot::booting(&layout()).gated();
    let (broker, working) = start_worker(root);
    let mut host = Host::ready(broker, working, &sandbox());
    let timeout = Duration::from_millis(500);
    let timed = Start::new(
        Key::new(1, 1),
        shared_limits(),
        timers().timeout(timeout),
        shell("exec sleep 60"),
    );
    let probe_mounts = Message::Probe(b"mounts".to_vec());
    let started = Instant::now();
    host.start_then(timed, Vec::new(), std::slice::from_ref(&probe_mounts));
    host.run_until(|host| !host.replies.is_empty() && host.outcome(1).is_some());
    let refused = Message::ProbeFailed(worker::EXECUTING.as_bytes().to_vec());
    assert_eq!(host.replies, [refused]);
    assert_eq!(host.outcome(1).unwrap().cause(), Some(Cause::TimedOut));
    assert!(started.elapsed() < timeout + GRACE + Duration::from_secs(2));

    // Once the execution ended, the worker probes again.
    gate.send(()).unwrap();
    host.send(&[probe_mounts]);
    host.run_until(|host| host.replies.len() == 2);
    let report = Message::Report(found(&probe("mounts")).encode());
    assert_eq!(host.replies[1], report);
    let (root, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
    let probes = root
        .calls
        .iter()
        .filter(|call| matches!(call, Call::Probe(_)));
    assert_eq!(
        probes.count(),
        1,
        "the worker ran only the probe that it took"
    );
}

#[test]
fn a_full_stream_to_the_host_holds_back_only_output_while_cancellations_and_stops_go_on() {
    // Four programs print the process ID of the group that they lead, then write zeros without
    // end, each with an output window of 40 frames of 16 KiB: together, more than the worker
    // queues for the stream before its executions take no more output.
    let mut host = Host::new();
    let limits = Limits::new(CHUNK, Window::frames(4), Window::frames(40))
        .unwrap()
        .shared_output();
    for id in 1..=4 {
        let zeros = shell("echo $$; exec cat /dev/zero");
        host.start(
            Start::new(Key::new(id, 1), limits, timers(), zeros),
            Vec::new(),
        );
    }
    host.run_until(|host| {
        (1..=4).all(|id| host.executions[&id].output(Stream::Stdout).contains(&b'\n'))
    });
    let groups = (1..=4)
        .map(|id| host.executions[&id].group())
        .collect::<Vec<_>>();

    // The broker stops reading the guest's stream, which fills up, and the worker's records
    // back up behind it.
    host.broker.pause();
    thread::sleep(Duration::from_millis(500));

    // The worker still takes the host's messages: a cancellation ends its program, and a stop
    // the others, while the stream stays full.
    assert!(host.broker.room >= 1024, "{}", host.broker.room);
    let now = Instant::now();
    host.execution(1).peer_mut().cancel(now);
    let cancel = host.execution(1).serve(now);
    host.send(&cancel);
    gone(groups[0]);
    host.stopping = true;
    host.send(&[Message::Stop]);
    for &group in &groups[1..] {
        gone(group);
    }

    // Once the broker reads again, the worker sends what waited, then confirms the stop.
    host.broker.resume();
    host.run_until(|host| host.replies.contains(&Message::Stopped));
    let (_, worked) = joined(host.working.take().unwrap());
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn executions_that_write_without_end_take_turns_at_the_stream_to_the_host() {
    // Two programs write zeros without end, each with an output window of 256 frames of 16 KiB:
    // 4 MiB, more than the worker queues for the stream before its executions take no more
    // output, so that either could keep the stream full alone.
    let mut host = Host::new();
    let limits = Limits::new(CHUNK, Window::frames(4), Window::frames(256))
        .unwrap()
        .shared_output();
    for id in 1..=2 {
        let zeros = shell("exec cat /dev/zero");
        host.start(
            Start::new(Key::new(id, 1), limits, timers(), zeros),
            Vec::new(),
        );
    }

    // Each takes a fair part of what the stream carries.
    let taken = |host: &Host, id: u64| host.executions[&id].output(Stream::Stdout).len();
    let total = |host: &Host| taken(host, 1) + taken(host, 2);
    host.run_until(|host| total(host) >= 16 * 1024 * 1024);
    let total = total(&host);
    for id in 1..=2 {
        let taken = taken(&host, id);
        assert!(taken >= total / 4, "{id}: {taken} of {total}");
    }
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn the_worker_refuses_starts_beyond_its_limits_with_a_failed_launch() {
    // The worker runs at most its limit of executions at once, and refuses the next start.
    let mut host = Host::new();
    let most = u64::try_from(MAX_EXECUTIONS).unwrap();
    for id in 1..=most {
        host.start(start_of(id, shell("exec sleep 60")), Vec::new());
    }
    host.run_until(|host| {
        host.executions
            .values()
            .all(|side| side.peer().has_started())
    });
    host.start(start_of(most + 1, shell("exit 0")), Vec::new());
    host.run_until(|host| host.outcome(most + 1).is_some());
    let refused = host.outcome(most + 1).unwrap();
    let again = LaunchFailure::new(0, Errno::EAGAIN as i32);
    assert_eq!(refused.launch_failure(), Some(again));
    assert_eq!(refused.status(), None);
    assert!(!refused.is_reusable());
    assert!(!host.executions[&(most + 1)].peer().has_started());

    // Once those ended, it refuses an execution that would hold more than it holds for one, and
    // reports a program that it cannot find.
    let now = Instant::now();
    for id in 1..=most {
        host.execution(id).peer_mut().cancel(now);
    }
    host.run_until(|host| (1..=most).all(|id| host.outcome(id).is_some()));
    for id in 1..=most {
        let outcome = host.outcome(id).unwrap();
        assert_eq!(outcome.cause(), Some(Cause::Cancelled), "{id}");
        assert_eq!(
            outcome.status(),
            Some(Status::Signaled(Signal::SIGKILL as i32))
        );
    }
    let wide = Limits::new(MAX_CHUNK, Window::frames(4), Window::frames(4)).unwrap();
    let start_wide = Start::new(Key::new(20, 1), wide, timers(), shell("exit 0"));
    host.start(start_wide, Vec::new());
    host.start(
        start_of(21, vec![b"/nonexistent/program".to_vec()]),
        Vec::new(),
    );
    host.run_until(|host| host.outcome(20).is_some() && host.outcome(21).is_some());
    let memory = LaunchFailure::new(0, Errno::ENOMEM as i32);
    assert_eq!(host.outcome(20).unwrap().launch_failure(), Some(memory));
    let missing = host.outcome(21).unwrap();
    let exec = LaunchFailure::new(21, Errno::ENOENT as i32);
    assert_eq!(missing.launch_failure(), Some(exec));
    assert_eq!(missing.status(), Some(Status::Exited(125)));
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn the_worker_ends_its_session_on_messages_about_executions_that_break_their_rules() {
    // Before its root, the worker runs no execution.
    let (mut broker, working) = start_worker(FakeRoot::booting(&layout()));
    broker.attach(FIRST);
    let early = HostMessage::Start(start_of(1, shell("exit 0"))).encode();
    broker.send_data(&early.encode().unwrap(), OUTER_MAX_PAYLOAD);
    let (_, worked) = joined(working);
    assert!(
        matches!(worked, Err(ServeError::Unexpected(12))),
        "{worked:?}"
    );

    // Once its root is ready, a body that does not decode, a second start, a message that the
    // engine refuses, and a message that only the guest sends each end it.
    let key = Key::new(1, 1);
    let sleeping = HostMessage::Start(start_of(1, shell("exec sleep 60"))).encode();
    let cases = [
        (
            vec![Message::ExecStart(vec![1, 2, 3])],
            Some("an execution's start ends early"),
        ),
        (
            vec![sleeping.clone(), sleeping.clone()],
            Some("execution 1.1 starts twice"),
        ),
        (
            vec![
                sleeping.clone(),
                HostMessage::Credit(key, Pool::Output, 9).encode(),
            ],
            Some("execution 1.1: the credit grants no units or more units than are in flight"),
        ),
        (
            vec![Message::ExecCredit(
                GuestMessage::Credit(key, 1).encode().encode().unwrap()[HEADER_LEN..].to_vec(),
            )],
            Some("the host credits the input of execution 1.1"),
        ),
        (vec![GuestMessage::Started(key).encode()], None),
    ];
    for (messages, problem) in cases {
        let mut host = Host::new();
        let working = host.working.take().unwrap();
        host.broker.send_data(&encode(&messages), OUTER_MAX_PAYLOAD);
        let (_, worked) = joined(working);
        match (problem, &worked) {
            (Some(problem), Err(ServeError::Execution(refused))) => {
                assert!(refused.contains(problem), "{refused}");
            }
            (None, Err(ServeError::Unexpected(17))) => {}
            _ => panic!("{messages:?}: {worked:?}"),
        }
    }
}

#[test]
fn a_reset_or_a_later_generation_abandons_executions_whose_programs_end() {
    // Each program prints the process ID of the group that it leads, then sleeps.
    let mut host = Host::new();
    let sleeper = || shell("echo $$; exec sleep 60");
    host.start(start_of(1, sleeper()), Vec::new());
    host.start(start_of(2, sleeper()), Vec::new());
    host.run_until(|host| {
        [1, 2]
            .iter()
            .all(|id| host.executions[id].output(Stream::Stdout).ends_with(b"\n"))
    });
    let (first, second) = (host.executions[&1].group(), host.executions[&2].group());

    // A start of a later generation of the second execution abandons the earlier one, whose
    // program ends, and runs. A message of the earlier generation that follows it is stale.
    let later = Start::new(
        Key::new(2, 2),
        shared_limits(),
        timers(),
        shell("echo later"),
    );
    let stale = HostMessage::Credit(Key::new(2, 1), Pool::Output, 1).encode();
    host.start_then(later, Vec::new(), &[stale]);
    host.run_until(|host| host.outcome(2).is_some());
    assert_eq!(
        host.executions[&2].outputs(),
        [b"later\n".to_vec(), Vec::new()]
    );
    check_lossless(&host, 2, Status::Exited(0));
    gone(second);

    // A reset abandons every execution: the first one's program ends, and the worker runs the
    // executions of the new epoch.
    host.broker.reset_in_flight(Identity { epoch: 2, ..FIRST });
    let now = Instant::now();
    host.execution(1).peer_mut().reset(now);
    gone(first);
    host.start(start_of(3, shell("echo again")), Vec::new());
    host.run_until(|host| host.outcome(1).is_some() && host.outcome(3).is_some());
    assert_eq!(
        host.executions[&3].outputs(),
        [b"again\n".to_vec(), Vec::new()]
    );
    check_lossless(&host, 3, Status::Exited(0));
    assert_eq!(host.outcome(1).unwrap().cause(), Some(Cause::Reset));
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn the_worker_ends_its_executions_when_the_host_stops_it() {
    let mut host = Host::new();
    host.start(start_of(1, shell("echo $$; exec sleep 60")), Vec::new());
    host.run_until(|host| host.executions[&1].output(Stream::Stdout).ends_with(b"\n"));
    let group = host.executions[&1].group();
    let (root, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
    assert_eq!(root.calls.last(), Some(&Call::Teardown));
    gone(group);
    collected(group);
}

#[test]
fn the_worker_collects_the_status_of_a_program_that_outlives_its_execution() {
    // Without a grace or a settlement time, the runner gives up on a cancelled execution as soon
    // as it kills the program, before the program's status is known.
    let mut host = Host::new();
    let instant = Timers::new(Duration::ZERO, Duration::ZERO);
    let sleeper = shell("echo $$; exec sleep 60");
    host.start(
        Start::new(Key::new(1, 1), shared_limits(), instant, sleeper),
        Vec::new(),
    );
    host.run_until(|host| host.executions[&1].output(Stream::Stdout).ends_with(b"\n"));
    let program = host.executions[&1].group();
    host.execution(1).peer_mut().cancel(Instant::now());
    host.run_until(|host| host.outcome(1).is_some());
    let outcome = host.outcome(1).unwrap();
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    assert_eq!(outcome.status(), None);
    assert!(!outcome.is_reusable());

    // The worker forgot the execution, and still collects the program's status once it ended.
    gone(program);
    collected(program);
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn the_worker_ends_what_is_left_of_the_process_group_of_a_program_that_ended() {
    // The program prints the process ID of the group that it leads, and ends, leaving a
    // descendant in the group that holds its output. The worker ends the descendant once the
    // program ended, before it collects the program's status, so that the output ends at once,
    // losing nothing, rather than once the runner gave up on it.
    let mut host = Host::new();
    let leaving = || shell("echo $$; sleep 60 & exit 3");
    let started = Instant::now();
    host.start(start_of(1, leaving()), Vec::new());
    host.run_until(|host| host.outcome(1).is_some());
    assert!(started.elapsed() < SETTLEMENT, "{:?}", started.elapsed());
    check_lossless(&host, 1, Status::Exited(3));
    let first = host.executions[&1].group();
    gone(first);
    collected(first);

    // A stop once the worker collected the status of such a program leaves no process of the
    // group running either.
    host.start(start_of(2, leaving()), Vec::new());
    host.run_until(|host| host.executions[&2].output(Stream::Stdout).ends_with(b"\n"));
    let second = host.executions[&2].group();
    reaped(second);
    // The fake root lives until the check, as its workload cgroup, if any, ends what is left in it
    // once it drops.
    let (_root, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
    gone(second);
}

/// Returns the program's process ID, or that of another process, from the line `line` of the
/// standard output of the execution `id`, and the directory of the program's cgroup from the next
/// line, which `/proc/self/cgroup` gave.
fn process_and_cgroup(host: &Host, id: u64, line: usize) -> (u32, PathBuf) {
    let output = String::from_utf8(host.executions[&id].output(Stream::Stdout).to_vec()).unwrap();
    let mut lines = output.lines().skip(line);
    let process = lines.next().unwrap().parse().unwrap();
    let cgroup = lines.next().unwrap().strip_prefix("0::/").unwrap();
    (process, Path::new("/sys/fs/cgroup").join(cgroup))
}

#[test]
fn an_execution_s_settlement_ends_a_descendant_that_left_its_process_group() {
    // Only an execution's cgroup reaches a descendant in a session of its own.
    if !cgroups() {
        return;
    }
    // The program starts a descendant in a session of its own, which holds the program's output
    // open once it wrote its process ID, and prints the descendant's process ID and its own
    // cgroup, then ends. The execution's settlement kills the descendant, so that the output ends
    // at once, losing nothing, and the execution settles.
    let mut host = Host::new();
    let script = "exec 3>&1; \
                  descendant=$(setsid sh -c 'echo $$; exec sleep 60 >&3 3>&-' &); \
                  echo \"$descendant\"; cat /proc/self/cgroup; exit 3";
    let started = Instant::now();
    host.start(start_of(1, shell(script)), Vec::new());
    host.run_until(|host| host.outcome(1).is_some());
    assert!(started.elapsed() < SETTLEMENT, "{:?}", started.elapsed());
    check_lossless(&host, 1, Status::Exited(3));
    let (descendant, cgroup) = process_and_cgroup(&host, 1, 0);
    reaped(descendant);
    // The worker removed the execution's cgroup before it sent the outcome, which freed the
    // execution's place: it runs its limit of executions at once again.
    assert!(!cgroup.exists(), "{}", cgroup.display());
    let most = u64::try_from(MAX_EXECUTIONS).unwrap();
    for id in 2..=most + 1 {
        host.start(start_of(id, shell("exec sleep 60")), Vec::new());
    }
    host.run_until(|host| (2..=most + 1).all(|id| host.executions[&id].peer().has_started()));
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

/// A process with a child that it never waits for, so that the child, once it ended, stays a
/// zombie, and occupies its cgroup, until the process ends. Both run in a process group that the
/// process leads, which the test kills when it drops, so that neither outlives a test that fails;
/// the system's collector of orphans then collects the child's status.
struct Holder {
    /// The process.
    process: Child,
    /// The process ID of its child.
    child: u32,
}

impl Holder {
    /// Starts the process and its child.
    fn new() -> Self {
        let mut process = Command::new("/bin/sh")
            .args(["-c", "sleep 600 & echo $!; exec sleep 600"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(process.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let child = line.trim().parse().unwrap();
        Self { process, child }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        // Until its status is collected, the process keeps its ID, which is the group's.
        let group = Pid::from_raw(i32::try_from(self.process.id()).unwrap());
        let _ = signal::killpg(group, Signal::SIGKILL);
        let _ = self.process.wait();
    }
}

#[test]
fn an_execution_that_does_not_settle_keeps_its_place_until_the_host_stops_the_worker() {
    // A zombie whose parent is outside the cgroup keeps a cgroup occupied, which needs cgroups.
    if !cgroups() {
        return;
    }
    // The program prints its process ID and its cgroup, and waits until the test signals it,
    // once the test moved a holder's child into the cgroup. The settlement kills the child, which
    // stays a zombie, so that the execution does not settle within its settlement time.
    let holder = Holder::new();
    let mut host = Host::new();
    let waiting = "trap 'exit 0' USR1; echo $$; cat /proc/self/cgroup; \
                   while :; do sleep 0.01; done";
    let settlement = Duration::from_secs(1);
    let start = Start::new(
        Key::new(1, 1),
        shared_limits(),
        Timers::new(GRACE, settlement),
        shell(waiting),
    );
    host.start(start, Vec::new());
    host.run_until(|host| {
        let output = host.executions[&1].output(Stream::Stdout);
        output.iter().filter(|&&byte| byte == b'\n').count() == 2
    });
    let (program, cgroup) = process_and_cgroup(&host, 1, 0);
    fs::write(cgroup.join("cgroup.procs"), holder.child.to_string()).unwrap();
    let program = Pid::from_raw(i32::try_from(program).unwrap());
    signal::kill(program, Signal::SIGUSR1).unwrap();
    host.run_until(|host| host.outcome(1).is_some());
    let outcome = host.outcome(1).unwrap();
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert_eq!(outcome.cause(), None);
    for stream in Stream::ALL {
        assert!(!outcome.is_truncated(stream), "{stream:?}");
    }
    assert!(!outcome.is_reusable());
    assert!(cgroup.exists(), "{}", cgroup.display());

    // The execution keeps its place: the worker runs one execution fewer than its limit, and
    // refuses the next start. It still does once the cgroup is empty, after the holder ended.
    let most = u64::try_from(MAX_EXECUTIONS).unwrap();
    fill_but_one(&mut host, 2);
    drop(holder);
    let deadline = Instant::now() + TIMEOUT;
    while fs::read(cgroup.join("pids.current")).unwrap() != b"0\n" {
        assert!(Instant::now() < deadline, "the cgroup stays occupied");
        thread::sleep(Duration::from_millis(10));
    }
    fill_but_one(&mut host, 2 + most);

    // The stop ends the executions, and removes every cgroup, the fenced one's included. The fake
    // root lives until the check, as its workload cgroup removes what is left in it once it drops.
    let (_root, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
    assert!(!cgroup.exists(), "{}", cgroup.display());
}

/// Starts one execution fewer than the worker's limit, from the ID `first` on, while another
/// execution keeps its place: checks that they run, and that the worker refuses the next start,
/// then cancels them, and checks that they settled.
fn fill_but_one(host: &mut Host, first: u64) {
    let most = u64::try_from(MAX_EXECUTIONS).unwrap();
    let ids = first..first + most - 1;
    for id in ids.clone() {
        host.start(start_of(id, shell("exec sleep 60")), Vec::new());
    }
    host.run_until(|host| {
        ids.clone()
            .all(|id| host.executions[&id].peer().has_started())
    });
    let refused = ids.end;
    host.start(start_of(refused, shell("exit 0")), Vec::new());
    host.run_until(|host| host.outcome(refused).is_some());
    let again = LaunchFailure::new(0, Errno::EAGAIN as i32);
    assert_eq!(host.outcome(refused).unwrap().launch_failure(), Some(again));
    let now = Instant::now();
    for id in ids.clone() {
        host.execution(id).peer_mut().cancel(now);
    }
    host.run_until(|host| ids.clone().all(|id| host.outcome(id).is_some()));
    for id in ids {
        let outcome = host.outcome(id).unwrap();
        assert_eq!(outcome.cause(), Some(Cause::Cancelled), "{id}");
        assert!(outcome.is_reusable(), "{id}");
    }
}

#[test]
fn an_execution_runs_in_its_working_directory_with_exactly_its_environment() {
    // In a sandbox with a workload identity, the worker prepares each execution's entry as it.
    // Without privileges, the program runs in the test's own root, so that its working directory
    // is one of the test's host. The first program prints its environment, which none of the
    // worker's own joins, and the second its working directory.
    let mut host = Host::new();
    let environment = ["GREETING=hello world", "EMPTY=", "TWICE=a=b"];
    let directory = fs::canonicalize(accounts()).unwrap();
    let directory = directory.to_str().unwrap();
    let printing = start_of(1, vec![b"/usr/bin/env".to_vec()]).with_environment(environment);
    host.start(printing, Vec::new());
    let moved = start_of(2, shell("pwd -P")).with_working_directory(directory);
    host.start(moved, Vec::new());
    host.run_until(|host| (1..=2).all(|id| host.outcome(id).is_some()));
    for id in 1..=2 {
        check_lossless(&host, id, Status::Exited(0));
    }
    let printed = environment.map(|entry| format!("{entry}\n")).concat();
    assert_eq!(
        host.executions[&1].output(Stream::Stdout),
        printed.as_bytes()
    );
    assert_eq!(
        host.executions[&2].output(Stream::Stdout),
        format!("{directory}\n").as_bytes()
    );
    let (root, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
    // The configuration's check, then each execution.
    let workload = entrance(entry::Identity::new(WORKLOAD.0, WORKLOAD.1));
    assert_eq!(
        entrances(&root),
        [workload.clone(), workload.clone(), workload]
    );
}

#[test]
fn an_execution_enters_the_root_as_the_identity_that_its_start_names() {
    // In a sandbox without a workload identity, each start names an identity, with its
    // supplementary groups, its capabilities, and its account mode, which the worker prepares an
    // entry with, and the default hostname.
    let mut host = Host::configured(FakeRoot::booting(&layout()), &RootConfig::default());
    let bit = |capability: Capability| 1 << capability.number();
    let identities = [
        exec::Identity {
            groups: vec![50],
            capabilities: bit(Capability::CHOWN) | bit(Capability::KILL),
            ..exec::Identity::new(0, 0)
        },
        exec::Identity {
            groups: vec![50, WORKLOAD.1],
            ..exec::Identity::new(WORKLOAD.0, WORKLOAD.1)
        },
        exec::Identity {
            account: AccountMode::Numeric,
            ..exec::Identity::new(4242, 4243)
        },
    ];
    for (id, identity) in (1..).zip(&identities) {
        let start = start_of(id, shell("exit 0")).with_identity(identity.clone());
        host.start(start, Vec::new());
    }
    host.run_until(|host| (1..=3).all(|id| host.outcome(id).is_some()));
    for id in 1..=3 {
        check_lossless(&host, id, Status::Exited(0));
    }
    let (root, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
    let capabilities = Capabilities::EMPTY
        .with(Capability::CHOWN)
        .with(Capability::KILL);
    let expected = [
        entry::Identity::new(0, 0)
            .groups([50])
            .capabilities(capabilities),
        entry::Identity::new(WORKLOAD.0, WORKLOAD.1).groups([50, WORKLOAD.1]),
        entry::Identity::new(4242, 4243).account(entry::AccountMode::Numeric),
    ]
    .map(entrance);
    assert_eq!(entrances(&root), expected);
}

#[test]
fn an_execution_enters_the_root_with_the_publication_of_listeners_that_its_configuration_names() {
    // Listeners are published unless the configuration names them unpublished. While they are,
    // the entry refuses at stage 0 the capabilities that could forge a listener announcement,
    // binding a privileged port and opening a raw socket, which it allows otherwise, as it allows
    // the other capabilities of its allowlist either way.
    let capabilities = [
        Capability::NET_BIND_SERVICE,
        Capability::NET_RAW,
        Capability::CHOWN,
    ];
    for (listeners, publication) in [
        (None, Publication::Published),
        (Some(Listeners::Published), Publication::Published),
        (Some(Listeners::Unpublished), Publication::Unpublished),
    ] {
        let config = RootConfig {
            listeners,
            ..RootConfig::default()
        };
        let mut host = Host::configured(FakeRoot::booting(&layout()), &config);
        for (id, capability) in (1..).zip(capabilities) {
            let identity = exec::Identity {
                capabilities: 1 << capability.number(),
                ..exec::Identity::new(0, 0)
            };
            host.start(
                start_of(id, shell("exit 0")).with_identity(identity),
                Vec::new(),
            );
        }
        host.run_until(|host| (1..=3).all(|id| host.outcome(id).is_some()));
        for (id, capability) in (1..).zip(capabilities) {
            if publication == Publication::Published && capability != Capability::CHOWN {
                let failure = LaunchFailure::new(0, Errno::EPERM as i32);
                let outcome = host.outcome(id).unwrap();
                assert_eq!(
                    outcome.launch_failure(),
                    Some(failure),
                    "{listeners:?}: {capability}"
                );
            } else {
                check_lossless(&host, id, Status::Exited(0));
            }
        }
        let (root, worked) = host.stop();
        assert_eq!(worked.unwrap(), Worked::Stopped);
        let expected = capabilities.map(|capability| Entrance {
            publication,
            ..entrance(
                entry::Identity::new(0, 0).capabilities(Capabilities::EMPTY.with(capability)),
            )
        });
        assert_eq!(entrances(&root), expected, "{listeners:?}");
    }

    // The check of a sandbox's workload identity enters with the configuration's publication, as
    // each execution does.
    let config = RootConfig {
        listeners: Some(Listeners::Unpublished),
        ..sandbox()
    };
    let mut host = Host::configured(FakeRoot::booting(&layout()), &config);
    host.start(start_of(1, shell("exit 0")), Vec::new());
    host.run_until(|host| host.outcome(1).is_some());
    check_lossless(&host, 1, Status::Exited(0));
    let (root, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
    let workload = Entrance {
        publication: Publication::Unpublished,
        ..entrance(entry::Identity::new(WORKLOAD.0, WORKLOAD.1))
    };
    assert_eq!(entrances(&root), [workload.clone(), workload]);
}

#[test]
fn the_worker_refuses_at_stage_0_a_start_whose_identity_breaks_the_rules() {
    // Each refusal's terminal record tells its `errno`, and the refusal takes no place.
    fn refused(host: &mut Host, cases: Vec<(Option<exec::Identity>, Errno)>) {
        let ids = 1..=u64::try_from(cases.len()).unwrap();
        for (id, (identity, _)) in ids.clone().zip(&cases) {
            let mut start = start_of(id, shell("exit 0"));
            start.identity = identity.clone();
            host.start(start, Vec::new());
        }
        host.run_until(|host| ids.clone().all(|id| host.outcome(id).is_some()));
        for (id, (identity, error)) in ids.zip(&cases) {
            let failure = LaunchFailure::new(0, *error as i32);
            let outcome = host.outcome(id).unwrap();
            assert_eq!(outcome.launch_failure(), Some(failure), "{identity:?}");
        }
    }

    // In a sandbox with a workload identity, a start names none.
    let mut host = Host::new();
    let workload = exec::Identity::new(WORKLOAD.0, WORKLOAD.1);
    refused(&mut host, vec![(Some(workload), Errno::EINVAL)]);
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);

    // In a sandbox without one, a start names its own, which the entry checks: its
    // capabilities, its IDs, and its account in existing-account mode.
    let mut host = Host::configured(FakeRoot::booting(&layout()), &RootConfig::default());
    let root = |capability: Capability| exec::Identity {
        capabilities: 1 << capability.number(),
        ..exec::Identity::new(0, 0)
    };
    let cases = vec![
        (None, Errno::EINVAL),
        (
            Some(exec::Identity {
                capabilities: 1 << 21,
                ..exec::Identity::new(0, 0)
            }),
            Errno::EPERM,
        ),
        (
            Some(exec::Identity {
                capabilities: 1 << Capability::CHOWN.number(),
                ..exec::Identity::new(WORKLOAD.0, WORKLOAD.1)
            }),
            Errno::EPERM,
        ),
        (Some(root(Capability::NET_BIND_SERVICE)), Errno::EPERM),
        (Some(root(Capability::NET_RAW)), Errno::EPERM),
        (Some(exec::Identity::new(4242, 4242)), Errno::ENOENT),
        (Some(exec::Identity::new(ORPHAN.0, ORPHAN.1)), Errno::ENOENT),
        (Some(exec::Identity::new(WORKLOAD.0, 50)), Errno::EINVAL),
        (
            Some(exec::Identity {
                account: AccountMode::Numeric,
                ..exec::Identity::new(u32::MAX, 0)
            }),
            Errno::EINVAL,
        ),
    ];
    let first = u64::try_from(cases.len()).unwrap() + 1;
    refused(&mut host, cases);
    let most = u64::try_from(MAX_EXECUTIONS).unwrap();
    let ids = first..first + most;
    for id in ids.clone() {
        let start = start_of(id, shell("exec sleep 60"))
            .with_identity(exec::Identity::new(WORKLOAD.0, WORKLOAD.1));
        host.start(start, Vec::new());
    }
    host.run_until(|host| {
        ids.clone()
            .all(|id| host.executions[&id].peer().has_started())
    });
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn the_worker_collects_the_status_of_a_program_that_a_real_time_signal_ended() {
    // `nix` decodes no real-time signal in a status, so that the worker learns that the program
    // ended from its pidfd, and leaves the status to the program's handle.
    let mut host = Host::new();
    host.start(start_of(1, shell("kill -40 $$")), Vec::new());
    host.run_until(|host| host.outcome(1).is_some());
    check_lossless(&host, 1, Status::Signaled(40));
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn an_execution_that_starts_with_the_root_counts_its_timeout_from_its_own_start() {
    // The execution's start comes in the same payload as the root's configuration and the
    // sandbox's start, and the root's assembly takes longer than the execution's timeout. The
    // timeout counts from the execution's own instant, so the program, which ends well before it,
    // ends on its own.
    let root = FakeRoot::booting(&layout()).assembling_for(Duration::from_secs(2));
    let (mut broker, working) = start_worker(root);
    broker.attach(FIRST);
    let timed = timers().timeout(Duration::from_secs(1));
    let start = Start::new(
        Key::new(1, 1),
        shared_limits(),
        timed,
        shell("exec sleep 0.2"),
    );
    let execution = HostExecution::new(start.clone(), timers(), Vec::new(), Instant::now());
    let mut host = Host {
        broker,
        working: Some(working),
        executions: BTreeMap::from([(1, execution)]),
        replies: Vec::new(),
        stopping: false,
    };
    host.send(&[
        Message::RootConfig(sandbox().encode()),
        Message::Start,
        HostMessage::Start(start).encode(),
    ]);
    host.run_until(|host| host.outcome(1).is_some());
    assert_eq!(host.replies, [ready(), Message::Started]);
    check_lossless(&host, 1, Status::Exited(0));
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

#[test]
fn a_host_that_a_test_drops_while_its_broker_pauses_ends_the_worker_and_its_programs() {
    // A test that fails while the broker pauses drops its host, which ends the worker's session,
    // so that neither the worker nor its programs outlive the test.
    let mut host = Host::new();
    host.start(
        start_of(1, shell("echo $$; exec cat /dev/zero")),
        Vec::new(),
    );
    host.run_until(|host| host.executions[&1].output(Stream::Stdout).contains(&b'\n'));
    let group = host.executions[&1].group();
    host.broker.pause();
    let working = host.working.take().unwrap();
    drop(host);
    let deadline = Instant::now() + TIMEOUT;
    while !working.is_finished() {
        assert!(Instant::now() < deadline, "the worker runs on");
        thread::sleep(Duration::from_millis(10));
    }
    let _ = working.join();
    gone(group);
}

#[test]
fn the_worker_remembers_the_generations_that_the_host_started() {
    // The worker would refuse a start of the wide limits at once, as it would hold too much, so
    // a start that it took for a new one would answer with a terminal record before the pong.
    let wide = Limits::new(MAX_CHUNK, Window::frames(4), Window::frames(4)).unwrap();
    let start_of_generation =
        |generation, limits| Start::new(Key::new(1, generation), limits, timers(), shell("exit 0"));
    let mut host = Host::new();
    host.start(start_of_generation(2, shared_limits()), Vec::new());
    host.run_until(|host| host.outcome(1).is_some());
    check_lossless(&host, 1, Status::Exited(0));

    // A start of an earlier generation is stale once the later one ended.
    let stale = HostMessage::Start(start_of_generation(1, wide)).encode();
    let replies = host
        .broker
        .exchange(&[stale, Message::Ping], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [Message::Pong]);

    // And once the worker refused a later one.
    host.start(start_of_generation(3, wide), Vec::new());
    host.run_until(|host| host.outcome(1).is_some());
    let memory = LaunchFailure::new(0, Errno::ENOMEM as i32);
    assert_eq!(host.outcome(1).unwrap().launch_failure(), Some(memory));
    let stale = HostMessage::Start(start_of_generation(2, wide)).encode();
    let replies = host
        .broker
        .exchange(&[stale, Message::Ping], OUTER_MAX_PAYLOAD, 1);
    assert_eq!(replies, [Message::Pong]);

    // A second start of a generation that the worker refused ends the session, as does one of
    // a generation that ended.
    let refused = (
        host,
        start_of_generation(3, wide),
        "execution 1.3 starts twice",
    );
    let mut ended = Host::new();
    ended.start(start_of_generation(2, shared_limits()), Vec::new());
    ended.run_until(|host| host.outcome(1).is_some());
    let ended = (
        ended,
        start_of_generation(2, shared_limits()),
        "execution 1.2 starts twice",
    );
    for (mut host, again, problem) in [refused, ended] {
        let working = host.working.take().unwrap();
        let again = HostMessage::Start(again).encode();
        host.broker.send_data(&encode(&[again]), OUTER_MAX_PAYLOAD);
        let (_, worked) = joined(working);
        assert!(
            matches!(&worked, Err(ServeError::Execution(refused)) if refused.contains(problem)),
            "{problem}: {worked:?}"
        );
    }
}

#[test]
fn the_worker_runs_executions_on_a_tty() {
    let (broker, terminal) = terminal();
    let path = terminal.path.clone();
    let working = thread::spawn(move || {
        let mut root = FakeRoot::booting(&layout());
        let worked = worker::work_on_terminal(&path, &mut root);
        (root, worked)
    });
    let mut host = Host::ready(broker, working, &sandbox());
    let script = "seq 1 100000; seq 1 1000 >&2";
    host.start(start_of(1, shell(script)), Vec::new());
    host.run_until(|host| host.outcome(1).is_some());
    assert_eq!(
        host.executions[&1].outputs(),
        [lines(1, 100_000), lines(1, 1000)]
    );
    check_lossless(&host, 1, Status::Exited(0));
    let (_, worked) = host.stop();
    assert_eq!(worked.unwrap(), Worked::Stopped);
}

/// The worker of a clone, which a restore of a template's snapshot started, on x86_64, whose time
/// ABI a clone needs. It resumes the session that the template's VM attached before its capture,
/// answers the reset that the restore queued, takes nothing but the host's `Activate` until the
/// clone is ready, applies its record, and then serves the host's order, as after a cold boot.
#[cfg(target_arch = "x86_64")]
mod clones {
    use std::net::{IpAddr, Ipv4Addr};
    use std::num::NonZeroU64;

    use nvx::activation::{
        self, ActivationRecord, Configuration, ExpectedResources, MachineIdMode, ReadyActivation,
        RestoreSection, StoreGeneration, TemplateReference, TransactionId, WorkloadIdentity,
        WorkloadView,
    };
    use nvx::guest::clone::{Activation, AdmissionFence, RootTemplate};
    use nvx::protocol::nvxs::{GuestEvent, GuestSession};
    use nvx::protocol::status;
    use runtime_adapter::frame::MAX_ACTIVATE_BODY;
    use runtime_adapter::worker::UNACTIVATED_DRAIN;
    use runtime_adapter::worker::activation::{ACTIVATION_PROBE, ACTIVATION_TIMEOUT};
    use runtime_adapter::worker::capture::{self, Admission};

    use super::*;

    /// Identity of the epoch that the broker of the clones' template started with the reset with
    /// which it answered the template's attachment, before the capture.
    const TEMPLATE: Identity = Identity {
        instance_id: [0x7e; 16],
        epoch: 1,
    };

    /// Identity of the epoch that the broker of a clone starts with the reset that the restore
    /// queued, of another instance.
    const RESTORED: Identity = Identity {
        instance_id: [0xc1; 16],
        epoch: 1,
    };

    /// The CPU count and the memory size of the tests' clones, as their repair found them.
    const RESOURCES: ExpectedResources = ExpectedResources {
        cpus: 2,
        memory: 1 << 30,
    };

    /// The IPv4 gateway of the NIC of the clones' template.
    const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

    /// Returns the record of a clone's Activate of the transaction `transaction`, whose workload
    /// runs as `identity`, if any: an EROFS image over a scratch, without an export, a mapping, or
    /// a network, which expects [`RESOURCES`].
    fn record(transaction: u8, identity: Option<(u32, u32)>) -> ActivationRecord {
        let configuration = Configuration {
            image: activation::Image::Erofs {
                binding: activation::Identity::from_bytes([0x11; 32]),
                uuid: None,
            },
            upper: activation::Upper::Scratch {
                binding: activation::Identity::from_bytes([0x22; 32]),
                journal: activation::Journal::Forbidden,
            },
            overlay: activation::OverlayMode::UntrustedLowers,
            export: None,
            mappings: 0,
            network: None,
            workload: WorkloadView {
                hostname: b"clone".to_vec(),
                machine_id: MachineIdMode::Image,
                identity: identity.map(|(uid, gid)| WorkloadIdentity { uid, gid }),
            },
            resources: RESOURCES,
        };
        let restore = RestoreSection {
            transaction: TransactionId::from_bytes([transaction; 16]),
            template: TemplateReference {
                generation: StoreGeneration::new(NonZeroU64::MIN),
                key: activation::Identity::from_bytes([0x44; 32]),
            },
        };
        ActivationRecord::new(Some(restore), configuration).unwrap()
    }

    /// Returns the record of `record`'s transaction whose configuration `change` changed.
    fn changed(
        record: &ActivationRecord,
        change: impl FnOnce(&mut Configuration),
    ) -> ActivationRecord {
        let mut configuration = record.configuration().clone();
        change(&mut configuration);
        ActivationRecord::new(record.restore().cloned(), configuration).unwrap()
    }

    /// Returns the record of `record`'s transaction with an aggregate export, whose mapping table
    /// holds `mappings` mappings.
    fn exported(record: &ActivationRecord, mappings: usize) -> ActivationRecord {
        changed(record, |configuration| {
            configuration.export = Some(activation::Export {
                kind: activation::ExportKind::Aggregate,
                plan: activation::Identity::from_bytes([0x33; 32]),
            });
            configuration.mappings = mappings;
        })
    }

    /// Returns the record of `record`'s transaction with a network whose DNS server is
    /// `10.0.0.3`, which publishes no listener, and whose proxy's endpoint is `proxy`, if any.
    fn networked(record: &ActivationRecord, proxy: Option<&str>) -> ActivationRecord {
        changed(record, |configuration| {
            configuration.network = Some(activation::Network {
                policy: activation::Identity::from_bytes([0x55; 32]),
                dns: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3))),
                publication: activation::Publication::Unpublished,
                proxy: proxy.map(str::to_owned),
            });
        })
    }

    /// Returns the record of `record`'s transaction with the network of [`networked`], without a
    /// proxy, but which publishes the workload's listeners.
    fn published(record: &ActivationRecord) -> ActivationRecord {
        changed(&networked(record, None), |configuration| {
            let network = configuration.network.as_mut().unwrap();
            network.publication = activation::Publication::Published;
        })
    }

    /// Returns the activation of a clone that the worker repaired, with [`RESOURCES`], behind the
    /// worker's fence, which the capture's barrier handed over closed, and whose deadline ends
    /// `timeout` after `acknowledged`, as from the repair's acknowledgement of the restore.
    fn repaired(acknowledged: Instant, timeout: Duration) -> Activation<Admission> {
        let mut fence = Admission::default();
        fence.close().unwrap();
        Activation::with_resources(RESOURCES, Some(acknowledged), fence, timeout, acknowledged)
    }

    /// Returns the inputs of the activation of the clones that their template captured: those of
    /// the template's own configuration, and the gateway of its NIC, [`GATEWAY`].
    fn template() -> RootTemplate {
        capture::root_template(&capture::template_config(), Some(GATEWAY))
    }

    /// Returns the guest's role of the session that the template's VM attached before its
    /// capture, as the snapshot holds it: active in the epoch of [`TEMPLATE`], which the
    /// template's broker started with the reset with which it answered the attachment, and which
    /// the template acknowledged.
    fn captured_session() -> GuestSession {
        let mut session = GuestSession::new();
        let mut output = Vec::new();
        session.attach(&mut output).unwrap();
        let reset = super::record(RecordType::Reset, TEMPLATE, 0, &[]);
        let (consumed, event) = session.receive(&reset).unwrap();
        assert_eq!(
            (consumed, event),
            (reset.len(), Some(GuestEvent::Reset(TEMPLATE)))
        );
        session.acknowledge(RECEIVE_WINDOW, &mut output).unwrap();
        session
    }

    /// Starts a clone's worker in a thread, with the root operations `root`, on one end of a
    /// socket pair, and returns the broker on the other end with the thread. The worker resumes
    /// the session that the template's VM attached, so it sends no attachment, and its
    /// activation's deadline ends `timeout` from now.
    fn start_clone(root: FakeRoot, timeout: Duration) -> (Broker, Working) {
        start_clone_after(root, timeout, |_| {})
    }

    /// Starts a clone's worker as [`start_clone`] does, once `ahead` sent what the broker sends
    /// before the worker runs, which the worker then takes at once, however late its thread runs.
    fn start_clone_after(
        root: FakeRoot,
        timeout: Duration,
        ahead: impl FnOnce(&mut Broker),
    ) -> (Broker, Working) {
        let (broker, guest) = UnixStream::pair().unwrap();
        let mut broker = Broker::new(broker);
        ahead(&mut broker);
        (broker, run_clone(guest, root, timeout))
    }

    /// Runs a clone's worker in a thread, with the root operations `root`, on `guest`, the guest's
    /// end of the stream, and returns the thread, as [`start_clone`] does. The clone's repair
    /// acknowledged the restore now.
    fn run_clone(guest: UnixStream, root: FakeRoot, timeout: Duration) -> Working {
        let session = captured_session();
        let acknowledged = Instant::now();
        let activation = repaired(acknowledged, timeout);
        thread::spawn(move || {
            let mut root = root;
            let worked = worker::work_clone(
                guest,
                session,
                &mut root,
                activation,
                template(),
                Some(acknowledged),
            );
            (root, worked)
        })
    }

    /// Returns the broker's end and the guest's end of a socket pair whose guest direction is
    /// full, as once the broker failed: the stream takes nothing more from the guest.
    fn stalled() -> (UnixStream, UnixStream) {
        let (broker, guest) = UnixStream::pair().unwrap();
        let filler = guest.try_clone().unwrap();
        filler.set_nonblocking(true).unwrap();
        loop {
            match (&filler).write(&[0; 4096]) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("cannot fill the stream: {error}"),
            }
        }
        (broker, guest)
    }

    impl Broker {
        /// Starts the epoch of `identity` with a RESET, as [`reset`](Self::reset) does, but takes
        /// the guest's acknowledgement later, with [`acknowledged`](Self::acknowledged), as from a
        /// guest that may not run yet: meanwhile, the broker takes the guest's receive window as
        /// the guest agent's, which the acknowledgement grants.
        fn reset_ahead(&mut self, identity: Identity) {
            self.identity = identity;
            self.send_sequence = 0;
            self.receive_sequence = 0;
            self.send(RecordType::Reset, &[]);
            self.room = RECEIVE_WINDOW as usize;
        }

        /// Takes the guest's acknowledgement of the reset that [`reset_ahead`](Self::reset_ahead)
        /// sent.
        fn acknowledged(&mut self) {
            let ack = self.expect();
            assert_eq!(ack.record_type(), RecordType::Ack);
            assert_eq!(ack.window(), Some(RECEIVE_WINDOW));
        }
    }

    /// Returns the host's Activate of `record`.
    fn activate(record: &ActivationRecord) -> Message {
        Message::Activate(record.encode())
    }

    /// Returns the worker's answer to the Activate of `record` once it activated the clone: its
    /// readiness, with the guest agent's build, and the activation, which reports that it applied,
    /// with the record's transaction.
    fn activated(record: &ActivationRecord) -> Message {
        let ready = Ready::new(GUEST_BUILD).unwrap();
        Message::Ready(ready.with_activation(ReadyActivation::of(record)).encode())
    }

    /// Returns the call of a fake root that applied `record` with the clones' [`template`].
    fn applied(record: &ActivationRecord) -> Call {
        Call::Apply(Box::new((record.clone(), template())))
    }

    /// Returns the root operations of the worker that runs in `working`, once it ended because the
    /// clone's activation failed with the status `expected`, and checks that it answered nothing,
    /// but credited what it took, before it ended its stream.
    fn unactivated(broker: &mut Broker, working: Working, expected: u8) -> FakeRoot {
        let (root, worked) = joined(working);
        let worked = worked.unwrap();
        assert!(matches!(worked, Worked::Unactivated { .. }), "{worked:?}");
        assert_eq!(worked.status(), expected, "{worked:?}");
        while let Some(record) = broker.read() {
            broker.check(&record);
            assert_eq!(broker.accept(&record), [], "{worked:?}");
        }
        root
    }

    impl Host {
        /// Starts a clone's worker with the root operations `root`, and returns the host, once the
        /// worker activated the clone with `record`, which announces no mapping, and started the
        /// sandbox. The host waits for Ready before it starts the sandbox.
        fn activated(root: FakeRoot, record: &ActivationRecord) -> Self {
            Self::mapped(root, record, &[])
        }

        /// Starts a clone's worker with the root operations `root`, and returns the host, once the
        /// worker activated the clone with `record`, applied `parts`, the parts of the mapping
        /// table that the record announces, in order, and started the sandbox.
        fn mapped(root: FakeRoot, record: &ActivationRecord, parts: &[Vec<u8>]) -> Self {
            let (mut broker, working) = start_clone(root, ACTIVATION_TIMEOUT);
            broker.reset(RESTORED);
            let replies = broker.exchange(&[activate(record)], OUTER_MAX_PAYLOAD, 1);
            assert_eq!(replies, [activated(record)]);
            let mut messages = parts.iter().cloned().map(Message::Maps).collect::<Vec<_>>();
            messages.push(Message::Start);
            let mut expected = parts.iter().map(|_| Message::Mapped).collect::<Vec<_>>();
            expected.push(Message::Started);
            let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, expected.len());
            assert_eq!(replies, expected);
            Self {
                broker,
                working: Some(working),
                executions: BTreeMap::new(),
                replies: Vec::new(),
                stopping: false,
            }
        }
    }

    #[test]
    fn a_clone_answers_the_restore_s_reset_and_activates_with_the_record_of_its_activate() {
        let (mut broker, working) = start_clone(FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
        // The worker sends no attachment: it acknowledges the reset that the restore queued, with
        // its receive window, first.
        broker.reset(RESTORED);
        let record = record(1, Some(WORKLOAD));
        let replies = broker.exchange(&[activate(&record)], OUTER_MAX_PAYLOAD, 1);
        assert_eq!(replies, [activated(&record)]);
        let Message::Ready(body) = &replies[0] else {
            panic!("{replies:?}");
        };
        let ready = Ready::decode(body).unwrap();
        assert_eq!(ready.build(), GUEST_BUILD);
        assert_eq!(ready.activation(), Some(ReadyActivation::of(&record)));

        // A retry of the Activate, byte for byte, gets the same Ready, without a second
        // application, in records of any size, and after a later reset, as once the host attached
        // again.
        let replies = broker.exchange(&[activate(&record)], 7, 1);
        assert_eq!(replies, [activated(&record)]);
        broker.settle();
        broker.reset(Identity {
            epoch: 2,
            ..RESTORED
        });
        let replies = broker.exchange(&[activate(&record)], OUTER_MAX_PAYLOAD, 1);
        assert_eq!(replies, [activated(&record)]);

        // The worker then serves the host's order, as after a cold boot: the record announces no
        // mapping, so the sandbox starts at once, and a retry still gets the same Ready.
        let messages = [
            Message::Ping,
            Message::Start,
            Message::Probe(b"mounts".to_vec()),
            activate(&record),
            Message::Stop,
        ];
        let expected = [
            Message::Pong,
            Message::Started,
            Message::Report(found(&probe("mounts")).encode()),
            activated(&record),
            Message::Stopped,
        ];
        let replies = broker.exchange(&messages, OUTER_MAX_PAYLOAD, expected.len());
        assert_eq!(replies, expected);
        let (root, worked) = joined(working);
        let worked = worked.unwrap();
        assert_eq!((&worked, worked.status()), (&Worked::Stopped, 0));
        assert!(broker.read().is_none(), "the worker ended its stream");
        // It applied the record once, with the template's inputs, and read no command line,
        // bootstrapped nothing, and assembled no root of a configuration: the template
        // bootstrapped the guest before its capture.
        let expected = [
            applied(&record),
            Call::Probe(probe("mounts")),
            Call::Teardown,
        ];
        assert_eq!(root.calls, expected);
    }

    #[test]
    fn a_clone_takes_nothing_but_its_activate_before_it_is_ready() {
        // Any other message fails the activation, finally, with the status of the host's session,
        // a stop and a ping included, as do the messages that only the guest sends. The worker
        // answers nothing, and applies, mounts, and spawns nothing.
        let refused = [
            Message::Ping,
            Message::Stop,
            Message::Echo(b"early".to_vec()),
            Message::Probe(b"mounts".to_vec()),
            configuration(),
            Message::Maps(maps_part()),
            Message::Start,
            HostMessage::Start(start_of(1, shell("exit 0"))).encode(),
            Message::Pong,
            ready(),
            Message::Started,
        ];
        for message in refused {
            let (mut broker, working) =
                start_clone(FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
            broker.reset(RESTORED);
            broker.send_data(&message.encode().unwrap(), OUTER_MAX_PAYLOAD);
            let root = unactivated(&mut broker, working, status::ACTIVATION_PROTOCOL);
            assert_eq!(root.calls, [], "{message:?}");
        }

        // So does another reset, which the worker acknowledges first.
        let (mut broker, working) = start_clone(FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
        broker.reset(RESTORED);
        broker.reset(Identity {
            epoch: 2,
            ..RESTORED
        });
        let root = unactivated(&mut broker, working, status::ACTIVATION_PROTOCOL);
        assert_eq!(root.calls, []);
    }

    #[test]
    fn a_clone_s_activation_fails_finally_with_the_status_of_the_failure_s_class() {
        // The activation refuses a record that does not decode, a cold boot's, and one that
        // expects other resources than the repair found, before anything has a side effect.
        let clone = record(1, None);
        let cold = ActivationRecord::new(None, clone.configuration().clone()).unwrap();
        let more = changed(&clone, |configuration| configuration.resources.memory *= 2);
        let cases = [
            (Message::Activate(Vec::new()), status::ACTIVATION_RECORD),
            (
                Message::Activate(b"\x01".to_vec()),
                status::ACTIVATION_RECORD,
            ),
            (activate(&cold), status::ACTIVATION_RECORD),
            (activate(&more), status::ACTIVATION_RESOURCES),
        ];
        for (message, expected) in cases {
            let (mut broker, working) =
                start_clone(FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
            broker.reset(RESTORED);
            broker.send_data(&message.encode().unwrap(), OUTER_MAX_PAYLOAD);
            let root = unactivated(&mut broker, working, expected);
            assert_eq!(root.calls, [], "{message:?}");
        }

        // The application fails once the kernel refused to reread a GPT disk's partition table,
        // and refuses a workload identity whose account the image lacks, or whose primary group
        // it lacks, a machine ID that it cannot generate, and a hostname or machine ID that it
        // cannot set for every execution, an export that it cannot mount, a resolver file that it
        // cannot write, and an announcer of the workload's listeners that it cannot start, before
        // Ready.
        let gpt = changed(&clone, |configuration| {
            configuration.image = activation::Image::Gpt {
                binding: activation::Identity::from_bytes([0x11; 32]),
                disk_guid: None,
                first_partition_bound: 1 << 20,
            };
        });
        let generated = changed(&clone, |configuration| {
            configuration.workload.machine_id = MachineIdMode::Generated;
        });
        let cases = [
            (
                gpt,
                FakeRoot::failing(Operation::Reread, "the disk is busy"),
                status::ACTIVATION_ROOT,
            ),
            (
                record(1, Some((4242, 4242))),
                FakeRoot::booting(&layout()),
                status::ACTIVATION_WORKLOAD,
            ),
            (
                record(1, Some(ORPHAN)),
                FakeRoot::booting(&layout()),
                status::ACTIVATION_WORKLOAD,
            ),
            (
                generated.clone(),
                FakeRoot::failing(Operation::MachineId, "a machine ID exists already"),
                status::ACTIVATION_WORKLOAD,
            ),
            (
                generated,
                FakeRoot::failing(Operation::View, "the image's machine ID is a directory"),
                status::ACTIVATION_WORKLOAD,
            ),
            (
                exported(&clone, 1),
                FakeRoot::failing(Operation::MountExport, "the export's device is missing"),
                status::ACTIVATION_EXPORT,
            ),
            (
                published(&clone),
                FakeRoot::failing(Operation::Announce, "no privileged port is free"),
                status::ACTIVATION_NETWORK,
            ),
            (
                networked(&clone, None),
                FakeRoot::failing(Operation::Resolver, "a resolver file exists already"),
                status::ACTIVATION_NETWORK,
            ),
        ];
        for (record, root, expected) in cases {
            let (mut broker, working) = start_clone(root, ACTIVATION_TIMEOUT);
            broker.reset(RESTORED);
            broker.send_data(&activate(&record).encode().unwrap(), OUTER_MAX_PAYLOAD);
            let root = unactivated(&mut broker, working, expected);
            assert_eq!(root.calls, [applied(&record)]);
        }
    }

    #[test]
    fn a_clone_fails_on_an_activate_longer_than_a_record_before_it_takes_its_body() {
        // The framing refuses the header of an Activate that announces more bytes than a record
        // can have before it takes any byte of its body, and the activation takes it as a record
        // that long: it fails with the record's status, whether the clone is ready or not, and the
        // worker answers nothing more.
        let accepted = record(1, Some(WORKLOAD));
        let kind = activate(&accepted).kind();
        for length in [MAX_ACTIVATE_BODY + 1, usize::try_from(u32::MAX).unwrap()] {
            let mut header = u32::try_from(length).unwrap().to_le_bytes().to_vec();
            header.push(kind);
            for ready in [false, true] {
                let (mut broker, working) =
                    start_clone(FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
                broker.reset(RESTORED);
                let mut calls = Vec::new();
                if ready {
                    let replies = broker.exchange(&[activate(&accepted)], OUTER_MAX_PAYLOAD, 1);
                    assert_eq!(replies, [activated(&accepted)]);
                    calls.push(applied(&accepted));
                }
                broker.send_data(&header, OUTER_MAX_PAYLOAD);
                let (root, worked) = joined(working);
                let worked = worked.unwrap();
                let Worked::Unactivated {
                    status: code,
                    problem,
                } = &worked
                else {
                    panic!("{length}, ready {ready}: {worked:?}");
                };
                assert_eq!(*code, status::ACTIVATION_RECORD, "{problem}");
                assert!(problem.contains(&format!("{length} bytes")), "{problem}");
                while let Some(record) = broker.read() {
                    broker.check(&record);
                    assert_eq!(broker.accept(&record), [], "{problem}");
                }
                assert_eq!(root.calls, calls, "{problem}");
            }
        }
    }

    #[test]
    fn a_clone_that_the_host_does_not_activate_in_time_ends_with_the_deadline_s_status() {
        // Whether the restore's reset came or not, or part of an Activate, the deadline ends the
        // activation, before anything has a side effect. The broker sends what it sends before the
        // worker runs, which then takes it at once: a worker that runs only once the deadline
        // passed still acknowledges the reset, and fails with the deadline's status.
        let record = record(1, Some(WORKLOAD));
        let bytes = activate(&record).encode().unwrap();
        let cases: [Option<&[u8]>; 3] = [None, Some(&[]), Some(&bytes[..bytes.len() / 2])];
        for sent in cases {
            let deadline = Duration::from_millis(200);
            let root = FakeRoot::booting(&layout());
            let (mut broker, working) = start_clone_after(root, deadline, |broker| {
                if let Some(sent) = sent {
                    broker.reset_ahead(RESTORED);
                    broker.send_data(sent, OUTER_MAX_PAYLOAD);
                }
            });
            if sent.is_some() {
                broker.acknowledged();
            }
            let root = unactivated(&mut broker, working, status::ACTIVATION_DEADLINE);
            assert_eq!(root.calls, [], "{sent:?}");
        }

        // Once the clone is ready, the deadline ends nothing.
        let deadline = Duration::from_secs(2);
        let root = FakeRoot::booting(&layout());
        let (mut broker, working) = start_clone_after(root, deadline, |broker| {
            broker.reset_ahead(RESTORED);
            broker.send_data(&bytes, OUTER_MAX_PAYLOAD);
        });
        broker.acknowledged();
        let mut replies = Vec::new();
        while replies.is_empty() {
            replies.extend(broker.take());
        }
        assert_eq!(replies, [activated(&record)]);
        thread::sleep(deadline + Duration::from_millis(500));
        let replies = broker.exchange(&[Message::Ping, Message::Stop], OUTER_MAX_PAYLOAD, 2);
        assert_eq!(replies, [Message::Pong, Message::Stopped]);
        let (_, worked) = joined(working);
        assert_eq!(worked.unwrap(), Worked::Stopped);
    }

    #[test]
    fn a_clone_s_application_ends_by_the_activation_s_deadline() {
        // The worker hands the application the activation's deadline, until which it waits for
        // the devices at the latest: an Activate whose application would outlast the deadline,
        // such as one whose device never appears, ends the clone with the deadline's status once
        // the deadline passed, rather than once the application's waits would have ended, with
        // no Ready.
        let timeout = Duration::from_secs(2);
        let slow = Duration::from_secs(60);
        let root = FakeRoot::booting(&layout()).assembling_for(slow);
        let started = Instant::now();
        let (mut broker, working) = start_clone(root, timeout);
        broker.reset(RESTORED);
        let record = record(1, Some(WORKLOAD));
        broker.send_data(&activate(&record).encode().unwrap(), OUTER_MAX_PAYLOAD);
        let root = unactivated(&mut broker, working, status::ACTIVATION_DEADLINE);
        let ended = Instant::now();
        assert!(ended - started < slow, "{:?}", ended - started);
        assert_eq!(root.calls, [applied(&record)]);
        // It is the activation's deadline: `timeout` after the repair's acknowledgement of the
        // restore, which the test's clone made once the test started.
        let [Some(deadline)] = root.deadlines[..] else {
            panic!("{:?}", root.deadlines);
        };
        assert!(deadline >= started + timeout, "{deadline:?}");
        assert!(deadline <= ended + timeout, "{deadline:?}");
    }

    #[test]
    fn a_clone_takes_what_came_during_its_application_before_it_is_ready() {
        // The application runs until the test opens the gate, by when the host sent more after
        // its Activate, before Ready, which the worker takes before it completes the activation:
        // another reset, which the worker acknowledges, or a request fails the activation, with
        // the status of the host's session, after one application, and with no Ready.
        let record = record(1, Some(WORKLOAD));
        let bytes = activate(&record).encode().unwrap();
        for reset in [true, false] {
            let (root, gate) = FakeRoot::booting(&layout()).gated();
            let (mut broker, working) = start_clone(root, ACTIVATION_TIMEOUT);
            broker.reset(RESTORED);
            broker.send_data(&bytes, OUTER_MAX_PAYLOAD);
            if reset {
                broker.reset_ahead(Identity {
                    epoch: 2,
                    ..RESTORED
                });
            } else {
                broker.send_data(&Message::Ping.encode().unwrap(), OUTER_MAX_PAYLOAD);
            }
            gate.send(()).unwrap();
            if reset {
                broker.acknowledged();
            }
            let root = unactivated(&mut broker, working, status::ACTIVATION_PROTOCOL);
            assert_eq!(root.calls, [applied(&record)], "reset {reset}");
        }

        // A retry of the Activate, byte for byte, joins the application: the worker answers both
        // with Ready, after one application, then serves the host's order.
        let (root, gate) = FakeRoot::booting(&layout()).gated();
        let (mut broker, working) = start_clone(root, ACTIVATION_TIMEOUT);
        broker.reset(RESTORED);
        broker.send_data(&bytes, OUTER_MAX_PAYLOAD);
        broker.send_data(&bytes, OUTER_MAX_PAYLOAD);
        gate.send(()).unwrap();
        let mut replies = Vec::new();
        while replies.len() < 2 {
            replies.extend(broker.take());
        }
        assert_eq!(replies, [activated(&record), activated(&record)]);
        let replies = broker.exchange(&[Message::Stop], OUTER_MAX_PAYLOAD, 1);
        assert_eq!(replies, [Message::Stopped]);
        let (root, worked) = joined(working);
        assert_eq!(worked.unwrap(), Worked::Stopped);
        assert_eq!(root.calls, [applied(&record), Call::Teardown]);
    }

    #[test]
    fn a_clone_whose_stream_takes_nothing_still_ends_once_its_activation_failed() {
        // The guest's direction of the stream is full, as once the broker failed: the worker
        // cannot send its acknowledgement of the restore's reset, and still ends once the
        // deadline passed, with the deadline's status, once it waited for the stream for
        // `UNACTIVATED_DRAIN`.
        let (broker, guest) = stalled();
        let reset = super::record(RecordType::Reset, RESTORED, 0, &[]);
        (&broker).write_all(&reset).unwrap();
        let deadline = Duration::from_millis(200);
        let started = Instant::now();
        let working = run_clone(guest, FakeRoot::booting(&layout()), deadline);
        let (root, worked) = joined(working);
        let worked = worked.unwrap();
        assert_eq!(worked.status(), status::ACTIVATION_DEADLINE, "{worked:?}");
        assert!(started.elapsed() >= deadline + UNACTIVATED_DRAIN);
        assert_eq!(root.calls, []);

        // So does a worker whose broker closes its end of the stream before the clone is ready,
        // as an abandoned clone, long before the deadline passes.
        let (broker, guest) = stalled();
        (&broker).write_all(&reset).unwrap();
        broker.shutdown(Shutdown::Write).unwrap();
        let started = Instant::now();
        let working = run_clone(guest, FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
        let (root, worked) = joined(working);
        assert_eq!(worked.unwrap(), Worked::Abandoned);
        assert!(started.elapsed() >= UNACTIVATED_DRAIN);
        assert_eq!(root.calls, []);
    }

    #[test]
    fn a_clone_whose_broker_closes_the_stream_before_it_is_ready_is_abandoned() {
        // Whether the restore's reset came or not, or part of an Activate, a broker that closes
        // the stream before the clone is ready ends the clone with the status of the host's
        // session, with nothing applied, and its fence closed.
        let record = record(1, Some(WORKLOAD));
        let bytes = activate(&record).encode().unwrap();
        let cases: [Option<&[u8]>; 3] = [None, Some(&[]), Some(&bytes[..bytes.len() / 2])];
        for sent in cases {
            let (mut broker, working) =
                start_clone(FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
            if let Some(sent) = sent {
                broker.reset(RESTORED);
                broker.send_data(sent, OUTER_MAX_PAYLOAD);
            }
            broker.close();
            let (root, worked) = joined(working);
            let worked = worked.unwrap();
            assert_eq!(
                (&worked, worked.status()),
                (&Worked::Abandoned, 1),
                "{sent:?}"
            );
            assert_eq!(root.calls, [], "{sent:?}");
            while let Some(record) = broker.read() {
                broker.check(&record);
                assert_eq!(broker.accept(&record), [], "{sent:?}");
            }
        }

        // Once the clone is ready, it ends as the worker of a VM that boots cold does, and leaves
        // the root that it assembled to its operations.
        let (mut broker, working) = start_clone(FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
        broker.reset(RESTORED);
        let replies = broker.exchange(&[activate(&record)], OUTER_MAX_PAYLOAD, 1);
        assert_eq!(replies, [activated(&record)]);
        broker.close();
        let (root, worked) = joined(working);
        let worked = worked.unwrap();
        assert_eq!((&worked, worked.status()), (&Worked::Closed, 1));
        assert_eq!(root.calls, [applied(&record)]);
    }

    #[test]
    fn a_clone_whose_broker_is_gone_still_ends_as_its_activation_tells() {
        // The broker sends the restore's reset, and a ping, which fails the activation, or
        // nothing more, and closes its socket before the worker runs: the worker cannot write
        // its acknowledgement, and still ends with the activation's status, or as an abandoned
        // clone, rather than with the failure of its stream.
        let reset = super::record(RecordType::Reset, RESTORED, 0, &[]);
        let ping = Message::Ping.encode().unwrap();
        let ping = super::record(RecordType::Data, RESTORED, 1, &ping);
        let cases = [
            (
                [reset.as_slice(), &ping].concat(),
                Some(status::ACTIVATION_PROTOCOL),
            ),
            (reset, None),
        ];
        for (sent, expected) in cases {
            let (broker, guest) = UnixStream::pair().unwrap();
            (&broker).write_all(&sent).unwrap();
            drop(broker);
            let working = run_clone(guest, FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
            let (root, worked) = joined(working);
            let worked = worked.unwrap();
            match expected {
                Some(expected) => {
                    assert!(matches!(worked, Worked::Unactivated { .. }), "{worked:?}");
                    assert_eq!(worked.status(), expected, "{worked:?}");
                }
                None => assert_eq!(worked, Worked::Abandoned),
            }
            assert_eq!(root.calls, [], "{worked:?}");
        }
    }

    #[test]
    fn a_ready_clone_fails_on_an_activate_that_is_not_a_retry() {
        // An Activate of another transaction, or of the same transaction with another record, once
        // the clone is ready or its sandbox started, fails the activation with the status of the
        // host's session. The worker leaves the root that it assembled to its operations.
        let accepted = record(1, Some(WORKLOAD));
        let cases = [
            (record(2, Some(WORKLOAD)), false),
            (record(1, None), false),
            (record(1, None), true),
        ];
        for (other, started) in cases {
            let (mut broker, working) =
                start_clone(FakeRoot::booting(&layout()), ACTIVATION_TIMEOUT);
            broker.reset(RESTORED);
            let replies = broker.exchange(&[activate(&accepted)], OUTER_MAX_PAYLOAD, 1);
            assert_eq!(replies, [activated(&accepted)]);
            if started {
                let replies = broker.exchange(&[Message::Start], OUTER_MAX_PAYLOAD, 1);
                assert_eq!(replies, [Message::Started]);
            }
            broker.send_data(&activate(&other).encode().unwrap(), OUTER_MAX_PAYLOAD);
            let root = unactivated(&mut broker, working, status::ACTIVATION_PROTOCOL);
            assert_eq!(root.calls, [applied(&accepted)], "{other:?}");
        }
    }

    /// Returns the entrance of a clone's execution as `identity`, with the hostname `hostname`,
    /// its record's, and unpublished listeners, as a record whose network publishes none, or
    /// that has none, has them.
    fn cloned(identity: entry::Identity, hostname: &[u8]) -> Entrance {
        Entrance {
            hostname: hostname.to_vec(),
            publication: Publication::Unpublished,
            ..entrance(identity)
        }
    }

    #[test]
    fn a_clone_s_executions_run_as_the_workload_identity_and_with_the_hostname_of_its_record() {
        // The record's workload identity is the whole sandbox's, as a root configuration's is:
        // every execution runs as it, and a start that names an identity is refused. Every
        // execution's entry takes the record's hostname.
        let refused = LaunchFailure::new(0, Errno::EINVAL as i32);
        let workload = exec::Identity::new(WORKLOAD.0, WORKLOAD.1);
        let mut host = Host::activated(FakeRoot::booting(&layout()), &record(1, Some(WORKLOAD)));
        host.start(start_of(1, shell("exit 0")), Vec::new());
        let named = start_of(2, shell("exit 0")).with_identity(workload.clone());
        host.start(named, Vec::new());
        host.run_until(|host| (1..=2).all(|id| host.outcome(id).is_some()));
        check_lossless(&host, 1, Status::Exited(0));
        assert_eq!(host.outcome(2).unwrap().launch_failure(), Some(refused));
        let (root, worked) = host.stop();
        assert_eq!(worked.unwrap(), Worked::Stopped);
        let identity = entry::Identity::new(WORKLOAD.0, WORKLOAD.1);
        assert_eq!(entrances(&root), [cloned(identity.clone(), b"clone")]);

        // Without one, each start names its own, and takes the hostname of a record that names
        // another.
        let renamed = changed(&record(1, None), |configuration| {
            configuration.workload.hostname = b"another-clone".to_vec();
        });
        let mut host = Host::activated(FakeRoot::booting(&layout()), &renamed);
        host.start(start_of(1, shell("exit 0")), Vec::new());
        let named = start_of(2, shell("exit 0")).with_identity(workload);
        host.start(named, Vec::new());
        host.run_until(|host| (1..=2).all(|id| host.outcome(id).is_some()));
        assert_eq!(host.outcome(1).unwrap().launch_failure(), Some(refused));
        check_lossless(&host, 2, Status::Exited(0));
        let (root, worked) = host.stop();
        assert_eq!(worked.unwrap(), Worked::Stopped);
        assert_eq!(entrances(&root), [cloned(identity, b"another-clone")]);
    }

    #[test]
    fn a_clone_s_executions_enter_the_root_with_the_publication_of_its_record_s_network() {
        // A record's network publishes the guest's listeners under a policy that allows ingress
        // or host-loopback access both ways, and then the entry refuses at stage 0 the
        // capabilities that could forge a listener announcement, as after a cold boot whose
        // configuration publishes them. A record whose network publishes none, or that has
        // none, leaves them to the execution, as the other capabilities of the allowlist.
        let capabilities = [
            Capability::NET_BIND_SERVICE,
            Capability::NET_RAW,
            Capability::CHOWN,
        ];
        for (record, publication) in [
            (record(1, None), Publication::Unpublished),
            (networked(&record(1, None), None), Publication::Unpublished),
            (published(&record(1, None)), Publication::Published),
        ] {
            let mut host = Host::activated(FakeRoot::booting(&layout()), &record);
            for (id, capability) in (1..).zip(capabilities) {
                let identity = exec::Identity {
                    capabilities: 1 << capability.number(),
                    ..exec::Identity::new(0, 0)
                };
                host.start(
                    start_of(id, shell("exit 0")).with_identity(identity),
                    Vec::new(),
                );
            }
            host.run_until(|host| (1..=3).all(|id| host.outcome(id).is_some()));
            for (id, capability) in (1..).zip(capabilities) {
                if publication == Publication::Published && capability != Capability::CHOWN {
                    let failure = LaunchFailure::new(0, Errno::EPERM as i32);
                    let outcome = host.outcome(id).unwrap();
                    assert_eq!(outcome.launch_failure(), Some(failure), "{capability}");
                } else {
                    check_lossless(&host, id, Status::Exited(0));
                }
            }
            let (root, worked) = host.stop();
            assert_eq!(worked.unwrap(), Worked::Stopped);
            let expected = capabilities.map(|capability| Entrance {
                publication,
                ..cloned(
                    entry::Identity::new(0, 0).capabilities(Capabilities::EMPTY.with(capability)),
                    b"clone",
                )
            });
            assert_eq!(entrances(&root), expected, "{publication:?}");
        }
    }

    #[test]
    fn a_clone_whose_announcer_ends_ends_with_the_network_s_status() {
        // The application started the announcer of the listeners that the record's network
        // publishes, whose end, once the clone is ready, before its sandbox started or after, and
        // with an execution running, ends the clone with the network's status, once the worker
        // dropped its executions: the gateway would keep publishing the listeners that the
        // announcer last announced, and learn of no later change of them.
        let record = published(&record(1, None));
        for started in [false, true] {
            let (root, ending) = FakeRoot::booting(&layout()).ending();
            let (mut broker, working) = start_clone(root, ACTIVATION_TIMEOUT);
            broker.reset(RESTORED);
            let replies = broker.exchange(&[activate(&record)], OUTER_MAX_PAYLOAD, 1);
            assert_eq!(replies, [activated(&record)]);
            // The host waits for Ready before it starts the sandbox: until the activation
            // settled, any message but a retry of the Activate fails it.
            if started {
                let replies = broker.exchange(&[Message::Start], OUTER_MAX_PAYLOAD, 1);
                assert_eq!(replies, [Message::Started]);
            }
            drop(ending);
            let (root, worked) = joined(working);
            let worked = worked.unwrap();
            assert_eq!(
                worked,
                Worked::Unactivated {
                    status: status::ACTIVATION_NETWORK,
                    problem: "the test ended the announcer".to_owned(),
                }
            );
            assert_eq!(root.calls, [applied(&record)], "{started}");
            assert!(root.announcer.is_none());
        }
        let (root, ending) = FakeRoot::booting(&layout()).ending();
        let mut host = Host::activated(root, &record);
        let running = start_of(1, shell("echo up; exec sleep 60"));
        host.start(running.with_identity(exec::Identity::new(0, 0)), Vec::new());
        host.run_until(|host| host.executions[&1].output(Stream::Stdout) == b"up\n");
        drop(ending);
        let (root, worked) = joined(host.working.take().unwrap());
        assert_eq!(worked.unwrap().status(), status::ACTIVATION_NETWORK);
        assert_eq!(root.calls[0], applied(&record));

        // A template without a gateway cannot announce the listeners of a record that publishes
        // them, which fails before Ready with the network's status.
        let root = FakeRoot::booting(&layout());
        let (broker, guest) = UnixStream::pair().unwrap();
        let mut broker = Broker::new(broker);
        let session = captured_session();
        let acknowledged = Instant::now();
        let activation = repaired(acknowledged, ACTIVATION_TIMEOUT);
        let gatewayless = RootTemplate {
            gateway: None,
            ..template()
        };
        let working = thread::spawn(move || {
            let mut root = root;
            let worked = worker::work_clone(
                guest,
                session,
                &mut root,
                activation,
                gatewayless,
                Some(acknowledged),
            );
            (root, worked)
        });
        broker.reset(RESTORED);
        broker.send_data(&activate(&record).encode().unwrap(), OUTER_MAX_PAYLOAD);
        let root = unactivated(&mut broker, working, status::ACTIVATION_NETWORK);
        assert!(root.announcer.is_none());
    }

    /// Returns the facts of the guest's `report`.
    fn reported(report: &Message) -> Facts {
        let Message::Report(body) = report else {
            panic!("the guest did not report: {report:?}");
        };
        Facts::decode(body).unwrap()
    }

    /// Returns the microseconds after the acknowledgement of the restore that the clone's `facts`
    /// report for its event `name`.
    fn after(facts: &Facts, name: &str) -> u64 {
        let value = facts.get(name);
        value
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("the clone's {name} is {value:?}: {facts:?}"))
    }

    #[test]
    fn a_clone_reports_when_its_events_happened_after_the_acknowledgement() {
        // The worker answers the probe of the clone's activation itself, with no root operation,
        // executions running or not: the restore's reset came after the repair's acknowledgement,
        // then the host's first message, the clone's Activate, then its Ready and its start, and
        // no execution yet.
        let record = record(1, Some(WORKLOAD));
        let mut host = Host::activated(FakeRoot::booting(&layout()), &record);
        let probe = Message::Probe(ACTIVATION_PROBE.as_bytes().to_vec());
        host.send(std::slice::from_ref(&probe));
        host.run_until(|host| !host.replies.is_empty());
        let facts = reported(&host.replies.remove(0));
        assert_eq!(facts.get("acknowledged"), Some("yes"));
        assert_eq!(facts.get("execution"), Some("none"));
        let events = ["reset", "message", "ready", "started"].map(|name| after(&facts, name));
        assert!(events.is_sorted(), "{facts:?}");

        // The start of the first execution came once the sandbox started.
        host.start(start_of(1, shell("sleep 1")), Vec::new());
        host.send(&[probe]);
        host.run_until(|host| !host.replies.is_empty());
        let facts = reported(&host.replies.remove(0));
        assert!(after(&facts, "execution") >= events[3], "{facts:?}");
        host.run_until(|host| host.outcome(1).is_some());
        check_lossless(&host, 1, Status::Exited(0));
        let (root, worked) = host.stop();
        assert_eq!(worked.unwrap(), Worked::Stopped);
        assert!(
            !root.calls.iter().any(|call| matches!(call, Call::Probe(_))),
            "{:?}",
            root.calls
        );
    }

    #[test]
    fn a_clone_maps_its_record_s_table_from_its_export_and_leaves_the_proxy_to_the_host() {
        // The application mounted the record's export, and wrote the resolver file of its
        // network's DNS server, before Ready. The host then sends the mapping table that the
        // record announces, in parts, as after a cold boot: the worker applies each part's
        // mappings from the export, and starts the sandbox once it applied every one.
        let table = table(3);
        let parts = parts(&table, 2);
        assert_eq!(parts.len(), 2);
        let record = exported(&record(1, Some(WORKLOAD)), table.len());
        let record = networked(&record, Some("http://10.0.0.1:3128"));
        let mut host = Host::mapped(FakeRoot::booting(&layout()), &record, &parts);
        // The host tells the workloads the endpoint of the network's proxy, as after a cold boot:
        // an execution's environment is exactly its start's, which names no proxy here.
        let printing = start_of(1, vec![b"/usr/bin/env".to_vec()]).with_environment(["NAME=value"]);
        host.start(printing, Vec::new());
        host.run_until(|host| host.outcome(1).is_some());
        check_lossless(&host, 1, Status::Exited(0));
        assert_eq!(host.executions[&1].output(Stream::Stdout), b"NAME=value\n");
        let (root, worked) = host.stop();
        assert_eq!(worked.unwrap(), Worked::Stopped);
        let identity = entry::Identity::new(WORKLOAD.0, WORKLOAD.1);
        let expected = [
            applied(&record),
            Call::Map(mappings(&table[..2])),
            Call::Map(mappings(&table[2..])),
            Call::Prepare(cloned(identity, b"clone")),
            Call::Teardown,
        ];
        assert_eq!(root.calls, expected);
    }
}
