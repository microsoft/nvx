//! The guest's role in an NVXS session.

use super::{
    Identity, Record, RecordParser, RecordType, SessionError, check_identity, check_sequence,
    encode_data, encode_next, encode_record, next_sequence, record_identity,
};

/// State of a [`GuestSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GuestState {
    /// The guest has not sent its attachment.
    Detached,
    /// The guest sent its attachment and waits for the broker's first RESET.
    Attaching,
    /// The broker reset the session, and the guest has not acknowledged the new epoch.
    Resetting,
    /// The guest acknowledged the epoch, and DATA and CREDIT flow.
    Active,
    /// The stream ended at a record boundary. The session has ended.
    Closed,
    /// The broker violated the protocol, or the stream ended inside a record. The session has
    /// ended.
    Failed,
}

/// Event that a [`GuestSession`] reports for a record from the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GuestEvent {
    /// The broker reset the session to this identity. The guest abandons the work of the earlier
    /// epoch, if any, then acknowledges the new one with [`GuestSession::acknowledge`].
    Reset(Identity),
    /// The payload of a DATA record from the host. Once the guest consumed its bytes, it returns
    /// them to its receive window with [`GuestSession::credit`].
    Data(Vec<u8>),
}

/// The guest's role in an NVXS session: its state, identity, sequence numbers, receive window,
/// and the parser of the broker's records, without I/O.
///
/// [`attach`](Self::attach) encodes the guest's attachment, [`receive`](Self::receive) parses the
/// broker's records from bytes in pieces of any size and reports their [`GuestEvent`]s,
/// [`acknowledge`](Self::acknowledge) activates the epoch of a reset, and [`send`](Self::send)
/// and [`credit`](Self::credit) encode DATA and CREDIT records in an active epoch. The module
/// documentation describes the protocol.
#[derive(Debug, Clone)]
pub(crate) struct GuestSession {
    /// State of the session.
    state: GuestState,
    /// Parser of the broker's records.
    parser: RecordParser,
    /// Identity of the current epoch, from its RESET.
    identity: Option<Identity>,
    /// Sequence number of the broker's next record.
    receive_sequence: u64,
    /// Sequence number of the guest's next record.
    send_sequence: u64,
    /// Receive window that the guest acknowledged the epoch with.
    pub(super) window: u32,
    /// DATA bytes of the epoch that the guest received and has not credited.
    pub(super) outstanding: u32,
}

impl GuestSession {
    /// Creates a session that has not attached yet.
    pub(crate) const fn new() -> Self {
        Self {
            state: GuestState::Detached,
            parser: RecordParser::new(),
            identity: None,
            receive_sequence: 0,
            send_sequence: 0,
            window: 0,
            outstanding: 0,
        }
    }

    /// Returns the state of the session.
    pub(crate) fn state(&self) -> GuestState {
        self.state
    }

    /// Returns the identity of the current epoch, once the broker reset the session.
    pub(crate) fn identity(&self) -> Option<Identity> {
        self.identity
    }

    /// Returns how many more bytes complete the broker's record in progress; see
    /// [`RecordParser::needed`].
    pub(crate) fn needed(&self) -> usize {
        self.parser.needed()
    }

    /// Appends the GUEST_ATTACH record to `output`, which the caller sends to the broker before
    /// anything else.
    pub(crate) fn attach(&mut self, output: &mut Vec<u8>) -> Result<(), SessionError> {
        if self.state != GuestState::Detached {
            return Err(SessionError::InvalidState);
        }
        encode_record(RecordType::GuestAttach, &[0; 16], 0, 0, &[], output)?;
        self.state = GuestState::Attaching;
        Ok(())
    }

