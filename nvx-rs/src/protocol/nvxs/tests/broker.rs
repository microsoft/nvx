//! A broker that relays NVXS records between one host and the guest as OpenVMM's broker does,
//! for the tests of the session and of its drivers.
//!
//! It resets the guest when the guest attaches and when an attached host's connection drops,
//! answers a host's attachment with WAIT until the guest acknowledges the reset, then READY, and
//! refuses a wrong capability with ERROR. It holds the host's DATA until the guest's receive
//! window has room, and numbers the records of each direction of each leg from 0. It panics on
//! any record that the protocol does not allow.

use std::collections::VecDeque;

use super::record;
use crate::protocol::CAPABILITY_LEN;
use crate::protocol::nvxs::{ErrorCode, Identity, Record, RecordParser, RecordType};

/// Instance ID of the test broker.
pub(crate) const INSTANCE: [u8; 16] = [0x5c; 16];
/// Identity of the first epoch of the test broker's session.
pub(crate) const IDENTITY: Identity = Identity {
    instance_id: INSTANCE,
    epoch: 1,
};
/// Launch capability that the test broker accepts.
pub(crate) const CAPABILITY: [u8; CAPABILITY_LEN] = [0xa5; CAPABILITY_LEN];

/// State of the guest's leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestLeg {
    /// The guest has not attached.
    Detached,
    /// The broker reset the guest's session and waits for the acknowledgement.
    Resetting,
    /// The guest acknowledged the epoch.
    Active,
}

/// State of the host's connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostLeg {
    /// The host connected and has not attached.
    Connected,
    /// The broker refused the host's capability.
    Refused,
    /// The host attached and waits for the guest.
    Waiting,
    /// The broker attached the host.
    Active,
}

/// Test broker of one session.
pub(crate) struct Broker {
    /// Epoch of the session.
    epoch: u64,
    /// State of the guest's leg.
    guest: GuestLeg,
    /// State of the host's connection, if one is open.
    host: Option<HostLeg>,
    /// Parser of the guest's records.
    guest_parser: RecordParser,
    /// Parser of the host's records.
    host_parser: RecordParser,
    /// Bytes for the guest.
    to_guest: Vec<u8>,
    /// Bytes for the host.
    to_host: Vec<u8>,
    /// Sequence number of the broker's next record to the guest.
    guest_send: u64,
    /// Sequence number of the guest's next record.
    guest_receive: u64,
    /// Sequence number of the broker's next record to the host.
    host_send: u64,
    /// Sequence number of the host's next record.
    host_receive: u64,
    /// Receive window that the guest acknowledged.
    window: u32,
    /// Bytes that the guest's window has room for.
    credit: u32,
    /// Payloads of the host's DATA that wait for the guest's window.
    held: VecDeque<Vec<u8>>,
}

impl Broker {
    /// Creates the broker of a session at epoch 1, before the guest attaches.
    pub(crate) fn new() -> Self {
        Self {
            epoch: 1,
            guest: GuestLeg::Detached,
            host: None,
            guest_parser: RecordParser::new(),
            host_parser: RecordParser::new(),
            to_guest: Vec::new(),
            to_host: Vec::new(),
            guest_send: 0,
            guest_receive: 0,
            host_send: 0,
            host_receive: 0,
            window: 0,
            credit: 0,
            held: VecDeque::new(),
        }
    }

    /// Returns the session's current identity.
    pub(crate) fn identity(&self) -> Identity {
        Identity {
            instance_id: INSTANCE,
            epoch: self.epoch,
        }
    }

    /// Returns the bytes of the host's DATA that wait for the guest's window.
    pub(crate) fn held(&self) -> usize {
        self.held.iter().map(Vec::len).sum()
    }

    /// Opens a host connection, which attaches first.
    pub(crate) fn connect_host(&mut self) {
        assert_eq!(self.host, None, "the broker serves one host");
        self.host = Some(HostLeg::Connected);
        self.host_parser = RecordParser::new();
        self.host_send = 0;
        self.host_receive = 0;
    }

    /// Drops the host's connection. Losing an attached host starts a new epoch of the guest's
    /// session: the broker drops the host's held DATA and resets the guest.
    pub(crate) fn disconnect_host(&mut self) {
        let attached = self.host == Some(HostLeg::Active);
        self.host = None;
        self.to_host.clear();
        if attached {
            self.epoch += 1;
            self.held.clear();
            self.window = 0;
            self.credit = 0;
            self.guest_send = 0;
            self.guest_receive = 0;
            self.guest = GuestLeg::Resetting;
            self.queue_guest(RecordType::Reset, &[]);
        }
    }

