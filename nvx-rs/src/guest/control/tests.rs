//! The guest's control connection: a session through the broker whose bytes flow past the
//! receive window as the guest receives them, its resets, and the failures that end it.

use std::error::Error as _;

use super::{Connection, ConnectionErrorKind, Event};
use crate::guest::nvxs::tests::Stream;
use crate::protocol::OUTER_RECEIVE_WINDOW;
use crate::protocol::nvxs::tests::broker::{Broker, CAPABILITY, IDENTITY};
use crate::protocol::nvxs::tests::record;
use crate::protocol::nvxs::{HostEvent, HostSession, RecordType};

/// Returns `length` bytes in which a lost, repeated, or reordered piece shows.
fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

/// Connects a host to `broker`, and returns its session once the broker attached it.
fn attach_host(broker: &mut Broker) -> HostSession {
    let mut host = HostSession::new();
    broker.connect_host();
    let mut to_broker = Vec::new();
    host.attach(&CAPABILITY, &mut to_broker).unwrap();
    broker.accept_host(&to_broker);
    let ready = broker.take_for_host();
    let attached = host.receive(&ready).unwrap();
    let expected = (ready.len(), Some(HostEvent::Attached(broker.identity())));
    assert_eq!(attached, expected);
    host
}

/// Returns the DATA records of `host` for `data`.
fn host_data(host: &mut HostSession, data: &[u8]) -> Vec<u8> {
    let mut records = Vec::new();
    let mut sent = 0;
    while sent < data.len() {
        sent += host.send(&data[sent..], &mut records).unwrap();
    }
    records
}

#[test]
fn the_connection_carries_bytes_past_its_window_and_resets_with_the_host() {
    let stream = Stream::default();
    let mut broker = Broker::new();
    let mut connection = Connection::attach(stream.clone()).unwrap();
    broker.accept_guest(&stream.take_output());

    // Out of the session's order, calls change nothing: before the reset, an acknowledgement or
    // a send, and after it, until the guest acknowledges it, a receive or a send.
    let early = [connection.acknowledge(), connection.send(b"early")];
    stream.supply(&broker.take_for_guest());
    assert_eq!(connection.receive().unwrap(), Event::Reset);
    let unacknowledged = [connection.receive().map(drop), connection.send(b"early")];
    for refused in early.into_iter().chain(unacknowledged) {
        assert_eq!(refused.unwrap_err().kind(), ConnectionErrorKind::OutOfOrder);
    }
    assert_eq!(stream.unread(), 0);
    assert!(stream.take_output().is_empty());
    connection.acknowledge().unwrap();
    broker.accept_guest(&stream.take_output());

    // A host sends twice the guest's receive window, which the VM releases only as the guest
    // receives it.
    let mut host = attach_host(&mut broker);
    let sent = pattern(2 * OUTER_RECEIVE_WINDOW as usize);
    broker.accept_host(&host_data(&mut host, &sent));
    let mut received = Vec::new();
    while received.len() < sent.len() {
        let input = broker.take_for_guest();
        assert!(!input.is_empty(), "the VM releases what the guest received");
        stream.supply(&input);
        while stream.unread() != 0 {
            let Event::Data(bytes) = connection.receive().unwrap() else {
                panic!("only bytes follow the acknowledgement");
            };
            received.extend(bytes);
        }
        broker.accept_guest(&stream.take_output());
    }
    assert_eq!(received, sent);
    assert_eq!(broker.held(), 0);

    // The guest's bytes reach the host in order.
    connection.send(&received).unwrap();
    broker.accept_guest(&stream.take_output());
    let from_broker = broker.take_for_host();
    let mut rest = from_broker.as_slice();
    let mut echoed = Vec::new();
    while !rest.is_empty() {
        let (consumed, event) = host.receive(rest).unwrap();
        rest = &rest[consumed..];
        let Some(HostEvent::Data(payload)) = event else {
            panic!("the host receives only the guest's bytes: {event:?}");
        };
        echoed.extend(payload);
    }
    assert_eq!(echoed, sent);

    // Losing the host resets the session once more, which the guest acknowledges once.
    broker.disconnect_host();
    stream.supply(&broker.take_for_guest());
    assert_eq!(connection.receive().unwrap(), Event::Reset);
    connection.acknowledge().unwrap();
    let again = connection.acknowledge().unwrap_err();
    assert_eq!(again.kind(), ConnectionErrorKind::OutOfOrder);
    broker.accept_guest(&stream.take_output());

    // The next host's bytes follow.
    let mut host = attach_host(&mut broker);
    broker.accept_host(&host_data(&mut host, b"again"));
    stream.supply(&broker.take_for_guest());
    assert_eq!(
        connection.receive().unwrap(),
        Event::Data(b"again".to_vec())
    );
}

#[test]
fn failures_end_the_connection_with_their_kinds() {
    // The VM closes the stream between records.
    let stream = Stream::default();
    let mut connection = Connection::attach(stream.clone()).unwrap();
    stream.supply(&record(RecordType::Reset, IDENTITY, 0, &[]));
    assert_eq!(connection.receive().unwrap(), Event::Reset);
    connection.acknowledge().unwrap();
    let closed = connection.receive().unwrap_err();
    assert_eq!(closed.kind(), ConnectionErrorKind::Closed);
    let ended = connection.send(b"x").unwrap_err();
    assert_eq!(ended.kind(), ConnectionErrorKind::Ended);

    // The stream ends inside a record.
    let stream = Stream::default();
    let mut connection = Connection::attach(stream.clone()).unwrap();
    stream.supply(&record(RecordType::Reset, IDENTITY, 0, &[])[..20]);
    let truncated = connection.receive().unwrap_err();
    assert_eq!(truncated.kind(), ConnectionErrorKind::Closed);
    assert!(truncated.source().is_some());

    // The VM sends bytes before it resets the session.
    let stream = Stream::default();
    let mut connection = Connection::attach(stream.clone()).unwrap();
    stream.supply(&record(RecordType::Data, IDENTITY, 0, b"x"));
    let unexpected = connection.receive().unwrap_err();
    assert_eq!(unexpected.kind(), ConnectionErrorKind::Protocol);
    assert!(unexpected.source().is_some());
    let ended = connection.receive().unwrap_err();
    assert_eq!(ended.kind(), ConnectionErrorKind::Ended);

    // The stream fails to write the attachment.
    let stream = Stream::default();
    stream.break_writes();
    let failed = Connection::attach(stream).unwrap_err();
    assert_eq!(failed.kind(), ConnectionErrorKind::Io);
    assert!(failed.source().is_some());
}