    /// Consumes bytes from the broker until a record completes or `input` runs out.
    ///
    /// Returns the number of bytes consumed and the event of the record that the last of them
    /// completed, if any. Bytes after that record are not consumed. A failure ends the session.
    pub(crate) fn receive(
        &mut self,
        input: &[u8],
    ) -> Result<(usize, Option<GuestEvent>), SessionError> {
        if !matches!(
            self.state,
            GuestState::Attaching | GuestState::Resetting | GuestState::Active
        ) {
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

    /// Appends the ACK record that acknowledges the session's reset to `output`, advertising a
    /// receive window of `window` bytes, from
    /// [`OUTER_MIN_RECEIVE_WINDOW`](crate::protocol::OUTER_MIN_RECEIVE_WINDOW) through
    /// [`OUTER_MAX_RECEIVE_WINDOW`](crate::protocol::OUTER_MAX_RECEIVE_WINDOW), and activates the
    /// epoch.
    ///
    /// The guest acknowledges once it abandoned the work of the earlier epoch and can take DATA
    /// of the new one. The broker sends none before.
    pub(crate) fn acknowledge(
        &mut self,
        window: u32,
        output: &mut Vec<u8>,
    ) -> Result<(), SessionError> {
        let (GuestState::Resetting, Some(identity)) = (self.state, self.identity) else {
            return Err(SessionError::InvalidState);
        };
        encode_record(
            RecordType::Ack,
            &identity.instance_id,
            identity.epoch,
            0,
            &window.to_le_bytes(),
            output,
        )?;
        self.send_sequence = 1;
        self.window = window;
        self.outstanding = 0;
        self.state = GuestState::Active;
        Ok(())
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
        let (GuestState::Active, Some(identity)) = (self.state, self.identity) else {
            return Err(SessionError::InvalidState);
        };
        encode_data(&identity, &mut self.send_sequence, data, output)
    }

    /// Appends a CREDIT record that returns `bytes` consumed DATA bytes to the receive window to
    /// `output`. `bytes` is from 1 through the DATA bytes of the epoch that the guest received and
    /// has not credited.
    pub(crate) fn credit(&mut self, bytes: u32, output: &mut Vec<u8>) -> Result<(), SessionError> {
        let (GuestState::Active, Some(identity)) = (self.state, self.identity) else {
            return Err(SessionError::InvalidState);
        };
        if bytes == 0 || bytes > self.outstanding {
            return Err(SessionError::Credit {
                outstanding: self.outstanding,
                bytes,
            });
        }
        encode_next(
            RecordType::Credit,
            &identity,
            &mut self.send_sequence,
            &bytes.to_le_bytes(),
            output,
        )?;
        self.outstanding -= bytes;
        Ok(())
    }

    /// Ends the session because the stream from the broker ended.
    ///
    /// Fails with [`SessionError::Truncated`] if a record was in progress. A session that
    /// already ended keeps its state.
    pub(crate) fn disconnect(&mut self) -> Result<(), SessionError> {
        if matches!(self.state, GuestState::Closed | GuestState::Failed) {
            return Ok(());
        }
        let buffered = self.parser.buffered();
        if buffered != 0 {
            return Err(self.fail(SessionError::Truncated { buffered }));
        }
        self.state = GuestState::Closed;
        Ok(())
    }

    /// Validates a complete record from the broker against the session and applies it.
    fn accept(&mut self, record: Record) -> Result<GuestEvent, SessionError> {
        match (self.state, record.record_type()) {
            (
                GuestState::Attaching | GuestState::Resetting | GuestState::Active,
                RecordType::Reset,
            ) => self.reset(&record),
            (GuestState::Active, RecordType::Data) => self.data(record),
            (_, record_type) => Err(SessionError::Unexpected(record_type)),
        }
    }

    /// Starts the epoch of a RESET, which carries an identity and, for the session's instance, a
    /// later epoch. The broker's next record follows the RESET's sequence number.
    fn reset(&mut self, record: &Record) -> Result<GuestEvent, SessionError> {
        let identity =
            record_identity(record).ok_or(SessionError::MissingIdentity(RecordType::Reset))?;
        if let Some(current) = self.identity
            && current.instance_id == identity.instance_id
            && identity.epoch <= current.epoch
        {
            return Err(SessionError::StaleEpoch {
                current: current.epoch,
                actual: identity.epoch,
            });
        }
        let next = next_sequence(record.sequence(), RecordType::Reset)?;
        self.identity = Some(identity);
        self.receive_sequence = next;
        self.send_sequence = 0;
        self.window = 0;
        self.outstanding = 0;
        self.state = GuestState::Resetting;
        Ok(GuestEvent::Reset(identity))
    }

    /// Accepts a DATA record at the next sequence number, with the session's identity, that fits
    /// the receive window.
    fn data(&mut self, record: Record) -> Result<GuestEvent, SessionError> {
        let Some(identity) = self.identity else {
            return Err(SessionError::Unexpected(RecordType::Data));
        };
        check_sequence(&record, self.receive_sequence)?;
        check_identity(&record, &identity)?;
        let next = next_sequence(self.receive_sequence, RecordType::Data)?;
        // A DATA payload has at most `OUTER_MAX_PAYLOAD` bytes, so its length fits a `u32`.
        let length = record.payload().len() as u32;
        if length > self.window.saturating_sub(self.outstanding) {
            return Err(SessionError::Window {
                window: self.window,
                outstanding: self.outstanding,
                length,
            });
        }
        self.receive_sequence = next;
        self.outstanding += length;
        Ok(GuestEvent::Data(record.into_payload()))
    }

    /// Ends the session with `error`, and returns it.
    fn fail(&mut self, error: SessionError) -> SessionError {
        self.state = GuestState::Failed;
        error
    }
}

impl Default for GuestSession {
    fn default() -> Self {
        Self::new()
    }
}
