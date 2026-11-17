//! The guest's role: attachment, reset and acknowledgement, the records that each state allows,
//! sequence numbers and identities, the receive window and credits, a reset in the middle of a
//! message, and disconnection.

use super::*;

/// Returns a guest session that sent its attachment.
fn attaching_guest() -> GuestSession {
    let mut guest = GuestSession::new();
    guest.attach(&mut Vec::new()).unwrap();
    guest
}

/// Returns a guest session that acknowledged the broker's RESET of [`IDENTITY`] at sequence 0
/// with `window`, and so expects the broker's next record at sequence 1.
fn active_guest(window: u32) -> GuestSession {
    let mut guest = attaching_guest();
    let reset = record(RecordType::Reset, IDENTITY, 0, &[]);
    let event = Some(GuestEvent::Reset(IDENTITY));
    assert_eq!(guest.receive(&reset), Ok((reset.len(), event)));
    guest.acknowledge(window, &mut Vec::new()).unwrap();
    guest
}

/// Receives one complete DATA record and returns its payload.
fn receive_data(guest: &mut GuestSession, encoded: &[u8]) -> Vec<u8> {
    match guest.receive(encoded) {
        Ok((consumed, Some(GuestEvent::Data(payload)))) if consumed == encoded.len() => payload,
        other => panic!("expected one DATA record: {other:?}"),
    }
}

#[test]
fn the_guest_attaches_acknowledges_receives_and_credits() {
    let mut guest = GuestSession::new();
    assert_eq!(guest.state(), GuestState::Detached);
    let mut output = Vec::new();
    guest.attach(&mut output).unwrap();
    assert_eq!(output, openvmm_vector("GUEST_ATTACH"));
    assert_eq!(guest.state(), GuestState::Attaching);

    // A RESET may carry any sequence number, and the broker's next record follows it.
    let reset = record(RecordType::Reset, IDENTITY, 7, &[]);
    let event = Some(GuestEvent::Reset(IDENTITY));
    assert_eq!(guest.receive(&reset), Ok((reset.len(), event)));
    assert_eq!(
        (guest.state(), guest.identity(), guest.window()),
        (GuestState::Resetting, Some(IDENTITY), None)
    );
    assert_eq!(
        guest.send(b"x", &mut output),
        Err(SessionError::InvalidState)
    );
    assert_eq!(
        guest.credit(1, &mut output),
        Err(SessionError::InvalidState)
    );

    output.clear();
    guest
        .acknowledge(OUTER_RECEIVE_WINDOW, &mut output)
        .unwrap();
    let ack = &records(&output)[0];
    assert_eq!(
        (
            ack.record_type(),
            ack.instance_id(),
            ack.epoch(),
            ack.sequence(),
            ack.window()
        ),
        (RecordType::Ack, INSTANCE, 1, 0, Some(OUTER_RECEIVE_WINDOW))
    );
    assert_eq!(
        (guest.state(), guest.window(), guest.outstanding()),
        (GuestState::Active, Some(OUTER_RECEIVE_WINDOW), 0)
    );

    let data = record(RecordType::Data, IDENTITY, 8, b"request");
    assert_eq!(receive_data(&mut guest, &data), b"request");
    assert_eq!(guest.outstanding(), 7);

    // ACK, CREDIT, and the guest's DATA share the guest's sequence numbers.
    output.clear();
    guest.credit(7, &mut output).unwrap();
    assert_eq!(guest.send(b"response", &mut output), Ok(8));
    let sent = records(&output);
    let kinds: Vec<_> = sent
        .iter()
        .map(|record| (record.record_type(), record.sequence(), record.epoch()))
        .collect();
    assert_eq!(
        kinds,
        [(RecordType::Credit, 1, 1), (RecordType::Data, 2, 1)]
    );
    assert_eq!(sent[0].credit(), Some(7));
    assert_eq!(sent[1].payload(), b"response");
    assert_eq!(guest.outstanding(), 0);
}

