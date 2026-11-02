//! NVXS records: their types, validation, encoding, and incremental parsing.

use std::fmt;

use crate::protocol::outer::encode_header;
use crate::protocol::{
    CAPABILITY_LEN, OUTER_ACK, OUTER_CREDIT, OUTER_DATA, OUTER_ERROR, OUTER_GUEST_ATTACH,
    OUTER_HEADER_LEN, OUTER_HOST_ATTACH, OUTER_MAGIC, OUTER_MAX_PAYLOAD, OUTER_MAX_RECEIVE_WINDOW,
    OUTER_MIN_RECEIVE_WINDOW, OUTER_READY, OUTER_RESET, OUTER_VERSION, OUTER_WAIT, array,
};

/// Type of an NVXS record, the byte at offset 6 of its header.
///
/// Each type allows one payload length, except [`Data`](Self::Data), which carries 1 through
/// [`OUTER_MAX_PAYLOAD`] bytes; [`allows_length`](Self::allows_length) tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordType {
    /// Bootstrap record with which the guest attaches. Its payload is empty.
    GuestAttach = OUTER_GUEST_ATTACH,
    /// Bootstrap record with which the host attaches. Its payload is the [`CAPABILITY_LEN`]-byte
    /// launch capability.
    HostAttach = OUTER_HOST_ATTACH,
    /// Record with which the broker starts a new epoch of the session. Its payload is empty.
    Reset = OUTER_RESET,
    /// Record with which the guest acknowledges a reset. Its payload is the guest's initial
    /// receive window, a little-endian `u32`.
    Ack = OUTER_ACK,
    /// Record that carries application bytes between the host and the guest.
    Data = OUTER_DATA,
    /// Record with which the broker tells a host whose capability it accepted to wait until the
    /// guest acknowledges a reset. Its payload is empty.
    Wait = OUTER_WAIT,
    /// Record with which the broker attaches the host to the session's identity. Its payload is
    /// empty.
    Ready = OUTER_READY,
    /// Record with which the broker refuses the host. Its payload is an [`ErrorCode`], a
    /// little-endian `u32`.
    Error = OUTER_ERROR,
    /// Record with which the guest returns consumed DATA bytes to its receive window. Its payload
    /// is the number of bytes, a little-endian `u32`.
    Credit = OUTER_CREDIT,
}

impl RecordType {
    /// Returns the type's code, the byte at offset 6 of a record header.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Returns the type with `code`, or `None` if protocol version 1 defines no such type.
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            OUTER_GUEST_ATTACH => Some(Self::GuestAttach),
            OUTER_HOST_ATTACH => Some(Self::HostAttach),
            OUTER_RESET => Some(Self::Reset),
            OUTER_ACK => Some(Self::Ack),
            OUTER_DATA => Some(Self::Data),
            OUTER_WAIT => Some(Self::Wait),
            OUTER_READY => Some(Self::Ready),
            OUTER_ERROR => Some(Self::Error),
            OUTER_CREDIT => Some(Self::Credit),
            _ => None,
        }
    }

    /// Returns whether records of this type are bootstrap records, whose instance ID, epoch, and
    /// sequence are zero.
    pub const fn is_bootstrap(self) -> bool {
        matches!(self, Self::GuestAttach | Self::HostAttach)
    }

    /// Returns whether a record of this type may carry a payload of `length` bytes.
    pub const fn allows_length(self, length: usize) -> bool {
        match self {
            Self::GuestAttach | Self::Reset | Self::Wait | Self::Ready => length == 0,
            Self::HostAttach => length == CAPABILITY_LEN,
            Self::Ack | Self::Error | Self::Credit => length == 4,
            Self::Data => length >= 1 && length <= OUTER_MAX_PAYLOAD,
        }
    }

    /// Returns the protocol's name of the type, such as `GUEST_ATTACH`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::GuestAttach => "GUEST_ATTACH",
            Self::HostAttach => "HOST_ATTACH",
            Self::Reset => "RESET",
            Self::Ack => "ACK",
            Self::Data => "DATA",
            Self::Wait => "WAIT",
            Self::Ready => "READY",
            Self::Error => "ERROR",
            Self::Credit => "CREDIT",
        }
    }
}

impl fmt::Display for RecordType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Code of an ERROR record. Protocol version 1 defines one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// The broker refused the host's launch capability.
    Authentication,
}

impl ErrorCode {
    /// Returns the code's value, the payload of an ERROR record.
    pub const fn code(self) -> u32 {
        match self {
            Self::Authentication => 1,
        }
    }