    /// Takes the bytes that the broker wrote to the guest.
    pub(crate) fn take_for_guest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.to_guest)
    }

    /// Takes the bytes that the broker wrote to the host.
    pub(crate) fn take_for_host(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.to_host)
    }

    /// Accepts bytes that the guest wrote.
    pub(crate) fn accept_guest(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let (consumed, record) = self.guest_parser.push(bytes).unwrap();
            bytes = &bytes[consumed..];
            if let Some(record) = record {
                self.guest_record(record);
            }
        }
    }

    /// Accepts bytes that the host wrote.
    pub(crate) fn accept_host(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let (consumed, record) = self.host_parser.push(bytes).unwrap();
            bytes = &bytes[consumed..];
            if let Some(record) = record {
                self.host_record(record);
            }
        }
    }

    /// Applies one record from the guest.
    fn guest_record(&mut self, record: Record) {
        match (self.guest, record.record_type()) {
            (GuestLeg::Detached, RecordType::GuestAttach) => {
                self.guest = GuestLeg::Resetting;
                self.queue_guest(RecordType::Reset, &[]);
            }
            (GuestLeg::Resetting, RecordType::Ack) => {
                self.check_guest(&record);
                self.window = record.window().unwrap();
                self.credit = self.window;
                self.guest = GuestLeg::Active;
                if self.host == Some(HostLeg::Waiting) {
                    self.queue_host(RecordType::Ready, &[]);
                    self.host = Some(HostLeg::Active);
                }
            }
            (GuestLeg::Active, RecordType::Credit) => {
                self.check_guest(&record);
                self.credit += record.credit().unwrap();
                assert!(self.credit <= self.window, "the guest credited too much");
                self.release();
            }
            (GuestLeg::Active, RecordType::Data) => {
                self.check_guest(&record);
                assert_eq!(self.host, Some(HostLeg::Active), "no host is attached");
                self.queue_host(RecordType::Data, record.payload());
            }
            (state, record_type) => panic!("the guest sent {record_type} while {state:?}"),
        }
    }

    /// Applies one record from the host.
    fn host_record(&mut self, record: Record) {
        match (self.host, record.record_type()) {
            (Some(HostLeg::Connected), RecordType::HostAttach) => {
                if record.payload() != CAPABILITY {
                    let code = ErrorCode::Authentication.code().to_le_bytes();
                    self.queue_host(RecordType::Error, &code);
                    self.host = Some(HostLeg::Refused);
                } else if self.guest == GuestLeg::Active {
                    self.queue_host(RecordType::Ready, &[]);
                    self.host = Some(HostLeg::Active);
                } else {
                    self.queue_host(RecordType::Wait, &[]);
                    self.host = Some(HostLeg::Waiting);
                }
            }
            (Some(HostLeg::Active), RecordType::Data) => {
                let header = (record.instance_id(), record.epoch(), record.sequence());
                assert_eq!(header, (INSTANCE, self.epoch, self.host_receive));
                self.host_receive += 1;
                self.held.push_back(record.into_payload());
                self.release();
            }
            (state, record_type) => panic!("the host sent {record_type} while {state:?}"),
        }
    }

    /// Checks that a record from the guest carries the identity and the next sequence number.
    fn check_guest(&mut self, record: &Record) {
        let header = (record.instance_id(), record.epoch(), record.sequence());
        assert_eq!(header, (INSTANCE, self.epoch, self.guest_receive));
        self.guest_receive += 1;
    }

    /// Forwards the host's held DATA to the guest, in order, while the guest's window has room.
    fn release(&mut self) {
        while let Some(length) = self.held.front().map(|payload| payload.len() as u32) {
            if length > self.credit {
                break;
            }
            self.credit -= length;
            let payload = self.held.pop_front().unwrap();
            self.queue_guest(RecordType::Data, &payload);
        }
    }

    /// Encodes a record for the guest.
    fn queue_guest(&mut self, record_type: RecordType, payload: &[u8]) {
        let encoded = record(record_type, self.identity(), self.guest_send, payload);
        self.to_guest.extend(encoded);
        self.guest_send += 1;
    }

    /// Encodes a record for the host.
    fn queue_host(&mut self, record_type: RecordType, payload: &[u8]) {
        let encoded = record(record_type, self.identity(), self.host_send, payload);
        self.to_host.extend(encoded);
        self.host_send += 1;
    }
}
