//! The NVXS session: the authenticated record stream that OpenVMM's broker relays between one host
//! client and the guest, without I/O and without an application protocol.
//!
//! Application frames travel in the session's DATA records: NVX's own (`NVXC`) and those of any
//! other application protocol. The session carries application bytes in and out, and depends on
//! none of those protocols.
//!
//! - Records: [`RecordType`], [`Record`], [`encode_record`], [`ErrorCode`], and [`RecordError`].
//!   A record is a 44-byte header, then a payload whose length its type fixes; the
//!   [`protocol`](crate::protocol) module's `OUTER_*` constants name the format's sizes and
//!   limits.
//! - [`RecordParser`]: incremental parsing across partial reads and record boundaries, with
//!   buffers bounded by one record.
//! - [`HostSession`] and [`GuestSession`]: the state machines of the host's and the guest's role.
//!   They validate each record against the session's state, identity, and sequence numbers, and
//!   encode the records that their role sends.
//!
//! The state machines do no I/O: callers hand them the bytes that they receive and write the
//! bytes that they encode. The host's `nvx::host::vm::Connection` (feature `host`) and the
//! guest's `nvx::guest::control::Connection` (feature `guest`, Linux) drive them over a transport.
//!
//! # Protocol
//!
//! The broker relays records between the guest's leg and the host's. Each direction of each leg
//! numbers its records from 0 in sequence, and every record after the bootstrap carries the
//! session's [`Identity`]: the broker's instance ID and the current epoch.
//!
//! The guest's role:
//!
//! 1. The guest sends GUEST_ATTACH, a bootstrap record, whose identity and sequence are zero.
//! 2. The broker answers with RESET, which carries the session's identity. The guest abandons the
//!    work of any earlier epoch, then sends ACK at sequence 0, which advertises its receive
//!    window. Its later records continue at sequence 1, and it expects the broker's next record at
//!    the sequence after the RESET's.
//! 3. Each DATA record from the broker carries the identity and the next sequence, and its
//!    payload counts against the receive window. Once the guest consumed payload bytes, it
//!    returns them with CREDIT. ACK, CREDIT, and the guest's DATA share the guest's sequence.
//! 4. A later RESET starts a new epoch. The epoch of an instance only advances; a new instance,
//!    such as the broker's after a snapshot restore, starts over.
//!
//! The host's role:
//!
//! 1. The host sends HOST_ATTACH, a bootstrap record that carries its launch capability.
//! 2. The broker answers with WAIT while the guest has not acknowledged the session's reset, then
//!    with READY, which carries the session's identity. It refuses a wrong capability with ERROR,
//!    or closes the stream. Its records to the host are numbered from 0.
//! 3. DATA flows both ways with the session's identity: the host's from sequence 0, the broker's
//!    after READY's. The broker holds the host's DATA until the guest has window for it.
//! 4. A RESET ends the host's attachment and reports the session's new identity. OpenVMM's broker
//!    closes the host's stream instead.
//!
//! A malformed record, a record that the role's state does not allow, a wrong identity, and a
//! duplicate or out-of-order sequence number each fail the session, because NVXS cannot
//! resynchronize a stream. A stream that ends inside a record fails it too, while one that ends at
//! a record boundary closes it.
//!
//! Each DATA record carries 1 through [`OUTER_MAX_PAYLOAD`] application bytes. `send` takes that
//! many bytes at most from the front of its input, so an application message of more bytes spans
//! several records, and a protocol that needs one frame per record sends frames that fit. The
//! receiver gets each record's payload in order, so a protocol with its own framing reassembles
//! its messages across records.
//!
//! The receive window and credits count DATA payload bytes from the host to the guest. They belong
//! to the session and are separate from any flow control of an application protocol.
//!
//! # Example
//!
//! ```
//! use nvx::protocol::CAPABILITY_LEN;
//! use nvx::protocol::nvxs::{HostEvent, HostSession, Identity, RecordType, encode_record};
//!
//! let mut host = HostSession::new();
//! let mut to_broker = Vec::new();
//! host.attach(&[7; CAPABILITY_LEN], &mut to_broker)?;
//!
//! // The broker attaches the host at its sequence 0, with the session's identity.
//! let identity = Identity { instance_id: [9; 16], epoch: 1 };
//! let mut from_broker = Vec::new();
//! encode_record(RecordType::Ready, &identity.instance_id, identity.epoch, 0, &[], &mut from_broker)?;
//! let (consumed, event) = host.receive(&from_broker)?;
//! assert_eq!(consumed, from_broker.len());
//! assert_eq!(event, Some(HostEvent::Attached(identity)));
//!
//! // A message of 100,000 bytes spans two DATA records.
//! let message = vec![0x5a; 100_000];
//! let mut sent = 0;
//! while sent < message.len() {
//!     to_broker.clear();
//!     sent += host.send(&message[sent..], &mut to_broker)?;
//!     // Write `to_broker` to the transport here.
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`OUTER_MAX_PAYLOAD`]: crate::protocol::OUTER_MAX_PAYLOAD

