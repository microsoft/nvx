//! The guest's connection over a byte stream: an end-to-end session through the broker, reads
//! that never pass the record in progress, and the failures that end the connection.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use super::{GuestConnection, GuestConnectionError};
use crate::protocol::nvxs::tests::broker::{Broker, CAPABILITY, IDENTITY};
use crate::protocol::nvxs::tests::{Reassembler, framed, messages, record};
use crate::protocol::nvxs::{
    GuestEvent, GuestState, HostEvent, HostSession, RecordParser, RecordType, SessionError,
};
use crate::protocol::{
    OUTER_HEADER_LEN, OUTER_MAX_PAYLOAD, OUTER_MIN_RECEIVE_WINDOW, OUTER_RECEIVE_WINDOW,
};

/// State of a [`Stream`], which the test shares with the connection.
#[derive(Default)]
struct Pipe {
    /// Bytes for the guest to read.
    input: VecDeque<u8>,
    /// Bytes that the guest wrote.
    output: Vec<u8>,
    /// Sizes of the guest's reads.
    reads: Vec<usize>,
    /// Whether the next read is interrupted.
    interrupt: bool,
    /// Whether writes fail.
    broken: bool,
}

/// In-memory stream whose reads end at the end of the input that the test supplied.
#[derive(Clone, Default)]
pub(crate) struct Stream(Arc<Mutex<Pipe>>);

impl Stream {
    /// Supplies bytes for the guest to read.
    pub(crate) fn supply(&self, bytes: &[u8]) {
        self.0.lock().unwrap().input.extend(bytes);
    }

    /// Takes the bytes that the guest wrote.
    pub(crate) fn take_output(&self) -> Vec<u8> {
        std::mem::take(&mut self.0.lock().unwrap().output)
    }

    /// Returns the number of supplied bytes that the guest has not read.
    pub(crate) fn unread(&self) -> usize {
        self.0.lock().unwrap().input.len()
    }

