//! Tests of the NVXS session: records against OpenVMM's vectors and cases and every payload rule,
//! the parser across partial reads and record boundaries, the state machines of both roles, and
//! both roles together through a broker that relays records as OpenVMM's does, which the tests of
//! the session's drivers share.

use super::*;
use crate::protocol::{
    CAPABILITY_LEN, OUTER_HEADER_LEN, OUTER_MAX_PAYLOAD, OUTER_MAX_RECEIVE_WINDOW,
    OUTER_MIN_RECEIVE_WINDOW, OUTER_RECEIVE_WINDOW,
};
use crate::tests::{OPENVMM_VECTORS, hex};

pub(crate) mod broker;
mod guest;
mod host;
mod record;
mod session;

/// Instance ID of the sessions in these tests.
const INSTANCE: [u8; 16] = [0x22; 16];
/// Identity of the first epoch of the sessions in these tests.
const IDENTITY: Identity = Identity {
    instance_id: INSTANCE,
    epoch: 1,
};
/// The zero identity of a bootstrap record.
const NONE: Identity = Identity {
    instance_id: [0; 16],
    epoch: 0,
};

/// Returns the identity of `epoch` of the sessions' instance, such as an epoch that a RESET
/// starts, or epoch 0, which no session has.
fn epoch(epoch: u64) -> Identity {
    Identity { epoch, ..IDENTITY }
}

/// Returns OpenVMM's vectors, one record of each type, each with its name, in order.
fn openvmm_vectors() -> Vec<(&'static str, Vec<u8>)> {
    OPENVMM_VECTORS
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, encoded) = line
                .split_once('|')
                .unwrap_or_else(|| panic!("OpenVMM's vector {line:?} lacks bytes"));
            (name, hex(encoded))
        })
        .collect()
}

/// Returns OpenVMM's vector named `name`.
fn openvmm_vector(name: &str) -> Vec<u8> {
    openvmm_vectors()
        .into_iter()
        .find_map(|(vector, encoded)| (vector == name).then_some(encoded))
        .unwrap_or_else(|| panic!("OpenVMM has no vector named {name}"))
}

/// Encodes a valid record.
pub(crate) fn record(
    record_type: RecordType,
    identity: Identity,
    sequence: u64,
    payload: &[u8],
) -> Vec<u8> {
    let mut output = Vec::new();
    encode_record(
        record_type,
        &identity.instance_id,
        identity.epoch,
        sequence,
        payload,
        &mut output,
    )
    .unwrap();
    output
}

/// Encodes a valid record of `record_type` with a typical payload, at `identity` and `sequence`
/// unless it is a bootstrap record.
fn sample(record_type: RecordType, identity: Identity, sequence: u64) -> Vec<u8> {
    let payload = match record_type {
        RecordType::HostAttach => vec![1; CAPABILITY_LEN],
        RecordType::Ack => OUTER_MIN_RECEIVE_WINDOW.to_le_bytes().to_vec(),
        RecordType::Credit | RecordType::Error => 1u32.to_le_bytes().to_vec(),
        RecordType::Data => b"x".to_vec(),
        _ => Vec::new(),
    };
    if record_type.is_bootstrap() {
        record(record_type, NONE, 0, &payload)
    } else {
        record(record_type, identity, sequence, &payload)
    }
}

/// Encodes a record header that declares `length` payload bytes, without validating it.
fn raw_header(code: u8, identity: Identity, sequence: u64, length: u32) -> Vec<u8> {
    let mut header = Vec::with_capacity(OUTER_HEADER_LEN);
    header.extend_from_slice(b"NVXS");
    header.extend_from_slice(&1u16.to_le_bytes());
    header.push(code);
    header.push(0);
    header.extend_from_slice(&identity.instance_id);
    header.extend_from_slice(&identity.epoch.to_le_bytes());
    header.extend_from_slice(&sequence.to_le_bytes());
    header.extend_from_slice(&length.to_le_bytes());
    header
}

/// Parses `bytes`, which hold exactly one record.
fn parse(bytes: &[u8]) -> Result<Record, RecordError> {
    let mut parser = RecordParser::new();
    let (consumed, record) = parser.push(bytes)?;
    assert_eq!(consumed, bytes.len());
    Ok(record.expect("the bytes hold a complete record"))
}

/// Parses every record of `bytes`, which end at a record boundary.
fn records(bytes: &[u8]) -> Vec<Record> {
    let mut parser = RecordParser::new();
    let records = feed(
        |piece| parser.push(piece).map_err(SessionError::from),
        bytes,
        &[],
    )
    .unwrap();
    assert_eq!(parser.buffered(), 0);
    records
}

/// Feeds `stream` to `receive` in pieces that end at each of `cuts` and then at its end, and
/// returns the items that it reports, in order.
fn feed<T>(
    mut receive: impl FnMut(&[u8]) -> Result<(usize, Option<T>), SessionError>,
    stream: &[u8],
    cuts: &[usize],
) -> Result<Vec<T>, SessionError> {
    let mut items = Vec::new();
    let mut start = 0;
    for end in cuts.iter().copied().chain([stream.len()]) {
        let mut piece = &stream[start..end];
        while !piece.is_empty() {
            let (consumed, item) = receive(piece)?;
            assert!(consumed > 0, "a nonempty piece makes progress");
            piece = &piece[consumed..];
            items.extend(item);
        }
        start = end;
    }
    Ok(items)
}

/// The broker's records to a host that attaches after waiting, receives three DATA records, and
/// is reset: payloads of 1, 21, and 300 bytes, one of them with the magic inside.
fn host_stream() -> Vec<u8> {
    [
        record(RecordType::Wait, IDENTITY, 0, &[]),
        record(RecordType::Ready, IDENTITY, 1, &[]),
        record(RecordType::Data, IDENTITY, 2, b"x"),
        record(RecordType::Data, IDENTITY, 3, b"NVXS inside a payload"),
        record(RecordType::Data, IDENTITY, 4, &[0x5a; 300]),
        record(RecordType::Reset, epoch(2), 5, &[]),
    ]
    .concat()
}

/// Application framing of the tests: each message is a little-endian `u32` length, then the
/// message, and messages may span DATA records.
pub(crate) fn framed(message: &[u8]) -> Vec<u8> {
    [&(message.len() as u32).to_le_bytes(), message].concat()
}

/// Receiver of the tests' application framing, which reassembles messages across DATA records.
#[derive(Default)]
pub(crate) struct Reassembler {
    /// Bytes of the message in progress.
    pending: Vec<u8>,
}

impl Reassembler {
    /// Appends the payload of a DATA record, and returns the messages that it completes.
    pub(crate) fn push(&mut self, payload: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(payload);
        let mut messages = Vec::new();
        while self.pending.len() >= 4 {
            let length = u32::from_le_bytes(self.pending[..4].try_into().unwrap()) as usize;
            if self.pending.len() < 4 + length {
                break;
            }
            messages.push(self.pending[4..4 + length].to_vec());
            self.pending.drain(..4 + length);
        }
        messages
    }

    /// Returns whether a message is in progress.
    pub(crate) fn is_partial(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// Messages of 10 bytes, of the largest frame that fits one record, of one more byte, and of
/// about 300 KB, which spans five records.
pub(crate) fn messages() -> Vec<Vec<u8>> {
    let largest_in_one_record = OUTER_MAX_PAYLOAD - 4;
    [
        10,
        largest_in_one_record,
        largest_in_one_record + 1,
        300_000,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, length)| {
        (0..length)
            .map(|byte| (byte as u8).wrapping_mul(31).wrapping_add(index as u8))
            .collect()
    })
    .collect()
}