    /// Returns the code with value `code`, or `None` if protocol version 1 defines no such code.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Authentication),
            _ => None,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication => formatter.write_str("authentication failed"),
        }
    }
}

/// Malformed NVXS record.
///
/// A stream that carries one cannot be resynchronized, so the receiver ends it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordError {
    /// The header does not start with the magic `NVXS`.
    Magic([u8; 4]),
    /// The header names a protocol version other than 1.
    Version(u16),
    /// The header names a record type that protocol version 1 does not define.
    Type(u8),
    /// The header's flags byte is not zero.
    Flags(u8),
    /// The payload length is not one that the record type allows.
    Length {
        /// Type of the record.
        record_type: RecordType,
        /// Length of its payload in bytes.
        length: usize,
    },
    /// A bootstrap record carries a nonzero instance ID, epoch, or sequence.
    BootstrapIdentity(RecordType),
    /// An ERROR record carries a code that protocol version 1 does not define.
    ErrorCode(u32),
    /// An ACK record advertises a receive window outside [`OUTER_MIN_RECEIVE_WINDOW`] through
    /// [`OUTER_MAX_RECEIVE_WINDOW`] bytes.
    ReceiveWindow(u32),
    /// A CREDIT record returns zero bytes or more than [`OUTER_MAX_RECEIVE_WINDOW`] bytes.
    Credit(u32),
}

impl fmt::Display for RecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Magic(magic) => {
                formatter.write_str("NVXS record has an invalid magic, 0x")?;
                for byte in magic {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
            Self::Version(version) => {
                write!(
                    formatter,
                    "NVXS record has an unsupported version {version}"
                )
            }
            Self::Type(code) => write!(formatter, "NVXS record has an unknown type {code}"),
            Self::Flags(flags) => write!(formatter, "NVXS record has nonzero flags {flags:#04x}"),
            Self::Length {
                record_type,
                length,
            } => write!(
                formatter,
                "NVXS {record_type} record has an invalid payload length of {length} bytes"
            ),
            Self::BootstrapIdentity(record_type) => {
                write!(
                    formatter,
                    "NVXS {record_type} record carries a session identity"
                )
            }
            Self::ErrorCode(code) => {
                write!(formatter, "NVXS ERROR record has an unknown code {code}")
            }
            Self::ReceiveWindow(window) => write!(
                formatter,
                "NVXS ACK record advertises an invalid receive window of {window} bytes"
            ),
            Self::Credit(bytes) => {
                write!(
                    formatter,
                    "NVXS CREDIT record returns an invalid {bytes} bytes"
                )
            }
        }
    }
}

impl std::error::Error for RecordError {}

/// NVXS record that [`RecordParser`] decoded and validated.
///
/// Its fields satisfy every rule that [`encode_record`] checks. `Debug` shows the payload's
/// length rather than its bytes, so that a host's capability cannot reach a log through it.
#[derive(Clone, PartialEq, Eq)]
pub struct Record {
    /// Type of the record.
    record_type: RecordType,
    /// Instance ID of the session, zero in a bootstrap record.
    instance_id: [u8; 16],
    /// Epoch of the session, zero in a bootstrap record.
    epoch: u64,
    /// Sequence number of the record in its direction, zero in a bootstrap record.
    sequence: u64,
    /// Payload, whose length and value the record type allows.
    payload: Vec<u8>,
}

impl Record {
    /// Returns the record's type.
    pub fn record_type(&self) -> RecordType {
        self.record_type
    }

    /// Returns the session's instance ID, zero in a bootstrap record.
    pub fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    /// Returns the session's epoch, zero in a bootstrap record.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the record's sequence number in its direction, zero in a bootstrap record.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the record's payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Returns the record's payload, consuming the record.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }

    /// Returns the initial receive window of an ACK record, or `None` for another type.
    pub fn window(&self) -> Option<u32> {
        (self.record_type == RecordType::Ack).then(|| value(&self.payload))
    }

    /// Returns the number of bytes that a CREDIT record returns, or `None` for another type.
    pub fn credit(&self) -> Option<u32> {
        (self.record_type == RecordType::Credit).then(|| value(&self.payload))
    }

    /// Returns the code of an ERROR record, or `None` for another type.
    pub fn error_code(&self) -> Option<ErrorCode> {
        if self.record_type == RecordType::Error {
            ErrorCode::from_code(value(&self.payload))
        } else {
            None
        }
    }
}