#[test]
fn the_guest_takes_data_only_in_an_acknowledged_epoch() {
    // Before the first RESET.
    let mut guest = attaching_guest();
    let data = record(RecordType::Data, IDENTITY, 0, b"x");
    let expected = Err(SessionError::Unexpected(RecordType::Data));
    assert_eq!(guest.receive(&data), expected.clone());
    assert_eq!(guest.state(), GuestState::Failed);

    // Between a RESET and its acknowledgement.
    let mut guest = attaching_guest();
    guest
        .receive(&record(RecordType::Reset, IDENTITY, 0, &[]))
        .unwrap();
    let data = record(RecordType::Data, IDENTITY, 1, b"x");
    assert_eq!(guest.receive(&data), expected);
    assert_eq!(guest.state(), GuestState::Failed);

    // Records that the broker never sends to the guest.
    use RecordType::*;
    for record_type in [GuestAttach, HostAttach, Ack, Wait, Ready, Error, Credit] {
        let mut guest = attaching_guest();
        let expected = Err(SessionError::Unexpected(record_type));
        assert_eq!(guest.receive(&sample(record_type, IDENTITY, 0)), expected);
        let mut guest = active_guest(OUTER_RECEIVE_WINDOW);
        assert_eq!(guest.receive(&sample(record_type, IDENTITY, 1)), expected);
        assert_eq!(guest.state(), GuestState::Failed);
    }
}

#[test]
fn the_guest_checks_the_identity_of_each_reset() {
    for identity in [
        Identity {
            instance_id: [0; 16],
            epoch: 1,
        },
        epoch(0),
    ] {
        let mut guest = attaching_guest();
        let reset = record(RecordType::Reset, identity, 0, &[]);
        let expected = SessionError::MissingIdentity(RecordType::Reset);
        assert_eq!(guest.receive(&reset), Err(expected));
        assert_eq!(
            (guest.state(), guest.identity()),
            (GuestState::Failed, None)
        );
    }

    // The epoch of an instance only advances, before and after the acknowledgement.
    let mut guest = active_guest(OUTER_RECEIVE_WINDOW);
    let expected = SessionError::StaleEpoch {
        current: 1,
        actual: 1,
    };
    let reset = record(RecordType::Reset, IDENTITY, 1, &[]);
    assert_eq!(guest.receive(&reset), Err(expected));
    let mut guest = active_guest(OUTER_RECEIVE_WINDOW);
    let later = epoch(9);
    let reset = record(RecordType::Reset, later, 0, &[]);
    let event = Some(GuestEvent::Reset(later));
    assert_eq!(guest.receive(&reset), Ok((reset.len(), event)));
    let mut stale = guest.clone();
    let earlier = record(RecordType::Reset, epoch(8), 1, &[]);
    let expected = SessionError::StaleEpoch {
        current: 9,
        actual: 8,
    };
    assert_eq!(stale.receive(&earlier), Err(expected));

    // A new instance, such as a restored broker's, starts over at any epoch.
    let restored = Identity {
        instance_id: [0x33; 16],
        epoch: 1,
    };
    let reset = record(RecordType::Reset, restored, 0, &[]);
    let event = Some(GuestEvent::Reset(restored));
    assert_eq!(guest.receive(&reset), Ok((reset.len(), event)));
    let mut output = Vec::new();
    guest
        .acknowledge(OUTER_RECEIVE_WINDOW, &mut output)
        .unwrap();
    let ack = &records(&output)[0];
    assert_eq!(
        (ack.instance_id(), ack.epoch(), ack.sequence()),
        ([0x33; 16], 1, 0)
    );

    // A RESET at the last sequence number leaves none for the broker's next record.
    let mut guest = attaching_guest();
    let reset = record(RecordType::Reset, IDENTITY, u64::MAX, &[]);
    let expected = SessionError::SequenceOverflow(RecordType::Reset);
    assert_eq!(guest.receive(&reset), Err(expected));
    assert_eq!(guest.state(), GuestState::Failed);
}

