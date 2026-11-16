//! The host's and the guest's roles exchange messages of the tests' own framing through the
//! broker: messages that span records, partial reads of every size, the guest's receive window,
//! a reset in the middle of a message, and a refused host.

use super::broker::{Broker, CAPABILITY, IDENTITY};
use super::{HostState, Reassembler, framed, messages};
use crate::protocol::nvxs::{
    ErrorCode, GuestEvent, GuestSession, GuestState, HostEvent, HostSession,
};
use crate::protocol::{
    CAPABILITY_LEN, OUTER_HEADER_LEN, OUTER_MAX_PAYLOAD, OUTER_MIN_RECEIVE_WINDOW,
};

/// The broker, both roles, and the applications above them.
struct Ends {
    /// Broker between the roles.
    broker: Broker,
    /// The host's role.
    host: HostSession,
    /// The guest's role.
    guest: GuestSession,
    /// Receiver of the host's application.
    host_application: Reassembler,
    /// Receiver of the guest's application.
    guest_application: Reassembler,
    /// Messages that the host's application received.
    host_messages: Vec<Vec<u8>>,
    /// Messages that the guest's application received.
    guest_messages: Vec<Vec<u8>>,
    /// Events of the host's role other than DATA.
    host_events: Vec<HostEvent>,
    /// Bytes that each role takes from the stream at a time.
    piece: usize,
    /// Whether the guest's application echoes each message.
    echo: bool,
}

impl Ends {
    /// Attaches the guest, which acknowledges the broker's reset with the smallest window.
    fn new(piece: usize, echo: bool) -> Self {
        let mut ends = Self {
            broker: Broker::new(),
            host: HostSession::new(),
            guest: GuestSession::new(),
            host_application: Reassembler::default(),
            guest_application: Reassembler::default(),
            host_messages: Vec::new(),
            guest_messages: Vec::new(),
            host_events: Vec::new(),
            piece,
            echo,
        };
        let mut output = Vec::new();
        ends.guest.attach(&mut output).unwrap();
        ends.broker.accept_guest(&output);
        ends.pump();
        assert_eq!(ends.guest.state(), GuestState::Active);
        ends
    }

    /// Attaches the guest, then a host.
    fn attached(piece: usize, echo: bool) -> Self {
        let mut ends = Self::new(piece, echo);
        ends.connect_host(&CAPABILITY);
        assert_eq!(ends.host.state(), HostState::Attached);
        ends
    }

    /// Connects a new host, which attaches with `capability`.
    fn connect_host(&mut self, capability: &[u8; CAPABILITY_LEN]) {
        self.broker.connect_host();
        self.host = HostSession::new();
        self.host_events.clear();
        let mut output = Vec::new();
        self.host.attach(capability, &mut output).unwrap();
        self.broker.accept_host(&output);
        self.pump();
    }

    /// The host's application sends `message`.
    fn host_sends(&mut self, message: &[u8]) {
        let data = framed(message);
        let mut output = Vec::new();
        let mut sent = 0;
        while sent < data.len() {
            sent += self.host.send(&data[sent..], &mut output).unwrap();
        }
        self.broker.accept_host(&output);
    }

    /// Relays bytes between the broker and the roles until neither has any to deliver.
    fn pump(&mut self) {
        loop {
            let to_guest = self.broker.take_for_guest();
            let to_host = self.broker.take_for_host();
            if to_guest.is_empty() && to_host.is_empty() {
                return;
            }
            for piece in to_guest.chunks(self.piece) {
                self.guest_receives(piece);
            }
            for piece in to_host.chunks(self.piece) {
                self.host_receives(piece);
            }
        }
    }

    /// The guest's role takes `piece`, and its application handles the events.
    fn guest_receives(&mut self, mut piece: &[u8]) {
        while !piece.is_empty() {
            let (consumed, event) = self.guest.receive(piece).unwrap();
            piece = &piece[consumed..];
            let mut output = Vec::new();
            match event {
                None => {}
                Some(GuestEvent::Reset(_)) => {
                    // The application abandons the earlier epoch's message in progress.
                    self.guest_application = Reassembler::default();
                    self.guest
                        .acknowledge(OUTER_MIN_RECEIVE_WINDOW, &mut output)
                        .unwrap();
                }
                Some(GuestEvent::Data(payload)) => {
                    let messages = self.guest_application.push(&payload);
                    // The application consumed the payload, so its bytes return to the window.
                    self.guest
                        .credit(payload.len() as u32, &mut output)
                        .unwrap();
                    for message in messages {
                        if self.echo {
                            let data = framed(&message);
                            let mut sent = 0;
                            while sent < data.len() {
                                sent += self.guest.send(&data[sent..], &mut output).unwrap();
                            }
                        }
                        self.guest_messages.push(message);
                    }
                }
            }
            self.broker.accept_guest(&output);
        }
    }

