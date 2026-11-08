//! The host's connection over a platform transport: attachment through WAIT, messages that span
//! records over reads of every size, receives that time out inside a record, and the failures
//! that end the connection.

use std::collections::VecDeque;
use std::error::Error as _;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nvx::host::transport::{HostConnection, HostConnectionError, Transport};
use nvx::protocol::nvxs::{
    ErrorCode, GuestEvent, GuestSession, HostState, Identity, RecordParser, RecordType,
    SessionError,
};
use nvx::protocol::{
    CAPABILITY_LEN, OUTER_HEADER_LEN, OUTER_MAX_PAYLOAD, OUTER_MIN_RECEIVE_WINDOW,
};
use nvx::{Error, ErrorKind};

use crate::broker::{Broker, CAPABILITY, IDENTITY, record};
use crate::{Reassembler, framed, messages};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn cause<T: std::error::Error + 'static>(error: &Error) -> Option<&T> {
    error.source().and_then(|cause| cause.downcast_ref())
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
        assert_eq!(connection.identity(), IDENTITY);
        assert_eq!(connection.session().state(), HostState::Attached);
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
        assert!(matches!(timed_out, Err(error) if error.kind() == ErrorKind::TimedOut));
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
    assert!(
        matches!(&first, Err(error) if error.kind() == ErrorKind::TimedOut),
        "{first:?}"
    );
    assert_eq!(
        connection.receive(Some(deadline())).unwrap(),
        b"the payload"
    );
    let again = connection.receive(None);
    assert!(
        matches!(&again, Err(error) if error.kind() == ErrorKind::TimedOut),
        "{again:?}"
    );
    // The stream then ends at a record boundary.
    let closed = connection.receive(None);
    assert!(
        matches!(&closed, Err(error) if error.kind() == ErrorKind::Protocol
            && matches!(cause(error), Some(HostConnectionError::Closed))),
        "{closed:?}"
    );
    let ended = connection.receive(None);
    assert!(
        matches!(&ended, Err(error) if matches!(cause(error), Some(HostConnectionError::Ended))),
        "{ended:?}"
    );
    let ended = connection.send(b"x", None);
    assert!(
        matches!(&ended, Err(error) if matches!(cause(error), Some(HostConnectionError::Ended))),
        "{ended:?}"
    );
}

#[test]
fn the_host_connection_reports_refusals_and_closed_streams_while_attaching() {
    let refusal = record(RecordType::Error, IDENTITY, 0, &1u32.to_le_bytes());
    let (transport, written) = scripted(vec![Ok(refusal)]);
    let result = HostConnection::attach(transport, &CAPABILITY, deadline());
    let refused = matches!(
        &result,
        Err(error) if error.kind() == ErrorKind::Protocol
            && matches!(cause(error), Some(HostConnectionError::Rejected(ErrorCode::Authentication)))
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
        matches!(&result, Err(error) if matches!(cause(error), Some(HostConnectionError::Closed))),
        "{:?}",
        result.err()
    );

    // An interrupted read is retried, and a deadline that passed fails before anything is sent.
    let interrupted = Err(io::ErrorKind::Interrupted.into());
    let (transport, _) = scripted(vec![interrupted, ready()]);
    assert_eq!(attach(transport).identity(), IDENTITY);
    let (transport, written) = scripted(vec![ready()]);
    let result = HostConnection::attach(transport, &CAPABILITY, Instant::now());
    assert!(
        matches!(&result, Err(error) if error.kind() == ErrorKind::TimedOut),
        "{:?}",
        result.err()
    );
    assert!(written.lock().unwrap().is_empty());
}

