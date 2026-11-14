//! The host's connection over a platform transport: attachment through WAIT, messages that span
//! records over reads of every size, receives that time out inside a record or poll once their
//! deadline passed, and the failures that end the connection.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::host::transport::{HostConnection, HostConnectionError, Transport};
use crate::protocol::nvxs::tests::broker::{Broker, CAPABILITY, IDENTITY};
use crate::protocol::nvxs::tests::{Reassembler, framed, messages, record};
use crate::protocol::nvxs::{
    ErrorCode, GuestEvent, GuestSession, HostState, Identity, RecordParser, RecordType,
    SessionError,
};
use crate::protocol::{
    CAPABILITY_LEN, OUTER_HEADER_LEN, OUTER_MAX_PAYLOAD, OUTER_MIN_RECEIVE_WINDOW,
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

/// Returns whether `result` failed with a transport error of `kind`.
fn io_failed<T>(result: &Result<T, HostConnectionError>, kind: io::ErrorKind) -> bool {
    matches!(result, Err(HostConnectionError::Io(error)) if error.kind() == kind)
}

/// Returns whether `result` failed because an earlier failure ended the connection.
fn ended<T>(result: &Result<T, HostConnectionError>) -> bool {
    matches!(result, Err(HostConnectionError::Ended))
}

/// The broker and a guest whose application echoes each message.
struct World {
    /// Broker between the host and the guest.
    broker: Broker,
    /// The guest's role.
    guest: GuestSession,
    /// Receiver of the guest's application.
    application: Reassembler,
}

impl World {
    /// Creates a world whose guest attached and received the broker's reset, which it has not
    /// acknowledged, with a host connection open.
    fn new() -> Self {
        let mut world = Self {
            broker: Broker::new(),
            guest: GuestSession::new(),
            application: Reassembler::default(),
        };
        let mut output = Vec::new();
        world.guest.attach(&mut output).unwrap();
        world.broker.accept_guest(&output);
        let reset = world.broker.take_for_guest();
        let (_, event) = world.guest.receive(&reset).unwrap();
        assert!(matches!(event, Some(GuestEvent::Reset(_))));
        world.broker.connect_host();
        world
    }

    /// Acknowledges the reset if the guest has not, then serves the guest's DATA until the
    /// broker has none for it.
    fn run_guest(&mut self) {
        let mut output = Vec::new();
        if self.guest.window().is_none() {
            self.guest
                .acknowledge(OUTER_MIN_RECEIVE_WINDOW, &mut output)
                .unwrap();
            self.broker.accept_guest(&output);
        }
        loop {
            let input = self.broker.take_for_guest();
            if input.is_empty() {
                return;
            }
            let mut rest = input.as_slice();
            while !rest.is_empty() {
                output.clear();
                let (consumed, event) = self.guest.receive(rest).unwrap();
                rest = &rest[consumed..];
                if let Some(GuestEvent::Data(payload)) = event {
                    self.guest
                        .credit(payload.len() as u32, &mut output)
                        .unwrap();
                    for message in self.application.push(&payload) {
                        let data = framed(&message);
                        let mut sent = 0;
                        while sent < data.len() {
                            sent += self.guest.send(&data[sent..], &mut output).unwrap();
                        }
                    }
                }
                self.broker.accept_guest(&output);
            }
        }
    }
}

/// Transport to the world's broker that returns at most `piece` bytes per read. The guest gets
/// ready only once the host reads with nothing left to read, so the host waits first.
struct BrokerTransport {
    /// World behind the broker.
    world: Arc<Mutex<World>>,
    /// Bytes from the broker that the host has not read.
    pending: VecDeque<u8>,
    /// Most bytes that one read returns.
    piece: usize,
}

impl Transport for BrokerTransport {
    fn read(&mut self, buffer: &mut [u8], _timeout: Option<Duration>) -> io::Result<usize> {
        if self.pending.is_empty() {
            let mut world = self.world.lock().unwrap();
            world.run_guest();
            self.pending.extend(world.broker.take_for_host());
        }
        if self.pending.is_empty() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let count = buffer.len().min(self.piece).min(self.pending.len());
        for (target, byte) in buffer.iter_mut().zip(self.pending.drain(..count)) {
            *target = byte;
        }
        Ok(count)
    }

    fn write_all(&mut self, data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
        let mut world = self.world.lock().unwrap();
        world.broker.accept_host(data);
        if world.guest.window().is_some() {
            world.run_guest();
        }
        Ok(())
    }
}

#[test]
fn the_host_connection_waits_attaches_and_exchanges_messages() {
    for piece in [1, 5, 44, 64 * 1024] {
        let world = Arc::new(Mutex::new(World::new()));
        let transport = Box::new(BrokerTransport {
            world: Arc::clone(&world),
            pending: VecDeque::new(),
            piece,
        });
        // The guest acknowledges the reset only after the broker answered the host's attachment,
        // which it did with WAIT.
        let mut connection = HostConnection::attach(transport, &CAPABILITY, deadline()).unwrap();
        assert_eq!(connection.session.identity(), Some(IDENTITY));
        assert_eq!(connection.session.state(), HostState::Attached);
        let window = world.lock().unwrap().guest.window();
        assert_eq!(window, Some(OUTER_MIN_RECEIVE_WINDOW));
        let mut application = Reassembler::default();
        let mut received = Vec::new();
        for message in messages() {
            connection
                .send(&framed(&message), Some(deadline()))
                .unwrap();
            let expected = received.len() + 1;
            while received.len() < expected {
                let payload = connection.receive(Some(deadline())).unwrap();
                received.extend(application.push(&payload));
            }
        }
        assert_eq!(received, messages(), "reads of {piece} bytes");
        assert!(!application.is_partial());
        assert_eq!(world.lock().unwrap().broker.held(), 0);
        // Nothing more arrives.
        let timed_out = connection.receive(Some(deadline()));
        assert!(io_failed(&timed_out, io::ErrorKind::TimedOut));
    }
}

/// Action of a scripted write: the index of the write, from 0.
type OnWrite = Box<dyn FnMut(usize) -> io::Result<()> + Send>;

/// Transport that returns one scripted read per call, then the end of the stream, and records the
/// host's writes.
struct Scripted {
    /// Results of the reads to come.
    reads: VecDeque<io::Result<Vec<u8>>>,
    /// Bytes that the host wrote.
    written: Arc<Mutex<Vec<u8>>>,
    /// Number of writes so far.
    writes: usize,
    /// Decides the result of each write.
    on_write: OnWrite,
}

impl Transport for Scripted {
    fn read(&mut self, buffer: &mut [u8], _timeout: Option<Duration>) -> io::Result<usize> {
        match self.reads.pop_front() {
            None => Ok(0),
            Some(Err(error)) => Err(error),
            Some(Ok(bytes)) => {
                buffer[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
        }
    }

    fn write_all(&mut self, data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
        let index = self.writes;
        self.writes += 1;
        (self.on_write)(index)?;
        self.written.lock().unwrap().extend_from_slice(data);
        Ok(())
    }
}

/// Returns a transport that returns `reads`, accepts every write, and records them.
fn scripted(reads: Vec<io::Result<Vec<u8>>>) -> (Box<dyn Transport>, Arc<Mutex<Vec<u8>>>) {
    scripted_writes(reads, Box::new(|_| Ok(())))
}

/// Returns a transport that returns `reads` and decides each write with `on_write`.
fn scripted_writes(
    reads: Vec<io::Result<Vec<u8>>>,
    on_write: OnWrite,
) -> (Box<dyn Transport>, Arc<Mutex<Vec<u8>>>) {
    let written = Arc::new(Mutex::new(Vec::new()));
    let transport = Scripted {
        reads: reads.into(),
        written: Arc::clone(&written),
        writes: 0,
        on_write,
    };
    (Box::new(transport), written)
}

/// The broker's READY at sequence 0.
fn ready() -> io::Result<Vec<u8>> {
    Ok(record(RecordType::Ready, IDENTITY, 0, &[]))
}

/// A read that times out.
fn timed_out() -> io::Result<Vec<u8>> {
    Err(io::ErrorKind::TimedOut.into())
}

/// Attaches over `transport`, which starts with READY.
fn attach(transport: Box<dyn Transport>) -> HostConnection {
    HostConnection::attach(transport, &CAPABILITY, deadline()).unwrap()
}

#[test]
fn a_receive_that_times_out_keeps_the_partial_record() {
    let data = record(RecordType::Data, IDENTITY, 1, b"the payload");
    let (transport, _) = scripted(vec![
        ready(),
        Ok(data[..30].to_vec()),
        timed_out(),
        Ok(data[30..].to_vec()),
        timed_out(),
    ]);
    let mut connection = attach(transport);
    let first = connection.receive(Some(deadline()));
    assert!(io_failed(&first, io::ErrorKind::TimedOut), "{first:?}");
    assert_eq!(
        connection.receive(Some(deadline())).unwrap(),
        b"the payload"
    );
    let again = connection.receive(None);
    assert!(io_failed(&again, io::ErrorKind::TimedOut), "{again:?}");
    // The stream then ends at a record boundary.
    let closed = connection.receive(None);
    assert!(
        matches!(&closed, Err(HostConnectionError::Closed)),
        "{closed:?}"
    );
    let result = connection.receive(None);
    assert!(ended(&result), "{result:?}");
    let result = connection.send(b"x", None);
    assert!(ended(&result), "{result:?}");
}

/// Transport that records the timeout of each read from another.
struct Timed {
    /// Transport that serves the reads and writes.
    inner: Box<dyn Transport>,
    /// Timeout of each read so far.
    timeouts: Arc<Mutex<Vec<Option<Duration>>>>,
}

impl Transport for Timed {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        self.timeouts.lock().unwrap().push(timeout);
        self.inner.read(buffer, timeout)
    }

    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        self.inner.write_all(data, timeout)
    }
}

#[test]
fn a_receive_whose_deadline_passed_polls_what_arrived_without_waiting() {
    let data = record(RecordType::Data, IDENTITY, 1, b"arrived");
    let (inner, _) = scripted(vec![
        ready(),
        Ok(data[..30].to_vec()),
        Ok(data[30..].to_vec()),
        timed_out(),
        Ok(record(RecordType::Data, IDENTITY, 2, b"later")),
    ]);
    let timeouts = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(Timed {
        inner,
        timeouts: Arc::clone(&timeouts),
    });
    let mut connection = attach(transport);
    timeouts.lock().unwrap().clear();
    let passed = Instant::now();
    // A record that arrives over several reads completes, and each read takes only what the
    // transport holds.
    assert_eq!(connection.receive(Some(passed)).unwrap(), b"arrived");
    let polled = connection.receive(Some(passed));
    assert!(io_failed(&polled, io::ErrorKind::TimedOut), "{polled:?}");
    assert_eq!(connection.receive(Some(passed)).unwrap(), b"later");
    assert_eq!(*timeouts.lock().unwrap(), [Some(Duration::ZERO); 4]);
}

#[test]
fn a_connection_opens_holding_what_arrived_with_its_attachment() {
    // The broker's READY and the guest's first bytes arrive in one read, which leaves the
    // transport with nothing to read once the connection opens.
    let mut arrived = record(RecordType::Ready, IDENTITY, 0, &[]);
    arrived.extend(record(RecordType::Data, IDENTITY, 1, b"first"));
    arrived.extend(record(RecordType::Data, IDENTITY, 2, b"second"));
    let (inner, _) = scripted(vec![Ok(arrived), timed_out()]);
    let timeouts = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(Timed {
        inner,
        timeouts: Arc::clone(&timeouts),
    });
    let mut connection = attach(transport);
    timeouts.lock().unwrap().clear();
    let passed = Instant::now();
    assert_eq!(connection.receive(Some(passed)).unwrap(), b"first");
    assert_eq!(connection.receive(Some(passed)).unwrap(), b"second");
    assert!(
        timeouts.lock().unwrap().is_empty(),
        "the transport was read"
    );
    let drained = connection.receive(Some(passed));
    assert!(io_failed(&drained, io::ErrorKind::TimedOut), "{drained:?}");
    assert_eq!(*timeouts.lock().unwrap(), [Some(Duration::ZERO)]);
}

/// Transport whose broker answers the host's attachment with WAIT whenever the host reads.
struct Waiting {
    /// Sequence number of the next WAIT.
    sequence: u64,
}

impl Transport for Waiting {
    fn read(&mut self, buffer: &mut [u8], _timeout: Option<Duration>) -> io::Result<usize> {
        let wait = record(RecordType::Wait, IDENTITY, self.sequence, &[]);
        self.sequence += 1;
        buffer[..wait.len()].copy_from_slice(&wait);
        Ok(wait.len())
    }

    fn write_all(&mut self, _data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_broker_that_keeps_the_host_waiting_times_the_attachment_out() {
    let transport = Box::new(Waiting { sequence: 0 });
    let deadline = Instant::now() + Duration::from_millis(50);
    let result = HostConnection::attach(transport, &CAPABILITY, deadline);
    assert!(
        matches!(&result, Err(HostConnectionError::TimedOut)),
        "{:?}",
        result.err()
    );
    assert!(Instant::now() >= deadline);
}

#[test]
fn the_host_connection_reports_refusals_and_closed_streams_while_attaching() {
    let refusal = record(RecordType::Error, IDENTITY, 0, &1u32.to_le_bytes());
    let (transport, written) = scripted(vec![Ok(refusal)]);
    let result = HostConnection::attach(transport, &CAPABILITY, deadline());
    let refused = matches!(
        &result,
        Err(HostConnectionError::Rejected(ErrorCode::Authentication))
    );
    assert!(refused, "{:?}", result.err());
    // The attachment carried the capability.
    let mut parser = RecordParser::new();
    let written = written.lock().unwrap();
    let (consumed, attachment) = parser.push(&written).unwrap();
    let attachment = attachment.unwrap();
    assert_eq!(consumed, written.len());
    assert_eq!(attachment.record_type(), RecordType::HostAttach);
    assert_eq!(attachment.payload(), CAPABILITY);

    // OpenVMM's broker closes the stream instead.
    let (transport, _) = scripted(Vec::new());
    let result = HostConnection::attach(transport, &CAPABILITY, deadline());
    assert!(
        matches!(&result, Err(HostConnectionError::Closed)),
        "{:?}",
        result.err()
    );

    // An interrupted read is retried, and a deadline that passed fails before anything is sent.
    let interrupted = Err(io::ErrorKind::Interrupted.into());
    let (transport, _) = scripted(vec![interrupted, ready()]);
    assert_eq!(attach(transport).session.identity(), Some(IDENTITY));
    let (transport, written) = scripted(vec![ready()]);
    let result = HostConnection::attach(transport, &CAPABILITY, Instant::now());
    assert!(
        matches!(&result, Err(HostConnectionError::TimedOut)),
        "{:?}",
        result.err()
    );
    assert!(written.lock().unwrap().is_empty());
}

#[test]
fn resets_truncations_violations_and_transport_failures_end_the_connection() {
    /// Checks the failure of a case.
    type Check = fn(&HostConnectionError) -> bool;
    let next = Identity {
        epoch: 2,
        ..IDENTITY
    };
    let data = record(RecordType::Data, IDENTITY, 1, b"x");
    let reset_by_peer = Err(io::ErrorKind::ConnectionReset.into());
    let cases: [(io::Result<Vec<u8>>, Check); 4] = [
        (Ok(record(RecordType::Reset, next, 1, &[])), |error| {
            matches!(error, HostConnectionError::Reset(Identity { epoch: 2, .. }))
        }),
        (Ok(data[..20].to_vec()), |error| {
            matches!(
                error,
                HostConnectionError::Protocol(SessionError::Truncated { buffered: 20 })
            )
        }),
        (Ok(record(RecordType::Data, IDENTITY, 2, b"x")), |error| {
            matches!(
                error,
                HostConnectionError::Protocol(SessionError::Sequence {
                    expected: 1,
                    actual: 2
                })
            )
        }),
        (
            reset_by_peer,
            |error| matches!(error, HostConnectionError::Io(error) if error.kind() == io::ErrorKind::ConnectionReset),
        ),
    ];
    for (read, expected) in cases {
        let (transport, _) = scripted(vec![ready(), read]);
        let mut connection = attach(transport);
        let error = connection.receive(Some(deadline())).unwrap_err();
        assert!(expected(&error), "{error:?}");
        let result = connection.receive(Some(deadline()));
        assert!(ended(&result), "{result:?}");
        let result = connection.send(b"x", Some(deadline()));
        assert!(ended(&result), "{result:?}");
    }
}

#[test]
fn a_send_that_times_out_before_writing_leaves_the_connection_usable() {
    let (transport, written) = scripted(vec![ready()]);
    let mut connection = attach(transport);
    let result = connection.send(b"late", Some(Instant::now()));
    assert!(
        matches!(&result, Err(HostConnectionError::TimedOut)),
        "{result:?}"
    );
    // The DATA that follows takes the first sequence number.
    connection.send(b"on time", Some(deadline())).unwrap();
    let written = written.lock().unwrap();
    let mut parser = RecordParser::new();
    let (consumed, _) = parser.push(&written).unwrap();
    let (_, data) = parser.push(&written[consumed..]).unwrap();
    let data = data.unwrap();
    assert_eq!((data.sequence(), data.payload()), (0, &b"on time"[..]));
}

#[test]
fn transport_timeouts_keep_the_original_read_and_write_causes() {
    let (transport, _) = scripted(vec![
        ready(),
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the original read failure",
        )),
        Ok(record(RecordType::Data, IDENTITY, 1, b"after timeout")),
    ]);
    let mut connection = attach(transport);
    let error = connection.receive(Some(deadline()));
    assert!(io_failed(&error, io::ErrorKind::TimedOut), "{error:?}");
    let Err(HostConnectionError::Io(error)) = error else {
        unreachable!()
    };
    assert_eq!(error.to_string(), "the original read failure");
    assert_eq!(
        connection.receive(Some(deadline())).unwrap(),
        b"after timeout"
    );

    let on_write: OnWrite = Box::new(|index| match index {
        0 => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the original write failure",
        )),
    });
    let (transport, _) = scripted_writes(vec![ready()], on_write);
    let mut connection = attach(transport);
    let error = connection.send(b"x", Some(deadline()));
    assert!(io_failed(&error, io::ErrorKind::TimedOut), "{error:?}");
    let Err(HostConnectionError::Io(error)) = error else {
        unreachable!()
    };
    assert_eq!(error.to_string(), "the original write failure");
    let result = connection.receive(None);
    assert!(ended(&result), "{result:?}");
}

