//! The host's role in an NVXS session.

use crate::protocol::CAPABILITY_LEN;

use super::{
    ErrorCode, Identity, Record, RecordParser, RecordType, SessionError, check_identity,
    check_sequence, encode_data, encode_record, next_sequence, record_identity,
};

/// State of a [`HostSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HostState {
    /// The host has not sent its attachment.
    Detached,
    /// The host sent its attachment and waits for the broker to attach it.
    Attaching,
    /// The broker attached the host, and DATA flows both ways.
    Attached,
    /// The broker refused the host's capability. The session has ended.
    Rejected,
    /// The broker reset the session, which ended the host's attachment.
    Reset,
    /// The stream ended at a record boundary. The session has ended.
    Closed,
    /// The broker violated the protocol, or the stream ended inside a record. The session has
    /// ended.
    Failed,
}

/// Event that a [`HostSession`] reports for a record from the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostEvent {
    /// The broker accepted the host's capability, and waits for the guest to acknowledge the
    /// session's reset before it attaches the host.
    Wait,
    /// The broker attached the host to the session with this identity.
    Attached(Identity),
    /// The broker refused the host, which ended the session.
    Rejected(ErrorCode),
    /// The payload of a DATA record from the guest.
    Data(Vec<u8>),
    /// The broker reset the session, which ended the host's attachment. The identity is the
    /// session's new one.
    Reset(Identity),
}

/// The host's role in an NVXS session: its state, identity, sequence numbers, and the parser of
/// the broker's records, without I/O.
///
/// [`attach`](Self::attach) encodes the host's attachment, [`receive`](Self::receive) parses the
/// broker's records from bytes in pieces of any size and reports their [`HostEvent`]s, and
/// [`send`](Self::send) encodes application bytes in DATA records once the broker attached the
/// host. The module documentation describes the protocol.
#[derive(Debug, Clone)]
pub(crate) struct HostSession {
    /// State of the session.
    pub(super) state: HostState,
    /// Parser of the broker's records.
    parser: RecordParser,
    /// Identity that READY attached the host to.
    pub(super) identity: Option<Identity>,
    /// Sequence number of the broker's next record.
    receive_sequence: u64,
    /// Sequence number of the host's next DATA record.
    send_sequence: u64,
}

impl HostSession {
    /// Creates a session that has not attached yet.
    pub(crate) const fn new() -> Self {
        Self {
            state: HostState::Detached,
            parser: RecordParser::new(),
            identity: None,
            receive_sequence: 0,
            send_sequence: 0,
        }
    }

    /// Appends the HOST_ATTACH record with `capability` to `output`, which the caller sends to
    /// the broker before anything else.
    pub(crate) fn attach(
        &mut self,
        capability: &[u8; CAPABILITY_LEN],
        output: &mut Vec<u8>,
    ) -> Result<(), SessionError> {
        if self.state != HostState::Detached {
            return Err(SessionError::InvalidState);
        }
        encode_record(RecordType::HostAttach, &[0; 16], 0, 0, capability, output)?;
        self.state = HostState::Attaching;
        Ok(())
    }

    /// Consumes bytes from the broker until a record completes or `input` runs out.
    ///
    /// Returns the number of bytes consumed and the event of the record that the last of them
    /// completed, if any. Bytes after that record are not consumed. A failure ends the session.
    pub(crate) fn receive(
        &mut self,
        input: &[u8],
    ) -> Result<(usize, Option<HostEvent>), SessionError> {
        if !matches!(self.state, HostState::Attaching | HostState::Attached) {
            return Err(SessionError::InvalidState);
        }
        let (consumed, record) = self
            .parser
            .push(input)
            .map_err(|error| self.fail(error.into()))?;
        let Some(record) = record else {
            return Ok((consumed, None));
        };
        let event = self.accept(record).map_err(|error| self.fail(error))?;
        Ok((consumed, Some(event)))
    }

    /// Appends a DATA record with at most [`OUTER_MAX_PAYLOAD`](crate::protocol::OUTER_MAX_PAYLOAD)
    /// bytes from the front of `data` to `output`, and returns how many bytes it took.
    ///
    /// `data` must not be empty. A message of more bytes takes one call per record.
    pub(crate) fn send(
        &mut self,
        data: &[u8],
        output: &mut Vec<u8>,
    ) -> Result<usize, SessionError> {
        let (HostState::Attached, Some(identity)) = (self.state, self.identity) else {
            return Err(SessionError::InvalidState);
        };
        encode_data(&identity, &mut self.send_sequence, data, output)
    }

    /// Ends the session because the stream from the broker ended.
    ///
    /// Fails with [`SessionError::Truncated`] if a record was in progress. A session that
    /// already ended keeps its state.
    pub(crate) fn disconnect(&mut self) -> Result<(), SessionError> {
        if !matches!(
            self.state,
            HostState::Detached | HostState::Attaching | HostState::Attached
        ) {
            return Ok(());
        }
        let buffered = self.parser.buffered();
        if buffered != 0 {
            return Err(self.fail(SessionError::Truncated { buffered }));
        }
        self.state = HostState::Closed;
        Ok(())
    }

    /// Validates a complete record from the broker against the session and applies it: its type
    /// against the state, then its sequence number, then its identity.
    fn accept(&mut self, record: Record) -> Result<HostEvent, SessionError> {
        let record_type = record.record_type();
        let allowed = match self.state {
            HostState::Attaching => matches!(
                record_type,
                RecordType::Wait | RecordType::Ready | RecordType::Error
            ),
            HostState::Attached => matches!(record_type, RecordType::Data | RecordType::Reset),
            _ => false,
        };
        if !allowed {
            return Err(SessionError::Unexpected(record_type));
        }
        check_sequence(&record, self.receive_sequence)?;
        let next = next_sequence(self.receive_sequence, record_type)?;
        let event = match (record_type, self.identity) {
            (RecordType::Wait, _) => HostEvent::Wait,
            (RecordType::Ready, _) => {
                let identity =
                    record_identity(&record).ok_or(SessionError::MissingIdentity(record_type))?;
                self.identity = Some(identity);
                self.state = HostState::Attached;
                HostEvent::Attached(identity)
            }
            (RecordType::Error, _) => {
                let code = record
                    .error_code()
                    .ok_or(SessionError::Unexpected(record_type))?;
                self.state = HostState::Rejected;
                HostEvent::Rejected(code)
            }
            (RecordType::Data, Some(identity)) => {
                check_identity(&record, &identity)?;
                HostEvent::Data(record.into_payload())
            }
            (RecordType::Reset, Some(identity)) => {
                let reset =
                    record_identity(&record).ok_or(SessionError::MissingIdentity(record_type))?;
                if reset.instance_id != identity.instance_id {
                    return Err(SessionError::Instance {
                        expected: identity.instance_id,
                        actual: reset.instance_id,
                    });
                }
                if reset.epoch <= identity.epoch {
                    return Err(SessionError::StaleEpoch {
                        current: identity.epoch,
                        actual: reset.epoch,
                    });
                }
                self.state = HostState::Reset;
                HostEvent::Reset(reset)
            }
            _ => return Err(SessionError::Unexpected(record_type)),
        };
        self.receive_sequence = next;
        Ok(event)
    }

    /// Ends the session with `error`, and returns it.
    fn fail(&mut self, error: SessionError) -> SessionError {
        self.state = HostState::Failed;
        error
    }
}

impl Default for HostSession {
    fn default() -> Self {
        Self::new()
    }
}