#[test]
fn the_guest_checks_the_sequence_and_identity_of_data() {
    let other = [0x33; 16];
    let cases = [
        (
            record(RecordType::Data, IDENTITY, 2, b"x"),
            SessionError::Sequence {
                expected: 1,
                actual: 2,
            },
        ),
        (
            record(RecordType::Data, IDENTITY, 0, b"x"),
            SessionError::Sequence {
                expected: 1,
                actual: 0,
            },
        ),
        (
            record(
                RecordType::Data,
                Identity {
                    instance_id: other,
                    ..IDENTITY
                },
                1,
                b"x",
            ),
            SessionError::Instance {
                expected: INSTANCE,
                actual: other,
            },
        ),
        (
            record(RecordType::Data, epoch(2), 1, b"x"),
            SessionError::Epoch {
                expected: 1,
                actual: 2,
            },
        ),
    ];
    for (encoded, expected) in cases {
        let mut guest = active_guest(OUTER_RECEIVE_WINDOW);
        assert_eq!(guest.receive(&encoded), Err(expected));
        assert_eq!(guest.state(), GuestState::Failed);
    }

    // A duplicate of the previous DATA record.
    let mut guest = active_guest(OUTER_RECEIVE_WINDOW);
    let data = record(RecordType::Data, IDENTITY, 1, b"x");
    receive_data(&mut guest, &data);
    let expected = SessionError::Sequence {
        expected: 2,
        actual: 1,
    };
    assert_eq!(guest.receive(&data), Err(expected));

    // After a RESET at the second-to-last sequence number, DATA at the last one leaves none for
    // the broker's next record.
    let mut guest = attaching_guest();
    let reset = record(RecordType::Reset, IDENTITY, u64::MAX - 1, &[]);
    let event = Some(GuestEvent::Reset(IDENTITY));
    assert_eq!(guest.receive(&reset), Ok((reset.len(), event)));
    guest
        .acknowledge(OUTER_RECEIVE_WINDOW, &mut Vec::new())
        .unwrap();
    let last = record(RecordType::Data, IDENTITY, u64::MAX, b"x");
    let expected = SessionError::SequenceOverflow(RecordType::Data);
    assert_eq!(guest.receive(&last), Err(expected));
    assert_eq!(guest.state(), GuestState::Failed);
}

#[test]
fn the_guest_enforces_its_receive_window() {
    let mut guest = active_guest(OUTER_MIN_RECEIVE_WINDOW);
    let full = record(RecordType::Data, IDENTITY, 1, &[0; OUTER_MAX_PAYLOAD]);
    assert_eq!(receive_data(&mut guest, &full).len(), OUTER_MAX_PAYLOAD);
    assert_eq!(guest.outstanding(), OUTER_MIN_RECEIVE_WINDOW);

    // A record beyond the window fails the session until the guest returns credit for it.
    let one = record(RecordType::Data, IDENTITY, 2, b"x");
    let mut over = guest.clone();
    let expected = SessionError::Window {
        window: OUTER_MIN_RECEIVE_WINDOW,
        outstanding: OUTER_MIN_RECEIVE_WINDOW,
        length: 1,
    };
    assert_eq!(over.receive(&one), Err(expected.clone()));
    assert_eq!(over.state(), GuestState::Failed);
    guest.credit(1, &mut Vec::new()).unwrap();
    assert_eq!(receive_data(&mut guest, &one), b"x");
    assert_eq!(guest.outstanding(), OUTER_MIN_RECEIVE_WINDOW);
    let next = record(RecordType::Data, IDENTITY, 3, b"y");
    assert_eq!(guest.receive(&next), Err(expected));
}