impl fmt::Debug for Record {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Record")
            .field("record_type", &self.record_type)
            .field("instance_id", &self.instance_id)
            .field("epoch", &self.epoch)
            .field("sequence", &self.sequence)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

/// Validates one record and appends its encoding to `output`.
///
/// It checks what [`RecordParser`] checks: the payload length that `record_type` allows, the zero
/// identity of a bootstrap record, the code of an ERROR record, and the byte counts of ACK and
/// CREDIT records. On an error, `output` is unchanged.
pub fn encode_record(
    record_type: RecordType,
    instance_id: &[u8; 16],
    epoch: u64,
    sequence: u64,
    payload: &[u8],
    output: &mut Vec<u8>,
) -> Result<(), RecordError> {
    check_length(record_type, payload.len())?;
    check_bootstrap(record_type, instance_id, epoch, sequence)?;
    check_payload(record_type, payload)?;
    // The length check bounds the payload by `OUTER_MAX_PAYLOAD`, so it fits the header's field.
    let length = payload.len() as u32;
    let header = encode_header(record_type.code(), instance_id, epoch, sequence, length);
    output.reserve(OUTER_HEADER_LEN + payload.len());
    output.extend_from_slice(&header);
    output.extend_from_slice(payload);
    Ok(())
}

/// Validated fields of a record header.
#[derive(Clone, Copy)]
struct Header {
    /// Type of the record.
    record_type: RecordType,
    /// Instance ID of the session.
    instance_id: [u8; 16],
    /// Epoch of the session.
    epoch: u64,
    /// Sequence number of the record.
    sequence: u64,
    /// Length of the payload, which the record type allows.
    length: usize,
}

/// Decodes and validates a record header: its magic, version, type, flags, payload length, and,
/// for a bootstrap record, its zero identity.
fn parse_header(bytes: &[u8; OUTER_HEADER_LEN]) -> Result<Header, RecordError> {
    let magic: [u8; 4] = array(&bytes[..4]);
    if magic != OUTER_MAGIC {
        return Err(RecordError::Magic(magic));
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != OUTER_VERSION {
        return Err(RecordError::Version(version));
    }
    let record_type = RecordType::from_code(bytes[6]).ok_or(RecordError::Type(bytes[6]))?;
    if bytes[7] != 0 {
        return Err(RecordError::Flags(bytes[7]));
    }
    let header = Header {
        record_type,
        instance_id: array(&bytes[8..24]),
        epoch: u64::from_le_bytes(array(&bytes[24..32])),
        sequence: u64::from_le_bytes(array(&bytes[32..40])),
        length: u32::from_le_bytes(array(&bytes[40..44])) as usize,
    };
    check_length(record_type, header.length)?;
    check_bootstrap(
        record_type,
        &header.instance_id,
        header.epoch,
        header.sequence,
    )?;
    Ok(header)
}

/// Checks that `record_type` allows a payload of `length` bytes.
fn check_length(record_type: RecordType, length: usize) -> Result<(), RecordError> {
    if record_type.allows_length(length) {
        Ok(())
    } else {
        Err(RecordError::Length {
            record_type,
            length,
        })
    }
}

/// Checks that a bootstrap record carries a zero instance ID, epoch, and sequence.
fn check_bootstrap(
    record_type: RecordType,
    instance_id: &[u8; 16],
    epoch: u64,
    sequence: u64,
) -> Result<(), RecordError> {
    if record_type.is_bootstrap() && (*instance_id != [0; 16] || epoch != 0 || sequence != 0) {
        Err(RecordError::BootstrapIdentity(record_type))
    } else {
        Ok(())
    }
}

/// Checks the value of an ACK, CREDIT, or ERROR payload, whose length the caller checked.
fn check_payload(record_type: RecordType, payload: &[u8]) -> Result<(), RecordError> {
    match record_type {
        RecordType::Ack => {
            let window = value(payload);
            if (OUTER_MIN_RECEIVE_WINDOW..=OUTER_MAX_RECEIVE_WINDOW).contains(&window) {
                Ok(())
            } else {
                Err(RecordError::ReceiveWindow(window))
            }
        }
        RecordType::Credit => {
            let bytes = value(payload);
            if (1..=OUTER_MAX_RECEIVE_WINDOW).contains(&bytes) {
                Ok(())
            } else {
                Err(RecordError::Credit(bytes))
            }
        }
        RecordType::Error => {
            let code = value(payload);
            match ErrorCode::from_code(code) {
                Some(_) => Ok(()),
                None => Err(RecordError::ErrorCode(code)),
            }
        }
        _ => Ok(()),
    }
}

/// Decodes the little-endian `u32` of a four-byte ACK, CREDIT, or ERROR payload.
fn value(payload: &[u8]) -> u32 {
    u32::from_le_bytes(array(payload))
}

/// Incremental parser of an NVXS byte stream.
///
/// [`push`](Self::push) takes bytes as they arrive, in pieces of any size, and returns each record
/// once its last byte arrives. The parser holds at most one partial record: up to
/// [`OUTER_HEADER_LEN`] header bytes, then a payload of at most [`OUTER_MAX_PAYLOAD`] bytes, whose
/// buffer it allocates, at the length that the header declares, only once the header passed
/// validation. Bytes after a complete record stay with the caller.
///
/// The parser validates what [`encode_record`] checks. A malformed record ends the stream, which
/// cannot be resynchronized: the parser discards its partial record and fails every later call
/// with the same error.
#[derive(Clone)]
pub struct RecordParser {
    /// Header bytes of the record in progress.
    header: [u8; OUTER_HEADER_LEN],
    /// Number of bytes of `header` that arrived.
    header_len: usize,
    /// Validated header and the payload bytes that arrived, once the header is complete.
    body: Option<(Header, Vec<u8>)>,
    /// Error that ended the stream.
    error: Option<RecordError>,
}

impl RecordParser {
    /// Creates a parser at the start of a stream.
    pub const fn new() -> Self {
        Self {
            header: [0; OUTER_HEADER_LEN],
            header_len: 0,
            body: None,
            error: None,
        }
    }

    /// Consumes bytes from the front of `input` until a record completes or `input` runs out.
    ///
    /// Returns the number of bytes consumed and the record that the last of them completed, if
    /// any. Bytes after that record are not consumed. A failure ends the stream.
    pub fn push(&mut self, input: &[u8]) -> Result<(usize, Option<Record>), RecordError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.accept(input).inspect_err(|error| {
            self.header_len = 0;
            self.body = None;
            self.error = Some(error.clone());
        })
    }

    /// Returns how many more bytes complete the header or the payload in progress: from 1
    /// through [`OUTER_HEADER_LEN`] for a header, or through [`OUTER_MAX_PAYLOAD`] for a payload.
    ///
    /// A reader that never reads more than this leaves every byte after the record in progress
    /// unread, for example so that polling the stream for readiness stays accurate.
    pub fn needed(&self) -> usize {
        match &self.body {
            Some((header, payload)) => header.length - payload.len(),
            None => OUTER_HEADER_LEN - self.header_len,
        }
    }

    /// Returns the number of bytes of the partial record that the parser holds.
    pub fn buffered(&self) -> usize {
        match &self.body {
            Some((_, payload)) => OUTER_HEADER_LEN + payload.len(),
            None => self.header_len,
        }
    }

    /// Consumes bytes of the record in progress; [`push`](Self::push) records a failure.
    fn accept(&mut self, input: &[u8]) -> Result<(usize, Option<Record>), RecordError> {
        let mut consumed = 0;
        if self.body.is_none() {
            let take = (OUTER_HEADER_LEN - self.header_len).min(input.len());
            self.header[self.header_len..self.header_len + take].copy_from_slice(&input[..take]);
            self.header_len += take;
            consumed = take;
            if self.header_len < OUTER_HEADER_LEN {
                return Ok((consumed, None));
            }
            let header = parse_header(&self.header)?;
            self.body = Some((header, Vec::with_capacity(header.length)));
        }
        let Some((header, payload)) = &mut self.body else {
            return Ok((consumed, None));
        };
        let take = (header.length - payload.len()).min(input.len() - consumed);
        payload.extend_from_slice(&input[consumed..consumed + take]);
        consumed += take;
        if payload.len() < header.length {
            return Ok((consumed, None));
        }
        let Some((header, payload)) = self.body.take() else {
            return Ok((consumed, None));
        };
        self.header_len = 0;
        check_payload(header.record_type, &payload)?;
        let record = Record {
            record_type: header.record_type,
            instance_id: header.instance_id,
            epoch: header.epoch,
            sequence: header.sequence,
            payload,
        };
        Ok((consumed, Some(record)))
    }
}

impl Default for RecordParser {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for RecordParser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordParser")
            .field("buffered", &self.buffered())
            .field("needed", &self.needed())
            .field("error", &self.error)
            .finish()
    }
}

#[cfg(test)]
mod tests;
