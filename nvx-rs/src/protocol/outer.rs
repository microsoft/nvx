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
    header[..4].copy_from_slice(&OUTER_MAGIC);
    header[4..6].copy_from_slice(&OUTER_VERSION.to_le_bytes());
    header[6] = record_type;
    // The flags byte, header[7], stays zero.
    header[8..24].copy_from_slice(instance_id);
    header[24..32].copy_from_slice(&epoch.to_le_bytes());
    header[32..40].copy_from_slice(&sequence.to_le_bytes());
    header[40..].copy_from_slice(&length.to_le_bytes());
    header
}

/// Decodes and validates an outer record header.
pub(crate) fn decode_outer_header(
    header: &[u8; OUTER_HEADER_LEN],
) -> Result<OuterHeader, ProtocolError> {
    let version = u16::from_le_bytes([header[4], header[5]]);
    let flags = header[7];
    let length = u32::from_le_bytes(array(&header[40..44])) as usize;
    if header[..4] != OUTER_MAGIC || version != OUTER_VERSION || flags != 0 {
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
        record_type: header[6],
        instance_id: array(&header[8..24]),
        epoch: u64::from_le_bytes(array(&header[24..32])),
        sequence: u64::from_le_bytes(array(&header[32..40])),
        length,
    })
}