#[test]
fn resets_truncations_violations_and_transport_failures_end_the_connection() {
    /// Checks the failure of a case.
    type Check = fn(&Error) -> bool;
    let next = Identity {
        epoch: 2,
        ..IDENTITY
    };
    let data = record(RecordType::Data, IDENTITY, 1, b"x");
    let reset_by_peer = Err(io::ErrorKind::ConnectionReset.into());
    let cases: [(io::Result<Vec<u8>>, Check); 4] = [
        (Ok(record(RecordType::Reset, next, 1, &[])), |error| {
            error.kind() == ErrorKind::Protocol
                && matches!(
                    cause(error),
                    Some(HostConnectionError::Reset(Identity { epoch: 2, .. }))
                )
        }),
        (Ok(data[..20].to_vec()), |error| {
            matches!(cause(error), Some(SessionError::Truncated { buffered: 20 }))
        }),
        (Ok(record(RecordType::Data, IDENTITY, 2, b"x")), |error| {
            matches!(
                cause(error),
                Some(SessionError::Sequence {
                    expected: 1,
                    actual: 2
                })
            )
        }),
        (reset_by_peer, |error| {
            error.kind() == ErrorKind::Protocol
                && matches!(cause::<io::Error>(error), Some(error) if error.kind() == io::ErrorKind::ConnectionReset)
        }),
    ];
    for (read, expected) in cases {
        let (transport, _) = scripted(vec![ready(), read]);
        let mut connection = attach(transport);
        let error = connection.receive(Some(deadline())).unwrap_err();
        assert!(expected(&error), "{error:?}");
        let ended = connection.receive(Some(deadline()));
        assert!(
            matches!(&ended, Err(error) if matches!(cause(error), Some(HostConnectionError::Ended))),
            "{ended:?}"
        );
        let ended = connection.send(b"x", Some(deadline()));
        assert!(
            matches!(&ended, Err(error) if matches!(cause(error), Some(HostConnectionError::Ended))),
            "{ended:?}"
        );
    }
}

#[test]
fn a_send_that_times_out_before_writing_leaves_the_connection_usable() {
    let (transport, written) = scripted(vec![ready()]);
    let mut connection = attach(transport);
    let result = connection.send(b"late", Some(Instant::now()));
    assert!(
        matches!(&result, Err(error) if error.kind() == ErrorKind::TimedOut),
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
    let error = connection.receive(Some(deadline())).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    assert_eq!(
        cause::<io::Error>(&error).unwrap().to_string(),
        "the original read failure"
    );
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
    let error = connection.send(b"x", Some(deadline())).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    assert_eq!(
        cause::<io::Error>(&error).unwrap().to_string(),
        "the original write failure"
    );
    let error = connection.receive(None).unwrap_err();
    assert!(matches!(cause(&error), Some(HostConnectionError::Ended)));
}

#[test]
fn other_transport_failures_keep_their_original_io_source() {
    let (transport, _) = scripted(vec![
        ready(),
        Err(io::Error::other("the original transport failure")),
    ]);
    let mut connection = attach(transport);
    let error = connection.receive(Some(deadline())).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        cause::<io::Error>(&error).unwrap().to_string(),
        "the original transport failure"
    );
    let error = connection.send(b"x", None).unwrap_err();
    assert!(matches!(cause(&error), Some(HostConnectionError::Ended)));
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
    assert!(
        matches!(&result, Err(error) if error.kind() == ErrorKind::TimedOut),
        "{result:?}"
    );
    let ended = connection.receive(Some(deadline()));
    assert!(
        matches!(&ended, Err(error) if matches!(cause(error), Some(HostConnectionError::Ended))),
        "{ended:?}"
    );

    // A transport failure does too.
    let on_write: OnWrite = Box::new(|index| match index {
        0 => Ok(()),
        _ => Err(io::ErrorKind::BrokenPipe.into()),
    });
    let (transport, _) = scripted_writes(vec![ready()], on_write);
    let mut connection = attach(transport);
    let result = connection.send(b"x", None);
    let broken = matches!(&result, Err(error) if error.kind() == ErrorKind::Protocol
        && matches!(cause::<io::Error>(error), Some(error) if error.kind() == io::ErrorKind::BrokenPipe));
    assert!(broken, "{result:?}");
    let ended = connection.receive(None);
    assert!(
        matches!(&ended, Err(error) if matches!(cause(error), Some(HostConnectionError::Ended))),
        "{ended:?}"
    );

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
        matches!(&result, Err(error) if error.kind() == ErrorKind::TimedOut),
        "{result:?}"
    );
    let ended = connection.send(b"x", None);
    assert!(
        matches!(&ended, Err(error) if matches!(cause(error), Some(HostConnectionError::Ended))),
        "{ended:?}"
    );
    // The attachment and the message's first record went out.
    let attachment = OUTER_HEADER_LEN + CAPABILITY_LEN;
    let first_record = OUTER_HEADER_LEN + OUTER_MAX_PAYLOAD;
    assert_eq!(written.lock().unwrap().len(), attachment + first_record);
}