#[test]
fn other_transport_failures_keep_their_original_io_source() {
    let (transport, _) = scripted(vec![
        ready(),
        Err(io::Error::other("the original transport failure")),
    ]);
    let mut connection = attach(transport);
    let error = connection.receive(Some(deadline()));
    assert!(io_failed(&error, io::ErrorKind::Other), "{error:?}");
    let Err(HostConnectionError::Io(error)) = error else {
        unreachable!()
    };
    assert_eq!(error.to_string(), "the original transport failure");
    let result = connection.send(b"x", None);
    assert!(ended(&result), "{result:?}");
}

#[test]
fn a_send_that_fails_after_writing_ends_the_connection() {
    // A write that times out may have written part of a record.
    let on_write: OnWrite = Box::new(|index| match index {
        0 => Ok(()),
        _ => Err(io::ErrorKind::TimedOut.into()),
    });
    let (transport, _) = scripted_writes(vec![ready()], on_write);
    let mut connection = attach(transport);
    let result = connection.send(b"x", Some(deadline()));
    assert!(io_failed(&result, io::ErrorKind::TimedOut), "{result:?}");
    let result = connection.receive(Some(deadline()));
    assert!(ended(&result), "{result:?}");

    // A transport failure does too.
    let on_write: OnWrite = Box::new(|index| match index {
        0 => Ok(()),
        _ => Err(io::ErrorKind::BrokenPipe.into()),
    });
    let (transport, _) = scripted_writes(vec![ready()], on_write);
    let mut connection = attach(transport);
    let result = connection.send(b"x", None);
    assert!(io_failed(&result, io::ErrorKind::BrokenPipe), "{result:?}");
    let result = connection.receive(None);
    assert!(ended(&result), "{result:?}");

    // So does a deadline that passes between two records of a message, which leaves the message
    // unfinished: the first record's write lasts until the deadline.
    let message_deadline = Arc::new(Mutex::new(None::<Instant>));
    let shared = Arc::clone(&message_deadline);
    let on_write: OnWrite = Box::new(move |index| {
        if index == 1 {
            let deadline = shared.lock().unwrap().unwrap();
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        }
        Ok(())
    });
    let (transport, written) = scripted_writes(vec![ready()], on_write);
    let mut connection = attach(transport);
    let deadline = Instant::now() + Duration::from_millis(500);
    *message_deadline.lock().unwrap() = Some(deadline);
    let result = connection.send(&vec![0; 100_000], Some(deadline));
    assert!(
        matches!(&result, Err(HostConnectionError::TimedOut)),
        "{result:?}"
    );
    let result = connection.send(b"x", None);
    assert!(ended(&result), "{result:?}");
    // The attachment and the message's first record went out.
    let attachment = OUTER_HEADER_LEN + CAPABILITY_LEN;
    let first_record = OUTER_HEADER_LEN + OUTER_MAX_PAYLOAD;
    assert_eq!(written.lock().unwrap().len(), attachment + first_record);
}