    /// The host's role takes `piece`, and its application handles the events.
    fn host_receives(&mut self, mut piece: &[u8]) {
        while !piece.is_empty() {
            let (consumed, event) = self.host.receive(piece).unwrap();
            piece = &piece[consumed..];
            match event {
                None => {}
                Some(HostEvent::Data(payload)) => {
                    let messages = self.host_application.push(&payload);
                    self.host_messages.extend(messages);
                }
                Some(event) => self.host_events.push(event),
            }
        }
    }
}

#[test]
fn messages_of_an_independent_framing_span_records_and_survive_partial_reads() {
    for piece in [1, 7, OUTER_HEADER_LEN, OUTER_HEADER_LEN + 1, 1000, 70_000] {
        let mut ends = Ends::attached(piece, true);
        assert_eq!(ends.host_events, [HostEvent::Attached(IDENTITY)]);
        for message in messages() {
            ends.host_sends(&message);
        }
        ends.pump();
        assert_eq!(ends.guest_messages, messages(), "pieces of {piece} bytes");
        assert_eq!(ends.host_messages, messages(), "pieces of {piece} bytes");
        // The guest returned every byte to its window, and no DATA waits for it.
        assert_eq!((ends.guest.outstanding(), ends.broker.held()), (0, 0));
        assert!(!ends.guest_application.is_partial());
        assert!(!ends.host_application.is_partial());
    }
}

#[test]
fn a_reset_abandons_the_message_that_the_host_left_unfinished() {
    let mut ends = Ends::attached(1000, false);
    let unfinished = vec![0x11; 200_000];
    ends.host_sends(&unfinished);

    // The guest's window of 64 KiB lets one record through, and the broker holds the rest.
    let first = ends.broker.take_for_guest();
    assert_eq!(first.len(), OUTER_HEADER_LEN + OUTER_MAX_PAYLOAD);
    let held = framed(&unfinished).len() - OUTER_MAX_PAYLOAD;
    assert_eq!(ends.broker.held(), held);
    let (_, event) = ends.guest.receive(&first).unwrap();
    let Some(GuestEvent::Data(payload)) = event else {
        panic!("expected DATA, got {event:?}");
    };
    assert!(ends.guest_application.push(&payload).is_empty());
    assert!(ends.guest_application.is_partial());

    // The host's connection drops before the guest credits the record, so the broker drops the
    // held DATA and resets the guest's session. A new host attaches before the guest
    // acknowledges the reset, so it waits until the guest does.
    ends.broker.disconnect_host();
    assert_eq!(ends.broker.held(), 0);
    ends.connect_host(&CAPABILITY);
    let identity = ends.broker.identity();
    assert_eq!(identity.epoch, 2);
    assert_eq!(
        ends.host_events,
        [HostEvent::Wait, HostEvent::Attached(identity)]
    );
    assert_eq!(ends.guest.identity(), Some(identity));
    assert_eq!(ends.guest.outstanding(), 0);
    assert!(!ends.guest_application.is_partial());

    // A new message arrives whole, and the unfinished one never does.
    let fresh = vec![0x22; 100_000];
    ends.host_sends(&fresh);
    ends.pump();
    assert_eq!(ends.guest_messages, [fresh]);
}

#[test]
fn the_broker_may_refuse_the_hosts_capability() {
    let mut ends = Ends::new(1000, false);
    ends.connect_host(&[0; CAPABILITY_LEN]);
    assert_eq!(
        ends.host_events,
        [HostEvent::Rejected(ErrorCode::Authentication)]
    );
    assert_eq!(ends.host.state(), HostState::Rejected);
    // The guest's session stays as it was.
    assert_eq!(ends.guest.state(), GuestState::Active);
}
