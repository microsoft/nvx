//! Unit tests of the worker's executions, which a test drives at the instants that it chooses, as
//! a loop that did not run for a while would, over a connection whose broker's end decodes the
//! worker's messages.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use nix::poll::{self, PollFd, PollFlags, PollTimeout};
use nvx::exec::{Cause, Key, Limits, Outcome, Status, Timers, Window};
use nvx::guest::entry::{self, Prepared};
use nvx::guest::process::Child;
use nvx::guest::spawn::{Command, Spawned};
use nvx::protocol::nvxs::{RecordParser, RecordType, encode_record};

use super::Executions;
use crate::exec::{AccountMode, GuestMessage, HostMessage, Identity, Start};
use crate::frame::{Decoder, Message};
use crate::worker::connection::{Connection, Received};
use crate::worker::{Entrance, EntryOperations};

/// Longest wait for an execution's terminal outcome.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Entry operations that run each program without entering a root, which needs privileges: they
/// prepare each entry into the test's own root, `/`, whose account databases they read in
/// existing-account mode alone, which the [identities](numeric) of these tests do not use.
struct Unentered;

impl EntryOperations for Unentered {
    fn prepare(&mut self, entrance: &Entrance) -> Result<Prepared, entry::Error> {
        entrance.entry("/").prepare()
    }

    fn spawn(&mut self, command: Command, _prepared: Prepared) -> io::Result<Spawned> {
        command.spawn()
    }
}

/// Returns the identity of user ID 0 and group ID 0, with numeric IDs, which the executions of
/// these tests name, as the worker runs them without a workload identity.
fn numeric() -> Identity {
    Identity {
        account: AccountMode::Numeric,
        ..Identity::new(0, 0)
    }
}

/// The broker's end of the worker's stream, which takes the worker's records as they arrive, and
/// decodes the messages of their DATA.
struct Broker {
    /// The stream, which the broker made nonblocking.
    stream: UnixStream,
    /// The parser of the worker's records.
    parser: RecordParser,
    /// The decoder of the worker's messages.
    decoder: Decoder,
}

impl Broker {
    /// Returns the worker's messages that the stream holds, without waiting for more.
    fn messages(&mut self) -> Vec<Message> {
        let mut messages = Vec::new();
        let mut bytes = [0; 4096];
        loop {
            let count = match self.stream.read(&mut bytes) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return messages,
                Err(error) => panic!("cannot read the worker's stream: {error}"),
            };
            assert_ne!(count, 0, "the worker ended its stream");
            let mut rest = &bytes[..count];
            while !rest.is_empty() {
                let (consumed, record) = self.parser.push(rest).unwrap();
                rest = &rest[consumed..];
                let Some(record) = record.filter(|record| record.record_type() == RecordType::Data)
                else {
                    continue;
                };
                let mut payload = record.payload();
                while !payload.is_empty() {
                    let (consumed, message) = self.decoder.push(payload).unwrap();
                    payload = &payload[consumed..];
                    messages.extend(message);
                }
            }
        }
    }
}

/// Returns the worker's connection over one end of a socket pair, in the epoch that the broker on
/// the other end started, with the broker.
fn connected() -> (Connection<UnixStream>, Broker) {
    let (worker, mut stream) = UnixStream::pair().unwrap();
    let mut connection = Connection::new(worker).unwrap();
    let mut reset = Vec::new();
    encode_record(RecordType::Reset, &[0x5c; 16], 1, 0, &[], &mut reset).unwrap();
    stream.write_all(&reset).unwrap();
    // The socket holds the whole record, so the connection receives it at once.
    let received = connection.receive().unwrap();
    assert!(
        matches!(received, Some(Received::Reset)),
        "the reset arrives"
    );
    stream.set_nonblocking(true).unwrap();
    let broker = Broker {
        stream,
        parser: RecordParser::new(),
        decoder: Decoder::new(),
    };
    (connection, broker)
}

/// Takes turns of `executions` until the worker sent the terminal record of an execution on
/// `connection`, which `broker` decodes, and returns the execution's outcome.
fn terminal(
    executions: &mut Executions,
    connection: &mut Connection<UnixStream>,
    broker: &mut Broker,
) -> Outcome {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        executions.run(Instant::now(), connection).unwrap();
        connection.flush().unwrap();
        for message in broker.messages() {
            if let Some(Ok(GuestMessage::Terminal(_, outcome))) = GuestMessage::decode(&message) {
                return outcome;
            }
        }
        assert!(Instant::now() < deadline, "the execution ends in time");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_cancellation_after_the_timeout_leaves_a_program_that_ended_before_it_its_own_end() {
    // The program ends at once, well before its timeout, but no turn of the loop collects its
    // status before the worker hands its execution the host's cancellation, once the timeout's
    // instant passed, as once the worker did not run meanwhile.
    let (mut connection, mut broker) = connected();
    let mut executions = Executions::new();
    let key = Key::new(1, 1);
    let timeout = Duration::from_secs(1);
    let limits = Limits::new(16 * 1024, Window::frames(4), Window::frames(8))
        .unwrap()
        .shared_output();
    let timers = Timers::new(Duration::from_millis(300), Duration::from_secs(5)).timeout(timeout);
    let arguments = [&b"/bin/sh"[..], b"-c", b"exit 0"];
    let started = Instant::now();
    let start = Start::new(key, limits, timers, arguments).with_identity(numeric());
    let start = HostMessage::Start(start);
    executions
        .deliver(started, start, &mut Unentered, &mut connection)
        .unwrap();
    {
        let program = executions.attached[&1].program.as_ref();
        let pidfd = program.and_then(Child::pidfd).unwrap();
        let mut descriptors = [PollFd::new(pidfd, PollFlags::POLLIN)];
        let left = (started + timeout).saturating_duration_since(Instant::now());
        poll::poll(&mut descriptors, PollTimeout::try_from(left).unwrap()).unwrap();
        let ended = descriptors[0].revents().unwrap();
        assert!(
            ended.contains(PollFlags::POLLIN),
            "the program ends before its timeout"
        );
    }
    thread::sleep(
        (started + timeout + Duration::from_millis(100)).saturating_duration_since(Instant::now()),
    );

    let cancel = HostMessage::Cancel(key, Cause::Cancelled);
    executions
        .deliver(Instant::now(), cancel, &mut Unentered, &mut connection)
        .unwrap();
    let outcome = terminal(&mut executions, &mut connection, &mut broker);
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert_eq!(outcome.cause(), None);
}
