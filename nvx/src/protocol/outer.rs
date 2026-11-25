//! Outer records (magic `NVXS`), which OpenVMM brokers between one host client and the guest
//! agent.
//!
//! A record is a 44-byte header (the magic, the version, the record type, a zero flags byte, the
//! session's 16-byte instance ID, its epoch, the record's sequence number, and the payload length)
//! followed by the payload.
//!
//! The guest first sends [`OUTER_GUEST_ATTACH`] with zero identity fields. The broker answers
//! with [`OUTER_RESET`]. A valid reset has no payload and a nonzero epoch; the guest adopts its
//! identity, expects the next broker-to-guest sequence after the reset's sequence, resets its own
//! sequence to zero, and sends [`OUTER_ACK`] at sequence zero with [`OUTER_RECEIVE_WINDOW`].
//! Guest records continue at sequence one. Each [`OUTER_DATA`] must match the current identity,
//! epoch, and next sequence exactly. Once the guest consumes its payload, it sends
//! [`OUTER_CREDIT`] for the payload byte count. ACK, CREDIT, and guest DATA share the ordered
//! guest sequence; a later RESET starts a new epoch and abandons work from the old one.

use std::ops::Range;

use super::{ProtocolError, array, violation};

/// Magic of every outer record.
pub(crate) const OUTER_MAGIC: [u8; 4] = *b"NVXS";
/// Version of the outer record format.
pub(crate) const OUTER_VERSION: u16 = 1;
/// Length of an outer record header.
pub(crate) const OUTER_HEADER_LEN: usize = 44;
/// Largest payload of one outer record.
pub(crate) const OUTER_MAX_PAYLOAD: usize = 65_536;
/// Receive window that the NVX guest advertises after a reset.
pub(crate) const OUTER_RECEIVE_WINDOW: u32 = 1024 * 1024;
/// Smallest receive window that the broker accepts in an acknowledgement.
pub(crate) const OUTER_MIN_RECEIVE_WINDOW: u32 = OUTER_MAX_PAYLOAD as u32;
/// Largest receive window that the broker accepts.
pub(crate) const OUTER_MAX_RECEIVE_WINDOW: u32 = 4 * 1024 * 1024 + 10;

/// Bootstrap record with which the guest agent attaches to the broker.
pub(crate) const OUTER_GUEST_ATTACH: u8 = 1;
/// Record with which the host client attaches, carrying the launch capability.
pub(crate) const OUTER_HOST_ATTACH: u8 = 2;
/// Record with which OpenVMM resets the session.
pub(crate) const OUTER_RESET: u8 = 3;
/// Record with which the guest acknowledges a reset and advertises its receive window.
pub(crate) const OUTER_ACK: u8 = 4;
/// Record that carries an application frame.
pub(crate) const OUTER_DATA: u8 = 5;
/// Record with which OpenVMM accepts the host client's capability and asks it to wait until the
/// guest attaches and acknowledges the session's reset.
pub(crate) const OUTER_WAIT: u8 = 6;
/// Record with which OpenVMM accepts the host client. It carries the session's instance ID and
/// epoch, which every data record repeats.
pub(crate) const OUTER_READY: u8 = 7;
/// Record with which OpenVMM rejects the host client's capability.
pub(crate) const OUTER_ERROR: u8 = 8;
/// Record with which the guest returns bytes to its receive window.
pub(crate) const OUTER_CREDIT: u8 = 9;

/// Length of the launch capability that authenticates the host client.
pub(crate) const CAPABILITY_LEN: usize = 32;

/// Positions of the fields of an outer record header, which the format lays out in this order,
/// with no padding, and which [`encode_header`] and [`decode_header`] share.
mod field {
    use super::{OUTER_HEADER_LEN, Range};

    /// Bytes of the magic, [`OUTER_MAGIC`](super::OUTER_MAGIC).
    pub(super) const MAGIC: Range<usize> = 0..4;
    /// Bytes of the version, a little-endian `u16`.
    pub(super) const VERSION: Range<usize> = 4..6;
    /// Byte of the record type.
    pub(super) const RECORD_TYPE: usize = 6;
    /// Byte of the flags, zero in a valid record.
    pub(super) const FLAGS: usize = 7;
    /// Bytes of the session's 16-byte instance ID.
    pub(super) const INSTANCE_ID: Range<usize> = 8..24;
    /// Bytes of the session's epoch, a little-endian `u64`.
    pub(super) const EPOCH: Range<usize> = 24..32;
    /// Bytes of the record's sequence number, a little-endian `u64`.
    pub(super) const SEQUENCE: Range<usize> = 32..40;
    /// Bytes of the payload length, a little-endian `u32`, which end the header.
    pub(super) const LENGTH: Range<usize> = 40..OUTER_HEADER_LEN;
}

/// Returns the number of bytes of `range`.
const fn width(range: Range<usize>) -> usize {
    range.end - range.start
}

