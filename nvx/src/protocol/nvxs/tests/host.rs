//! The host's role: attachment, refusal, the records that each state allows, sequence numbers and
//! identities, reset, disconnection, and long messages.

use super::*;
use crate::tests::{NVXC_V1, vector};

/// Returns a host session that sent its attachment.
fn attaching_host() -> HostSession {
    let mut host = HostSession::new();
    host.attach(&[1; CAPABILITY_LEN], &mut Vec::new()).unwrap();
    host
}

/// Returns a host session that the broker attached at its sequence 0 with `identity`.
fn attached_host_at(identity: Identity) -> HostSession {
    let mut host = attaching_host();
    let ready = record(RecordType::Ready, identity, 0, &[]);
    let attached = Some(HostEvent::Attached(identity));
    assert_eq!(host.receive(&ready), Ok((ready.len(), attached)));
    host
}

/// Returns a host session that the broker attached at its sequence 0 with [`IDENTITY`].
fn attached_host() -> HostSession {
    attached_host_at(IDENTITY)
}

#[test]
fn the_host_attaches_after_waiting_and_exchanges_data() {
    let mut host = HostSession::new();
    assert_eq!((host.state(), host.identity()), (HostState::Detached, None));
    // The attachment of the control protocol's conformance vectors, whose payload is the
    // capability.
    let attachment = vector(NVXC_V1, "RECORD", "HOST_ATTACH").bytes;
    let capability: [u8; CAPABILITY_LEN] = attachment[OUTER_HEADER_LEN..].try_into().unwrap();
    let mut output = Vec::new();
    host.attach(&capability, &mut output).unwrap();
    assert_eq!(output, attachment);
    assert_eq!(host.state(), HostState::Attaching);
    assert_eq!(
        host.send(b"early", &mut output),
        Err(SessionError::InvalidState)
    );

    let stream = [
        record(RecordType::Wait, IDENTITY, 0, &[]),
        record(RecordType::Ready, IDENTITY, 1, &[]),
        record(RecordType::Data, IDENTITY, 2, b"from the guest"),
    ]
    .concat();
    let events = feed(|piece| host.receive(piece), &stream, &[]).unwrap();
    assert_eq!(
        events,
        [
            HostEvent::Wait,
            HostEvent::Attached(IDENTITY),
            HostEvent::Data(b"from the guest".to_vec()),
        ]
    );
    assert_eq!(
        (host.state(), host.identity()),
        (HostState::Attached, Some(IDENTITY))
    );

    // The host's DATA carries the identity and its own sequence numbers, from 0.
    output.clear();
    assert_eq!(host.send(b"hello", &mut output), Ok(5));
    assert_eq!(host.send(b"again", &mut output), Ok(5));
    let sent: Vec<_> = records(&output)
        .into_iter()
        .map(|record| {
            (
                record.record_type(),
                record.instance_id(),
                record.epoch(),
                record.sequence(),
                record.into_payload(),
            )
        })
        .collect();
    assert_eq!(
        sent,
        [
            (RecordType::Data, INSTANCE, 1, 0, b"hello".to_vec()),
            (RecordType::Data, INSTANCE, 1, 1, b"again".to_vec()),
        ]
    );
}

#[test]
fn the_host_reports_a_refused_capability() {
    let mut host = attaching_host();
    // A refusal may follow WAIT, at the broker's next sequence number.
    let stream = [
        record(RecordType::Wait, IDENTITY, 0, &[]),
        record(RecordType::Error, IDENTITY, 1, &1u32.to_le_bytes()),
    ]
    .concat();
    let events = feed(|piece| host.receive(piece), &stream, &[]).unwrap();
    let rejected = HostEvent::Rejected(ErrorCode::Authentication);
    assert_eq!(events, [HostEvent::Wait, rejected.clone()]);
    assert_eq!((host.state(), host.identity()), (HostState::Rejected, None));
    let ready = record(RecordType::Ready, IDENTITY, 2, &[]);
    assert_eq!(host.receive(&ready), Err(SessionError::InvalidState));
    assert_eq!(
        host.send(b"x", &mut Vec::new()),
        Err(SessionError::InvalidState)
    );
    assert_eq!(host.disconnect(), Ok(()));
    assert_eq!(host.state(), HostState::Rejected);

    // The refusal carries no identity of its own.
    let mut host = attaching_host();
    let error = record(RecordType::Error, NONE, 0, &1u32.to_le_bytes());
    assert_eq!(host.receive(&error), Ok((error.len(), Some(rejected))));
    // An ERROR record without its code is malformed.
    let mut host = attaching_host();
    let error = raw_header(RecordType::Error.code(), NONE, 0, 0);
    let expected = SessionError::Record(RecordError::Length {
        record_type: RecordType::Error,
        length: 0,
    });
    assert_eq!(host.receive(&error), Err(expected));
    assert_eq!(host.state(), HostState::Failed);
}