    /// Makes every later write fail.
    pub(crate) fn break_writes(&self) {
        self.0.lock().unwrap().broken = true;
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut pipe = self.0.lock().unwrap();
        if std::mem::take(&mut pipe.interrupt) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        pipe.reads.push(buffer.len());
        let count = buffer.len().min(pipe.input.len());
        for (target, byte) in buffer.iter_mut().zip(pipe.input.drain(..count)) {
            *target = byte;
        }
        Ok(count)
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut pipe = self.0.lock().unwrap();
        if pipe.broken {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        pipe.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Returns the type and sequence number of each record of `bytes`.
fn kinds(bytes: &[u8]) -> Vec<(RecordType, u64)> {
    let mut parser = RecordParser::new();
    let mut kinds = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (consumed, record) = parser.push(rest).unwrap();
        rest = &rest[consumed..];
        let record = record.unwrap();
        kinds.push((record.record_type(), record.sequence()));
    }
    kinds
}

#[test]
fn the_guest_connection_echoes_messages_that_span_records() {
    let stream = Stream::default();
    let mut broker = Broker::new();
    let mut connection = GuestConnection::new(stream.clone());
    connection.attach().unwrap();
    broker.accept_guest(&stream.take_output());
    stream.supply(&broker.take_for_guest());
    assert_eq!(
        connection.next_event().unwrap(),
        GuestEvent::Reset(IDENTITY)
    );
    // With the smallest window, the broker releases the host's DATA as the guest credits it.
    connection.acknowledge(OUTER_MIN_RECEIVE_WINDOW).unwrap();
    broker.accept_guest(&stream.take_output());

    // A host attaches, then sends messages that span records.
    let mut host = HostSession::new();
    broker.connect_host();
    let mut to_broker = Vec::new();
    host.attach(&CAPABILITY, &mut to_broker).unwrap();
    broker.accept_host(&to_broker);
    let ready = broker.take_for_host();
    let attached = host.receive(&ready).unwrap();
    assert_eq!(attached, (ready.len(), Some(HostEvent::Attached(IDENTITY))));
    let expected = messages();
    to_broker.clear();
    for message in &expected {
        let data = framed(message);
        let mut sent = 0;
        while sent < data.len() {
            sent += host.send(&data[sent..], &mut to_broker).unwrap();
        }
    }
    broker.accept_host(&to_broker);

    // The guest echoes each message, and credits each payload once it consumed it.
    let mut application = Reassembler::default();
    let mut echoed = 0;
    while echoed < expected.len() {
        let input = broker.take_for_guest();
        assert!(
            !input.is_empty(),
            "the broker releases DATA as the guest credits it"
        );
        stream.supply(&input);
        while stream.unread() != 0 {
            let GuestEvent::Data(payload) = connection.next_event().unwrap() else {
                panic!("only DATA follows the acknowledgement");
            };
            let messages = application.push(&payload);
            connection.credit(payload.len() as u32).unwrap();
            for message in messages {
                connection.send(&framed(&message)).unwrap();
                echoed += 1;
            }
        }
        broker.accept_guest(&stream.take_output());
    }
    assert_eq!((connection.session().outstanding(), broker.held()), (0, 0));
    assert!(!application.is_partial());

    // The host receives the echoes.
    let mut host_application = Reassembler::default();
    let mut received = Vec::new();
    let from_broker = broker.take_for_host();
    let mut rest = from_broker.as_slice();
    while !rest.is_empty() {
        let (consumed, event) = host.receive(rest).unwrap();
        rest = &rest[consumed..];
        if let Some(HostEvent::Data(payload)) = event {
            received.extend(host_application.push(&payload));
        }
    }
    assert_eq!(received, expected);
}

#[test]
fn the_guest_connection_never_reads_past_the_record_in_progress() {
    let stream = Stream::default();
    let mut connection = GuestConnection::new(stream.clone());
    connection.attach().unwrap();
    let reset = record(RecordType::Reset, IDENTITY, 0, &[]);
    let small = record(RecordType::Data, IDENTITY, 1, b"small");
    let large = record(RecordType::Data, IDENTITY, 2, &[7; OUTER_MAX_PAYLOAD]);
    stream.supply(&[reset.as_slice(), &small, &large].concat());
    assert_eq!(
        connection.next_event().unwrap(),
        GuestEvent::Reset(IDENTITY)
    );
    assert_eq!(stream.unread(), small.len() + large.len());
    connection.acknowledge(OUTER_RECEIVE_WINDOW).unwrap();
    let event = connection.next_event().unwrap();
    assert_eq!(event, GuestEvent::Data(b"small".to_vec()));
    assert_eq!(stream.unread(), large.len());
    let event = connection.next_event().unwrap();
    assert_eq!(event, GuestEvent::Data(vec![7; OUTER_MAX_PAYLOAD]));
    // Each read asked for at most the rest of the header or payload in progress.
    let reads = stream.0.lock().unwrap().reads.clone();
    assert_eq!(
        reads,
        [
            OUTER_HEADER_LEN,
            OUTER_HEADER_LEN,
            5,
            OUTER_HEADER_LEN,
            16 * 1024,
            16 * 1024,
            16 * 1024,
            16 * 1024,
        ]
    );
    // The guest's records share one sequence: ACK, then DATA and CREDIT.
    connection.credit(5 + OUTER_MAX_PAYLOAD as u32).unwrap();
    connection.send(b"reply").unwrap();
    let written = stream.take_output();
    let expected = [
        (RecordType::GuestAttach, 0),
        (RecordType::Ack, 0),
        (RecordType::Credit, 1),
        (RecordType::Data, 2),
    ];
    assert_eq!(kinds(&written), expected);
    // The stream then ends at a record boundary.
    let closed = connection.next_event();
    assert!(
        matches!(closed, Err(GuestConnectionError::Closed)),
        "{closed:?}"
    );
    let ended = connection.send(b"x");
    assert!(
        matches!(ended, Err(GuestConnectionError::Ended)),
        "{ended:?}"
    );
}

#[test]
fn the_guest_connection_fails_closed() {
    // A stream that ends inside a record.
    let stream = Stream::default();
    let mut connection = GuestConnection::new(stream.clone());
    connection.attach().unwrap();
    stream.supply(&record(RecordType::Reset, IDENTITY, 0, &[])[..20]);
    let truncated = connection.next_event();
    let expected = matches!(
        truncated,
        Err(GuestConnectionError::Protocol(SessionError::Truncated {
            buffered: 20
        }))
    );
    assert!(expected, "{truncated:?}");
    let ended = connection.next_event();
    assert!(
        matches!(ended, Err(GuestConnectionError::Ended)),
        "{ended:?}"
    );

    // DATA before the first RESET, after an interrupted read that the connection retries.
    let stream = Stream::default();
    let mut connection = GuestConnection::new(stream.clone());
    connection.attach().unwrap();
    stream.0.lock().unwrap().interrupt = true;
    stream.supply(&record(RecordType::Data, IDENTITY, 0, b"x"));
    let unexpected = connection.next_event();
    let expected = matches!(
        unexpected,
        Err(GuestConnectionError::Protocol(SessionError::Unexpected(
            RecordType::Data
        )))
    );
    assert!(expected, "{unexpected:?}");
    assert_eq!(connection.session().state(), GuestState::Failed);
    let ended = connection.acknowledge(OUTER_RECEIVE_WINDOW);
    assert!(
        matches!(ended, Err(GuestConnectionError::Ended)),
        "{ended:?}"
    );

    // A stream that fails to write.
    let stream = Stream::default();
    let mut connection = GuestConnection::new(stream.clone());
    stream.0.lock().unwrap().broken = true;
    let broken = connection.attach();
    let expected = matches!(&broken, Err(GuestConnectionError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe);
    assert!(expected, "{broken:?}");
    let ended = connection.next_event();
    assert!(
        matches!(ended, Err(GuestConnectionError::Ended)),
        "{ended:?}"
    );
}

#[test]
fn operations_that_the_state_does_not_allow_change_nothing() {
    let stream = Stream::default();
    let mut connection = GuestConnection::new(stream.clone());
    stream.supply(&record(RecordType::Reset, IDENTITY, 0, &[]));
    // Nothing is read before the guest attaches, and nothing is written for a refused operation.
    for result in [
        connection.next_event().map(|_| ()),
        connection.credit(1),
        connection.acknowledge(OUTER_RECEIVE_WINDOW),
        connection.send(b"x"),
    ] {
        let refused = matches!(
            result,
            Err(GuestConnectionError::Protocol(SessionError::InvalidState))
        );
        assert!(refused, "{result:?}");
    }
    assert_eq!(stream.unread(), OUTER_HEADER_LEN);
    assert!(stream.take_output().is_empty());
    connection.attach().unwrap();
    assert_eq!(
        connection.next_event().unwrap(),
        GuestEvent::Reset(IDENTITY)
    );
    let refused = connection.credit(1);
    let expected = matches!(
        refused,
        Err(GuestConnectionError::Protocol(SessionError::InvalidState))
    );
    assert!(expected, "{refused:?}");
    connection.acknowledge(OUTER_RECEIVE_WINDOW).unwrap();
    let refused = connection.credit(1);
    let expected = matches!(
        refused,
        Err(GuestConnectionError::Protocol(SessionError::Credit {
            outstanding: 0,
            bytes: 1
        }))
    );
    assert!(expected, "{refused:?}");
    // An empty message sends nothing.
    connection.send(&[]).unwrap();
    let written = stream.take_output();
    let expected = [(RecordType::GuestAttach, 0), (RecordType::Ack, 0)];
    assert_eq!(kinds(&written), expected);
}