// The fields tile the header in order, and each range holds exactly the bytes of its field's
// type, so that the header's slices and `array`'s copies always fit.
const _: () = {
    assert!(field::MAGIC.start == 0);
    assert!(field::MAGIC.end == field::VERSION.start);
    assert!(field::VERSION.end == field::RECORD_TYPE);
    assert!(field::RECORD_TYPE + 1 == field::FLAGS);
    assert!(field::FLAGS + 1 == field::INSTANCE_ID.start);
    assert!(field::INSTANCE_ID.end == field::EPOCH.start);
    assert!(field::EPOCH.end == field::SEQUENCE.start);
    assert!(field::SEQUENCE.end == field::LENGTH.start);
    assert!(field::LENGTH.end == OUTER_HEADER_LEN);
    assert!(width(field::MAGIC) == size_of::<[u8; 4]>());
    assert!(width(field::VERSION) == size_of::<u16>());
    assert!(width(field::INSTANCE_ID) == size_of::<[u8; 16]>());
    assert!(width(field::EPOCH) == size_of::<u64>());
    assert!(width(field::SEQUENCE) == size_of::<u64>());
    assert!(width(field::LENGTH) == size_of::<u32>());
};

/// Decoded outer record header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OuterHeader {
    /// One of the `OUTER_*` record types.
    pub(crate) record_type: u8,
    /// ID of the session instance, all zero before the session is established.
    pub(crate) instance_id: [u8; 16],
    /// Epoch of the session, zero before the session is established.
    pub(crate) epoch: u64,
    /// Sequence number of the record within its direction of the session.
    pub(crate) sequence: u64,
    /// Length of the payload that follows the header.
    pub(crate) length: usize,
}

/// Encodes one outer record.
pub(crate) fn encode_outer(
    record_type: u8,
    instance_id: &[u8; 16],
    epoch: u64,
    sequence: u64,
    payload: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|&length| length as usize <= OUTER_MAX_PAYLOAD)
        .ok_or_else(|| violation("control payload exceeds the outer protocol limit"))?;
    let header = encode_header(record_type, instance_id, epoch, sequence, length);
    let mut record = Vec::with_capacity(OUTER_HEADER_LEN + payload.len());
    record.extend_from_slice(&header);
    record.extend_from_slice(payload);
    Ok(record)
}

/// Returns the header of a record: [`OUTER_MAGIC`], [`OUTER_VERSION`], `record_type`, a zero
/// flags byte, `instance_id`, `epoch`, `sequence`, and the payload's `length`, with the integers
/// little-endian. It checks nothing: [`encode_outer`] and
/// [`encode_record`](super::nvxs::encode_record) each validate their record first.
pub(super) fn encode_header(
    record_type: u8,
    instance_id: &[u8; 16],
    epoch: u64,
    sequence: u64,
    length: u32,
) -> [u8; OUTER_HEADER_LEN] {
    let mut header = [0; OUTER_HEADER_LEN];
    header[field::MAGIC].copy_from_slice(&OUTER_MAGIC);
    header[field::VERSION].copy_from_slice(&OUTER_VERSION.to_le_bytes());
    header[field::RECORD_TYPE] = record_type;
    // The flags byte, header[field::FLAGS], stays zero.
    header[field::INSTANCE_ID].copy_from_slice(instance_id);
    header[field::EPOCH].copy_from_slice(&epoch.to_le_bytes());
    header[field::SEQUENCE].copy_from_slice(&sequence.to_le_bytes());
    header[field::LENGTH].copy_from_slice(&length.to_le_bytes());
    header
}

/// Fields of a record header, as [`decode_header`] reads them, none of them checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HeaderFields {
    /// Magic, which a valid record sets to [`OUTER_MAGIC`].
    pub(super) magic: [u8; 4],
    /// Version, which a valid record sets to [`OUTER_VERSION`].
    pub(super) version: u16,
    /// Code of the record type.
    pub(super) record_type: u8,
    /// Flags, which a valid record sets to zero.
    pub(super) flags: u8,
    /// Instance ID of the session.
    pub(super) instance_id: [u8; 16],
    /// Epoch of the session.
    pub(super) epoch: u64,
    /// Sequence number of the record.
    pub(super) sequence: u64,
    /// Length of the payload.
    pub(super) length: u32,
}

/// Reads the fields of a record header at the offsets at which [`encode_header`] writes them. It
/// checks nothing: [`decode_outer_header`] and the NVXS record parser each validate the fields.
pub(super) fn decode_header(header: &[u8; OUTER_HEADER_LEN]) -> HeaderFields {
    HeaderFields {
        magic: array(&header[field::MAGIC]),
        version: u16::from_le_bytes(array(&header[field::VERSION])),
        record_type: header[field::RECORD_TYPE],
        flags: header[field::FLAGS],
        instance_id: array(&header[field::INSTANCE_ID]),
        epoch: u64::from_le_bytes(array(&header[field::EPOCH])),
        sequence: u64::from_le_bytes(array(&header[field::SEQUENCE])),
        length: u32::from_le_bytes(array(&header[field::LENGTH])),
    }
}

/// Decodes and validates an outer record header.
pub(crate) fn decode_outer_header(
    header: &[u8; OUTER_HEADER_LEN],
) -> Result<OuterHeader, ProtocolError> {
    let fields = decode_header(header);
    let length = fields.length as usize;
    if fields.magic != OUTER_MAGIC || fields.version != OUTER_VERSION || fields.flags != 0 {
        return Err(violation(
            "control endpoint returned an invalid outer record",
        ));
    }
    if length > OUTER_MAX_PAYLOAD {
        return Err(violation(
            "control endpoint returned an oversized outer record",
        ));
    }
    Ok(OuterHeader {
        record_type: fields.record_type,
        instance_id: fields.instance_id,
        epoch: fields.epoch,
        sequence: fields.sequence,
        length,
    })
}