#[test]
fn the_host_refuses_records_that_its_state_does_not_allow() {
    use RecordType::*;
    for record_type in [GuestAttach, HostAttach, Reset, Ack, Data, Credit] {
        let mut host = attaching_host();
        let encoded = sample(record_type, IDENTITY, 0);
        let expected = Err(SessionError::Unexpected(record_type));
        assert_eq!(host.receive(&encoded), expected, "{record_type}");
        assert_eq!(host.state(), HostState::Failed);
        assert_eq!(host.receive(&encoded), Err(SessionError::InvalidState));
    }
    for record_type in [GuestAttach, HostAttach, Ack, Wait, Ready, Error, Credit] {
        let mut host = attached_host();
        let encoded = sample(record_type, IDENTITY, 1);
        let expected = Err(SessionError::Unexpected(record_type));
        assert_eq!(host.receive(&encoded), expected, "{record_type}");
        assert_eq!(host.state(), HostState::Failed);
    }
}

#[test]
fn the_host_checks_the_brokers_sequence_numbers() {
    // The broker numbers its records to the host from 0.
    let mut host = attaching_host();
    let wait = record(RecordType::Wait, IDENTITY, 1, &[]);
    let expected = SessionError::Sequence {
        expected: 0,
        actual: 1,
    };
    assert_eq!(host.receive(&wait), Err(expected));
    assert_eq!(host.state(), HostState::Failed);

    // A READY that repeats WAIT's sequence number.
    let mut host = attaching_host();
    let stream = [
        record(RecordType::Wait, IDENTITY, 0, &[]),
        record(RecordType::Ready, IDENTITY, 0, &[]),
    ]
    .concat();
    let expected = SessionError::Sequence {
        expected: 1,
        actual: 0,
    };
    assert_eq!(
        feed(|piece| host.receive(piece), &stream, &[]),
        Err(expected)
    );

    // DATA and RESET out of sequence after READY at 0.
    for (encoded, actual) in [
        (record(RecordType::Data, IDENTITY, 0, b"x"), 0),
        (record(RecordType::Data, IDENTITY, 2, b"x"), 2),
        (record(RecordType::Data, IDENTITY, u64::MAX, b"x"), u64::MAX),
        (record(RecordType::Reset, epoch(2), 0, &[]), 0),
    ] {
        let mut host = attached_host();
        let expected = SessionError::Sequence {
            expected: 1,
            actual,
        };
        assert_eq!(host.receive(&encoded), Err(expected));
        assert_eq!(host.state(), HostState::Failed);
    }

    // A duplicate of the previous DATA record.
    let mut host = attached_host();
    let data = record(RecordType::Data, IDENTITY, 1, b"x");
    host.receive(&data).unwrap();
    let expected = SessionError::Sequence {
        expected: 2,
        actual: 1,
    };
    assert_eq!(host.receive(&data), Err(expected));
}

#[test]
fn the_host_checks_the_sessions_identity() {
    for identity in [
        Identity {
            instance_id: [0; 16],
            epoch: 1,
        },
        epoch(0),
    ] {
        let mut host = attaching_host();
        let ready = record(RecordType::Ready, identity, 0, &[]);
        let expected = SessionError::MissingIdentity(RecordType::Ready);
        assert_eq!(host.receive(&ready), Err(expected));
        assert_eq!((host.state(), host.identity()), (HostState::Failed, None));
    }

    let other = [0x33; 16];
    let other_instance = Identity {
        instance_id: other,
        epoch: 2,
    };
    let instance_error = SessionError::Instance {
        expected: INSTANCE,
        actual: other,
    };
    let cases = [
        (
            record(RecordType::Data, other_instance, 1, b"x"),
            instance_error.clone(),
        ),
        (
            record(RecordType::Data, epoch(2), 1, b"x"),
            SessionError::Epoch {
                expected: 1,
                actual: 2,
            },
        ),
        (
            record(RecordType::Data, epoch(0), 1, b"x"),
            SessionError::Epoch {
                expected: 1,
                actual: 0,
            },
        ),
        (
            record(RecordType::Reset, other_instance, 1, &[]),
            instance_error,
        ),
        (
            record(RecordType::Reset, IDENTITY, 1, &[]),
            SessionError::StaleEpoch {
                current: 1,
                actual: 1,
            },
        ),
        (
            record(RecordType::Reset, epoch(0), 1, &[]),
            SessionError::MissingIdentity(RecordType::Reset),
        ),
    ];
    for (encoded, expected) in cases {
        let mut host = attached_host();
        assert_eq!(host.receive(&encoded), Err(expected));
        assert_eq!(host.state(), HostState::Failed);
    }

    // A reset never moves an instance's epoch back.
    let mut host = attached_host_at(epoch(3));
    let reset = record(RecordType::Reset, epoch(2), 1, &[]);
    let expected = SessionError::StaleEpoch {
        current: 3,
        actual: 2,
    };
    assert_eq!(host.receive(&reset), Err(expected));
}