#[test]
fn the_guest_credits_only_bytes_that_it_received() {
    let mut guest = active_guest(OUTER_RECEIVE_WINDOW);
    let mut output = Vec::new();
    let expected = SessionError::Credit {
        outstanding: 0,
        bytes: 1,
    };
    assert_eq!(guest.credit(1, &mut output), Err(expected));
    receive_data(&mut guest, &record(RecordType::Data, IDENTITY, 1, &[0; 10]));
    for bytes in [0, 11, u32::MAX] {
        let expected = SessionError::Credit {
            outstanding: 10,
            bytes,
        };
        assert_eq!(guest.credit(bytes, &mut output), Err(expected));
    }
    // A refused credit changes nothing.
    assert!(output.is_empty());
    assert_eq!(
        (guest.state(), guest.outstanding()),
        (GuestState::Active, 10)
    );
    guest.credit(4, &mut output).unwrap();
    guest.credit(6, &mut output).unwrap();
    let credits: Vec<_> = records(&output)
        .iter()
        .map(|record| (record.sequence(), record.credit()))
        .collect();
    assert_eq!(credits, [(1, Some(4)), (2, Some(6))]);
    assert_eq!(guest.outstanding(), 0);
}

#[test]
fn the_guest_acknowledges_with_a_valid_window_once() {
    let mut guest = attaching_guest();
    let mut output = Vec::new();
    let expected = Err(SessionError::InvalidState);
    assert_eq!(
        guest.acknowledge(OUTER_RECEIVE_WINDOW, &mut output),
        expected
    );
    guest
        .receive(&record(RecordType::Reset, IDENTITY, 0, &[]))
        .unwrap();
    for window in [
        0,
        OUTER_MIN_RECEIVE_WINDOW - 1,
        OUTER_MAX_RECEIVE_WINDOW + 1,
    ] {
        let expected = SessionError::Record(RecordError::ReceiveWindow(window));
        assert_eq!(guest.acknowledge(window, &mut output), Err(expected));
        assert_eq!(guest.state(), GuestState::Resetting);
    }
    assert!(output.is_empty());
    guest
        .acknowledge(OUTER_MAX_RECEIVE_WINDOW, &mut output)
        .unwrap();
    let again = guest.acknowledge(OUTER_MAX_RECEIVE_WINDOW, &mut output);
    assert_eq!(again, Err(SessionError::InvalidState));
    assert_eq!(guest.window(), Some(OUTER_MAX_RECEIVE_WINDOW));
}

#[test]
fn a_reset_in_the_middle_of_a_message_abandons_it() {
    let mut guest = active_guest(OUTER_MAX_RECEIVE_WINDOW);
    let message = framed(&vec![0xab; 150_000]);
    let mut application = Reassembler::default();

    // The first of the message's three records arrives.
    let first = record(RecordType::Data, IDENTITY, 1, &message[..OUTER_MAX_PAYLOAD]);
    let payload = receive_data(&mut guest, &first);
    assert!(application.push(&payload).is_empty());
    assert_eq!(guest.outstanding(), OUTER_MAX_PAYLOAD as u32);

    // The broker resets the session before the rest arrives, so the application abandons the
    // message, and the outstanding bytes of the old epoch need no credit.
    let next = epoch(2);
    let reset = record(RecordType::Reset, next, 0, &[]);
    let event = Some(GuestEvent::Reset(next));
    assert_eq!(guest.receive(&reset), Ok((reset.len(), event)));
    application = Reassembler::default();
    assert_eq!(guest.outstanding(), 0);
    let expected = SessionError::InvalidState;
    assert_eq!(guest.credit(1, &mut Vec::new()), Err(expected));
    guest
        .acknowledge(OUTER_RECEIVE_WINDOW, &mut Vec::new())
        .unwrap();

    // The rest of the old epoch's message would fail the session.
    let rest = &message[OUTER_MAX_PAYLOAD..2 * OUTER_MAX_PAYLOAD];
    let mut stale = guest.clone();
    let expected = SessionError::Epoch {
        expected: 2,
        actual: 1,
    };
    assert_eq!(
        stale.receive(&record(RecordType::Data, IDENTITY, 1, rest)),
        Err(expected)
    );

    // A new message spans two records of the new epoch, split anywhere.
    let reply = framed(&vec![0xcd; 70_000]);
    let stream = [
        record(RecordType::Data, next, 1, &reply[..OUTER_MAX_PAYLOAD]),
        record(RecordType::Data, next, 2, &reply[OUTER_MAX_PAYLOAD..]),
    ]
    .concat();
    let events = feed(|piece| guest.receive(piece), &stream, &[3, 50, 65_600]).unwrap();
    let mut messages = Vec::new();
    for event in events {
        let GuestEvent::Data(payload) = event else {
            panic!("only DATA follows the acknowledgement");
        };
        messages.extend(application.push(&payload));
    }
    assert_eq!(messages, [vec![0xcd; 70_000]]);
    assert_eq!(guest.outstanding(), reply.len() as u32);
}