use std::fmt;

use crate::protocol::OUTER_MAX_PAYLOAD;

mod guest;
mod host;
mod record;
#[cfg(test)]
pub(crate) mod tests;

pub use guest::{GuestEvent, GuestSession, GuestState};
pub use host::{HostEvent, HostSession, HostState};
pub use record::{ErrorCode, Record, RecordError, RecordParser, RecordType, encode_record};

/// Identity of an established session, which every record after the bootstrap repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    /// ID of the broker instance. A broker restored from a snapshot has a new one.
    pub instance_id: [u8; 16],
    /// Epoch of the session, which each reset advances. It is never zero.
    pub epoch: u64,
}

/// Failure of an NVXS session.
///
/// A failure in a received record ends the session; see [`HostState::Failed`] and
/// [`GuestState::Failed`]. An operation that the caller asks for and that the session refuses,
/// such as a credit of more bytes than the guest received, changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionError {
    /// A malformed record, or a record to send that would be one.
    Record(RecordError),
    /// A record that the session's state does not allow.
    Unexpected(RecordType),
    /// A READY or RESET record whose instance ID or epoch is zero.
    MissingIdentity(RecordType),
    /// A record of another session instance.
    Instance {
        /// Instance ID of the session.
        expected: [u8; 16],
        /// Instance ID of the record.
        actual: [u8; 16],
    },
    /// A record of another epoch.
    Epoch {
        /// Epoch of the session.
        expected: u64,
        /// Epoch of the record.
        actual: u64,
    },
    /// A RESET of the session's instance that does not advance the session's epoch.
    StaleEpoch {
        /// Epoch of the session.
        current: u64,
        /// Epoch of the RESET.
        actual: u64,
    },
    /// A record whose sequence number is not the next one of its direction.
    Sequence {
        /// Sequence number that the session expects.
        expected: u64,
        /// Sequence number of the record.
        actual: u64,
    },
    /// The sequence numbers of a direction ran out at a record of this type, which took or would
    /// take the last one and leave none for the next record.
    SequenceOverflow(RecordType),
    /// A DATA record from the broker that exceeds the guest's receive window.
    Window {
        /// Receive window that the guest advertised.
        window: u32,
        /// Bytes that the guest received and has not credited.
        outstanding: u32,
        /// Length of the record's payload.
        length: u32,
    },
    /// A credit of zero bytes, or of more bytes than the guest received and has not credited.
    Credit {
        /// Bytes that the guest received and has not credited.
        outstanding: u32,
        /// Bytes of the credit.
        bytes: u32,
    },
    /// The stream ended inside a record.
    Truncated {
        /// Bytes of the partial record.
        buffered: usize,
    },
    /// An operation that the session's state does not allow, such as sending before the session
    /// is attached or after it ended.
    InvalidState,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(error) => error.fmt(formatter),
            Self::Unexpected(record_type) => {
                write!(formatter, "unexpected NVXS {record_type} record")
            }
            Self::MissingIdentity(record_type) => {
                write!(
                    formatter,
                    "NVXS {record_type} record carries no session identity"
                )
            }
            Self::Instance { .. } => {
                formatter.write_str("NVXS record belongs to another session instance")
            }
            Self::Epoch { expected, actual } => write!(
                formatter,
                "NVXS record has epoch {actual} instead of the session's {expected}"
            ),
            Self::StaleEpoch { current, actual } => write!(
                formatter,
                "NVXS RESET record has epoch {actual}, which does not advance the session's \
                 epoch {current}"
            ),
            Self::Sequence { expected, actual } => write!(
                formatter,
                "NVXS record has sequence {actual} instead of {expected}"
            ),
            Self::SequenceOverflow(record_type) => write!(
                formatter,
                "NVXS sequence numbers ran out at the {record_type} record"
            ),
            Self::Window {
                window,
                outstanding,
                length,
            } => write!(
                formatter,
                "NVXS DATA record of {length} bytes exceeds the receive window of {window} \
                 bytes, of which {outstanding} are outstanding"
            ),
            Self::Credit { outstanding, bytes } => write!(
                formatter,
                "NVXS credit of {bytes} bytes is invalid with {outstanding} bytes outstanding"
            ),
            Self::Truncated { buffered } => write!(
                formatter,
                "NVXS stream ended inside a record, after {buffered} bytes of it"
            ),
            Self::InvalidState => formatter
                .write_str("the NVXS session does not allow the operation in its current state"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Record(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RecordError> for SessionError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}

/// Returns the identity that a READY or RESET record carries, or `None` if its instance ID or its
/// epoch is zero.
fn record_identity(record: &Record) -> Option<Identity> {
    (record.instance_id() != [0; 16] && record.epoch() != 0).then(|| Identity {
        instance_id: record.instance_id(),
        epoch: record.epoch(),
    })
}

/// Returns the sequence number after `sequence`, which a record of `record_type` takes.
fn next_sequence(sequence: u64, record_type: RecordType) -> Result<u64, SessionError> {
    sequence
        .checked_add(1)
        .ok_or(SessionError::SequenceOverflow(record_type))
}

/// Checks that `record` carries the sequence number `expected`.
fn check_sequence(record: &Record, expected: u64) -> Result<(), SessionError> {
    if record.sequence() == expected {
        Ok(())
    } else {
        Err(SessionError::Sequence {
            expected,
            actual: record.sequence(),
        })
    }
}

/// Checks that `record` carries the session's `identity`: its instance ID, then its epoch.
fn check_identity(record: &Record, identity: &Identity) -> Result<(), SessionError> {
    if record.instance_id() != identity.instance_id {
        return Err(SessionError::Instance {
            expected: identity.instance_id,
            actual: record.instance_id(),
        });
    }
    if record.epoch() != identity.epoch {
        return Err(SessionError::Epoch {
            expected: identity.epoch,
            actual: record.epoch(),
        });
    }
    Ok(())
}

/// Encodes a record of `record_type` with `payload` and the session's `identity` at the next
/// sequence number of its direction, which `sequence` holds, and advances `sequence` past it.
///
/// A record that would take the direction's last sequence number, or that would be malformed,
/// fails without encoding anything or advancing `sequence`.
fn encode_next(
    record_type: RecordType,
    identity: &Identity,
    sequence: &mut u64,
    payload: &[u8],
    output: &mut Vec<u8>,
) -> Result<(), SessionError> {
    let next = next_sequence(*sequence, record_type)?;
    encode_record(
        record_type,
        &identity.instance_id,
        identity.epoch,
        *sequence,
        payload,
        output,
    )?;
    *sequence = next;
    Ok(())
}

/// Encodes a DATA record of the session with `identity` and at most [`OUTER_MAX_PAYLOAD`] bytes
/// from the front of `data` as [`encode_next`] does, and returns how many bytes it took.
fn encode_data(
    identity: &Identity,
    sequence: &mut u64,
    data: &[u8],
    output: &mut Vec<u8>,
) -> Result<usize, SessionError> {
    let taken = data.len().min(OUTER_MAX_PAYLOAD);
    encode_next(RecordType::Data, identity, sequence, &data[..taken], output)?;
    Ok(taken)
}