#[test]
fn a_reset_ends_the_hosts_attachment() {
    let mut host = attached_host();
    let next = epoch(2);
    let stream = [
        record(RecordType::Data, IDENTITY, 1, b"x"),
        record(RecordType::Reset, next, 2, &[]),
    ]
    .concat();
    let events = feed(|piece| host.receive(piece), &stream, &[]).unwrap();
    assert_eq!(
        events,
        [HostEvent::Data(b"x".to_vec()), HostEvent::Reset(next)]
    );
    assert_eq!(
        (host.state(), host.identity()),
        (HostState::Reset, Some(IDENTITY))
    );
    assert_eq!(
        host.send(b"x", &mut Vec::new()),
        Err(SessionError::InvalidState)
    );
    let data = record(RecordType::Data, next, 3, b"x");
    assert_eq!(host.receive(&data), Err(SessionError::InvalidState));
}

#[test]
fn the_host_session_closes_only_at_a_record_boundary() {
    let mut host = attached_host();
    assert_eq!(host.disconnect(), Ok(()));
    assert_eq!(host.state(), HostState::Closed);
    let data = record(RecordType::Data, IDENTITY, 1, b"after");
    assert_eq!(host.receive(&data), Err(SessionError::InvalidState));
    assert_eq!(host.disconnect(), Ok(()));

    // A stream that ends inside a record, at any byte, fails the session.
    let data = record(RecordType::Data, IDENTITY, 1, b"partial");
    for cut in 1..data.len() {
        let mut host = attached_host();
        assert_eq!(host.receive(&data[..cut]), Ok((cut, None)));
        let expected = SessionError::Truncated { buffered: cut };
        assert_eq!(host.disconnect(), Err(expected), "after {cut} bytes");
        assert_eq!(host.state(), HostState::Failed);
    }
    let mut host = attaching_host();
    let ready = record(RecordType::Ready, IDENTITY, 0, &[]);
    assert_eq!(host.receive(&ready[..3]), Ok((3, None)));
    assert_eq!(
        host.disconnect(),
        Err(SessionError::Truncated { buffered: 3 })
    );
}

#[test]
fn the_host_sends_a_long_message_in_records_of_the_largest_payload() {
    let mut host = attached_host();
    let message: Vec<u8> = (0..200_000u32).map(|index| index as u8).collect();
    let mut output = Vec::new();
    let mut taken = Vec::new();
    let mut sent = 0;
    while sent < message.len() {
        let count = host.send(&message[sent..], &mut output).unwrap();
        taken.push(count);
        sent += count;
    }
    assert_eq!(
        taken,
        [
            OUTER_MAX_PAYLOAD,
            OUTER_MAX_PAYLOAD,
            OUTER_MAX_PAYLOAD,
            3392
        ]
    );
    let sent = records(&output);
    assert_eq!(
        sent.iter().map(Record::sequence).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert!(
        sent.iter()
            .all(|record| record.record_type() == RecordType::Data
                && record.instance_id() == INSTANCE
                && record.epoch() == 1)
    );
    let payloads: Vec<u8> = sent
        .iter()
        .flat_map(|record| record.payload().to_vec())
        .collect();
    assert_eq!(payloads, message);

    // Empty data encodes nothing and takes no sequence number.
    let before = output.len();
    let expected = SessionError::Record(RecordError::Length {
        record_type: RecordType::Data,
        length: 0,
    });
    assert_eq!(host.send(&[], &mut output), Err(expected));
    assert_eq!((output.len(), host.state()), (before, HostState::Attached));
    output.clear();
    host.send(b"next", &mut output).unwrap();
    assert_eq!(records(&output)[0].sequence(), 4);
}

#[test]
fn the_host_attaches_once_before_it_receives() {
    let mut host = HostSession::new();
    let ready = record(RecordType::Ready, IDENTITY, 0, &[]);
    assert_eq!(host.receive(&ready), Err(SessionError::InvalidState));
    assert_eq!(host.state(), HostState::Detached);
    let mut output = Vec::new();
    host.attach(&[1; CAPABILITY_LEN], &mut output).unwrap();
    let attached = output.len();
    let again = host.attach(&[1; CAPABILITY_LEN], &mut output);
    assert_eq!(again, Err(SessionError::InvalidState));
    assert_eq!(
        (host.state(), output.len()),
        (HostState::Attaching, attached)
    );
}

#[test]
fn the_host_reports_the_same_events_for_every_split_of_its_stream() {
    let stream = host_stream();
    let mut reference = attaching_host();
    let expected = feed(|piece| reference.receive(piece), &stream, &[]).unwrap();
    assert_eq!(expected.len(), 6);
    assert_eq!(reference.state(), HostState::Reset);
    for cut in 0..=stream.len() {
        let mut host = attaching_host();
        let events = feed(|piece| host.receive(piece), &stream, &[cut]).unwrap();
        assert_eq!(events, expected, "split at {cut}");
    }
    let cuts: Vec<usize> = (1..stream.len()).collect();
    let mut host = attaching_host();
    let events = feed(|piece| host.receive(piece), &stream, &cuts).unwrap();
    assert_eq!(events, expected, "one byte at a time");
}