#[test]
fn the_guest_session_closes_only_at_a_record_boundary() {
    let mut guest = active_guest(OUTER_RECEIVE_WINDOW);
    assert_eq!(guest.disconnect(), Ok(()));
    assert_eq!(guest.state(), GuestState::Closed);
    let data = record(RecordType::Data, IDENTITY, 1, b"after");
    assert_eq!(guest.receive(&data), Err(SessionError::InvalidState));
    assert_eq!(
        guest.send(b"x", &mut Vec::new()),
        Err(SessionError::InvalidState)
    );

    // A stream that ends inside a record, at any byte, fails the session.
    let data = record(RecordType::Data, IDENTITY, 1, b"partial");
    for cut in 1..data.len() {
        let mut guest = active_guest(OUTER_RECEIVE_WINDOW);
        assert_eq!(guest.receive(&data[..cut]), Ok((cut, None)));
        let expected = SessionError::Truncated { buffered: cut };
        assert_eq!(guest.disconnect(), Err(expected), "after {cut} bytes");
        assert_eq!(guest.state(), GuestState::Failed);
    }
    // Including a RESET that the stream cuts off.
    let mut guest = attaching_guest();
    let reset = record(RecordType::Reset, IDENTITY, 0, &[]);
    assert_eq!(guest.receive(&reset[..20]), Ok((20, None)));
    let expected = SessionError::Truncated { buffered: 20 };
    assert_eq!(guest.disconnect(), Err(expected));
}

#[test]
fn the_guest_reports_the_same_events_for_every_split_of_its_stream() {
    let next = epoch(2);
    let stream = [
        record(RecordType::Reset, IDENTITY, 0, &[]),
        record(RecordType::Data, IDENTITY, 1, b"x"),
        record(RecordType::Data, IDENTITY, 2, &[0x5a; 300]),
        record(RecordType::Reset, next, 0, &[]),
        record(RecordType::Data, next, 1, b"NVXS inside a payload"),
    ]
    .concat();
    // The guest acknowledges each reset as soon as it arrives.
    let run = |cuts: &[usize]| {
        let mut guest = attaching_guest();
        let events = feed(
            |piece| {
                let (consumed, event) = guest.receive(piece)?;
                if let Some(GuestEvent::Reset(_)) = event {
                    guest.acknowledge(OUTER_RECEIVE_WINDOW, &mut Vec::new())?;
                }
                Ok((consumed, event))
            },
            &stream,
            cuts,
        )
        .unwrap();
        (events, guest.outstanding())
    };
    let expected = run(&[]);
    assert_eq!(expected.0.len(), 5);
    assert_eq!(expected.1, 21);
    for cut in 0..=stream.len() {
        assert_eq!(run(&[cut]), expected, "split at {cut}");
    }
    let cuts: Vec<usize> = (1..stream.len()).collect();
    assert_eq!(run(&cuts), expected, "one byte at a time");
}
