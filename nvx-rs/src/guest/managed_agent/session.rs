use std::cell::{Cell, RefCell};
use std::io::{self, Read, Write};
use std::process::ExitStatus;

use crate::guest::init::Runtime;
use crate::guest::nvxs::{GuestConnection, GuestConnectionError};
use crate::guest::sys::{self, Errno, FileDescriptor, Pollable};
use crate::protocol::nvxs::{GuestEvent, RecordError, RecordType, SessionError};
use crate::protocol::{self, APP_ERROR, OUTER_MAX_PAYLOAD, OUTER_RECEIVE_WINDOW};

/// The agent's control session: the guest's role of the shared NVXS session on the control
/// console, and the NVXC frames that its DATA records carry.
pub(crate) struct Session<'r> {
    /// The guest's NVXS connection on the owned control-console descriptor.
    connection: GuestConnection<Control<'r>>,
    /// Error of the connection's latest failure, which a connection that a failure ended reports
    /// again.
    last_error: Option<Errno>,
}

/// Byte transport only: the existing connection still owns all record framing.
struct Control<'r> {
    descriptor: FileDescriptor,
    runtime: Option<RefCell<&'r mut Runtime>>,
    leader: Cell<u32>,
    status: Cell<Option<(u32, ExitStatus)>>,
    outcome: RefCell<Result<bool, String>>,
}

impl Control<'_> {
    fn service_leader(&self) -> Result<(), Errno> {
        if self.service(|id, status| {
            // Keep the first leader status across partial I/O, not unrelated orphans.
            if id == self.leader.get() && self.status.get().is_none() {
                self.status.set(Some((id, status)));
            }
        })? {
            return Err(Errno::ECANCELED);
        }
        Ok(())
    }

    fn service(&self, observed: impl FnMut(u32, ExitStatus)) -> Result<bool, Errno> {
        if self.outcome.borrow().is_err() {
            return Err(Errno::ECHILD);
        }
        if let Some(runtime) = &self.runtime {
            *self.outcome.borrow_mut() = runtime.borrow_mut().service(observed);
        } else {
            super::exec::collect(observed);
        }
        self.outcome
            .borrow()
            .as_ref()
            .copied()
            .map_err(|_| Errno::ECHILD)
    }
}

impl Read for Control<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Read::read(&mut self.descriptor, bytes);
        }
        let Some(runtime) = &self.runtime else {
            return Read::read(&mut self.descriptor, bytes);
        };
        loop {
            self.service_leader()?;
            let signals = sys::poll(
                [Pollable::File(&self.descriptor), runtime.borrow().wake()],
                None,
            )?[1];
            if signals.error {
                return Err(Errno::EIO.into());
            }
            if !signals.readable {
                // A control-only wake includes HUP, which PollEvents does not expose.
                match self.descriptor.read(bytes) {
                    Err(Errno::EAGAIN) => {}
                    result => return result.map_err(io::Error::from),
                }
            }
        }
    }
}

impl Write for Control<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(runtime) = &self.runtime else {
            return Write::write(&mut self.descriptor, bytes);
        };
        if bytes.is_empty() {
            return Write::write(&mut self.descriptor, bytes);
        }
        loop {
            self.service_leader()?;
            match self.descriptor.write(bytes) {
                Err(Errno::EAGAIN) => loop {
                    let events = sys::poll(
                        [
                            Pollable::Writable(&self.descriptor),
                            runtime.borrow().wake(),
                        ],
                        None,
                    )?;
                    if events[1].error {
                        return Err(Errno::EIO.into());
                    }
                    self.service_leader()?;
                    if events[0].writable || !events[1].readable {
                        break;
                    }
                },
                result => return result.map_err(io::Error::from),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.descriptor)
    }
}

/// Record from the broker that the session accepted.
pub(crate) enum Event {
    /// The broker reset the session. The agent abandons the earlier epoch's work, then
    /// acknowledges the new epoch with [`Session::acknowledge`].
    Reset,
    /// The payload of a DATA record: one NVXC frame, which [`Session::accept_request`] decodes.
    Data(Vec<u8>),
}

/// A record from the broker that ended the session, classified by the stage at which the C agent
/// reported it.
pub(crate) enum ReceiveFailure {
    /// The stream failed or ended, or a record header was malformed, with the error to report.
    Read(Errno),
    /// A record of a type that the agent does not take in its state, with the type's code.
    Type(u8),
    /// A RESET that the session refused.
    Reset,
    /// A DATA record that the session refused.
    Data(DataFailure),
}

/// Protocol stage that rejected an outer DATA record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataFailureKind {
    /// Host sequence mismatch.
    Sequence,
    /// Instance identity or epoch mismatch.
    Identity,
    /// A payload beyond the receive window, which the C agent did not check.
    Window,
    /// Malformed application frame or request metadata.
    Application,
    /// Failure to replenish host receive credit.
    Credit,
}

impl DataFailureKind {
    /// Returns the diagnostic stage that reports this failure, as the C agent names it.
    pub(crate) fn stage(self) -> &'static str {
        match self {
            Self::Sequence => "data-sequence",
            Self::Identity => "data-identity",
            Self::Window => "data-window",
            Self::Application => "data-application",
            Self::Credit => "data-credit",
        }
    }
}

/// Typed failed-DATA validation with diagnostic metadata.
pub(crate) struct DataFailure {
    /// Validation stage.
    pub(crate) kind: DataFailureKind,
    /// C-compatible stage status.
    pub(crate) status: i32,
    /// Underlying protocol or I/O error.
    error: Errno,
}

impl DataFailure {
    /// Consumes the metadata and returns the underlying error.
    pub(crate) fn into_error(self) -> Errno {
        self.error
    }
}

impl<'r> Session<'r> {
    /// Starts unattached session state on an owned control descriptor.
    pub(crate) fn new(descriptor: FileDescriptor) -> Self {
        Self::open(descriptor, None)
    }

    fn open(descriptor: FileDescriptor, runtime: Option<&'r mut Runtime>) -> Self {
        Self {
            connection: GuestConnection::new(Control {
                descriptor,
                runtime: runtime.map(RefCell::new),
                leader: Cell::new(0),
                status: Cell::new(None),
                outcome: RefCell::new(Ok(false)),
            }),
            last_error: None,
        }
    }

    /// Borrows init's sole collector for the lifetime of this control session.
    pub(crate) fn with_runtime(
        descriptor: FileDescriptor,
        runtime: &'r mut Runtime,
    ) -> Result<Self, String> {
        let session = Self::open(descriptor, Some(runtime));
        session
            .connection
            .get_ref()
            .service(|_, _| {})
            .map_err(|_| {
                session
                    .runtime_failure()
                    .expect("the collector retained its failure")
            })?;
        session
            .descriptor()
            .set_nonblocking()
            .map_err(|error| format!("cannot configure layered control: {}", error.desc()))?;
        Ok(session)
    }

    /// Registers the newly launched leader before any control I/O can collect it.
    pub(crate) fn watch(&self, id: u32) {
        self.connection.get_ref().leader.set(id);
        self.connection.get_ref().status.set(None);
    }

    /// Takes a status collected while a control record was being read or written.
    pub(crate) fn take_status(&self) -> Option<(u32, ExitStatus)> {
        self.connection.get_ref().status.take()
    }

    /// Collects in the caller's existing position, after handing over any earlier status.
    pub(crate) fn collect(&self, mut observed: impl FnMut(u32, ExitStatus)) -> Result<(), Errno> {
        if let Some((id, status)) = self.take_status() {
            observed(id, status);
        }
        if self.connection.get_ref().service(observed)? {
            return Err(Errno::ECANCELED);
        }
        Ok(())
    }

    /// A helper failure overrides even an already-latched normal termination.
    pub(crate) fn stopping(&self) -> bool {
        matches!(*self.connection.get_ref().outcome.borrow(), Ok(true))
    }

    /// Returns the original init diagnostic to its shutdown owner.
    pub(crate) fn runtime_failure(&self) -> Option<String> {
        std::mem::replace(
            &mut *self.connection.get_ref().outcome.borrow_mut(),
            Ok(false),
        )
        .err()
    }

    /// Borrows the control descriptor for polling and TTY operations. The connection never reads
    /// past the record in progress and holds no partial record between calls, so polling sees
    /// every record that the session has not returned.
    pub(crate) fn descriptor(&self) -> &FileDescriptor {
        &self.connection.get_ref().descriptor
    }

    /// Sends the initial guest-attach record.
    pub(crate) fn send_attach(&mut self) -> Result<(), Errno> {
        let attached = self.connection.attach();
        attached.map_err(|error| self.error(error))
    }

    /// Blocks until the broker's next record completes, and returns what the session accepted.
    pub(crate) fn next_event(&mut self) -> Result<Event, ReceiveFailure> {
        match self.connection.next_event() {
            Ok(GuestEvent::Reset(_)) => Ok(Event::Reset),
            Ok(GuestEvent::Data(payload)) => Ok(Event::Data(payload)),
            Err(error) => Err(self.receive_failure(error)),
        }
    }

    /// Acknowledges the broker's reset with NVX's receive window.
    pub(crate) fn acknowledge(&mut self) -> Result<(), Errno> {
        let acknowledged = self.connection.acknowledge(OUTER_RECEIVE_WINDOW);
        acknowledged.map_err(|error| self.error(error))
    }

    /// Decodes the NVXC request that a DATA payload carries, then returns the payload's bytes to
    /// the receive window before the agent serves the request, as the C agent did.
    pub(crate) fn accept_request<'a>(
        &mut self,
        payload: &'a [u8],
    ) -> Result<protocol::AppFrame<'a>, DataFailure> {
        // A DATA payload has at most `OUTER_MAX_PAYLOAD` bytes, so its length fits either type.
        let status = payload.len() as i32;
        let frame = protocol::decode_app(payload)
            .map_err(|_| data_failure(DataFailureKind::Application, status))?;
        if frame.request_id == 0 || frame.status != 0 {
            return Err(data_failure(DataFailureKind::Application, status));
        }
        let credited = self.connection.credit(payload.len() as u32);
        credited.map_err(|error| DataFailure {
            kind: DataFailureKind::Credit,
            status,
            error: self.error(error),
        })?;
        Ok(frame)
    }

    /// Encodes and sends one application frame, in one DATA record.
    pub(crate) fn send_app(
        &mut self,
        kind: u8,
        request_id: u64,
        status: i32,
        payload: &[u8],
    ) -> Result<(), Errno> {
        if protocol::APP_HEADER_LEN
            .checked_add(payload.len())
            .is_none_or(|length| length > OUTER_MAX_PAYLOAD)
        {
            return Err(Errno::EMSGSIZE);
        }
        let frame = protocol::encode_app(kind, request_id, status, payload);
        let sent = self.connection.send(&frame);
        sent.map_err(|error| self.error(error))
    }

    /// Sends one application ERROR frame with an ASCII category.
    pub(crate) fn send_error(
        &mut self,
        request_id: u64,
        status: i32,
        category: &str,
    ) -> Result<(), Errno> {
        self.send_app(APP_ERROR, request_id, status, category.as_bytes())
    }

    /// Classifies a failure to receive the broker's next record by the stage at which the C agent
    /// reported it.
    fn receive_failure(&mut self, error: GuestConnectionError) -> ReceiveFailure {
        let epoch = self
            .connection
            .session()
            .identity()
            .map_or(0, |identity| identity.epoch);
        let failure = match &error {
            GuestConnectionError::Protocol(error) => classify(error, epoch),
            _ => None,
        };
        let error = self.error(error);
        failure.unwrap_or(ReceiveFailure::Read(error))
    }

    /// Converts a failure of the connection into the error that the agent reports. A connection
    /// that an earlier failure ended reports that failure's error again, as another write to a
    /// failed descriptor would fail again.
    fn error(&mut self, error: GuestConnectionError) -> Errno {
        let error = match error {
            GuestConnectionError::Ended => return self.last_error.unwrap_or(Errno::EPROTO),
            GuestConnectionError::Io(error) => io_error(&error),
            // `FileDescriptor::read_exact` reports end of file as `EIO`, inside a record or not.
            GuestConnectionError::Closed
            | GuestConnectionError::Protocol(SessionError::Truncated { .. }) => Errno::EIO,
            GuestConnectionError::Protocol(_) => Errno::EPROTO,
        };
        self.last_error = Some(error);
        error
    }
}

/// Returns the stage at which the C agent reported a record that the session refused, or `None`
/// for one that it reported as a failure to read.
///
/// The C agent read each record whole, checking only its header's magic, version, flags, and
/// length limit, then took RESET and DATA records and reported every other type. It checked a
/// RESET's payload and epoch, then a DATA record's sequence, identity, and NVXC frame. A record
/// with one fault thus reports the C agent's stage and status, except for the faults that only
/// the shared session checks: a RESET without an instance ID, of an earlier epoch, or at the
/// last sequence number, DATA at the last sequence number, which the C agent took and followed
/// with sequence number 0 and which reports `data-sequence`, DATA before the first RESET, and DATA
/// beyond the receive window.
fn classify(error: &SessionError, epoch: u64) -> Option<ReceiveFailure> {
    let data = |kind, status| Some(ReceiveFailure::Data(data_failure(kind, status)));
    match *error {
        SessionError::Record(ref error) => match *error {
            RecordError::Magic(_) | RecordError::Version(_) | RecordError::Flags(_) => None,
            RecordError::Length { length, .. } if length > OUTER_MAX_PAYLOAD => None,
            RecordError::Type(code) => Some(ReceiveFailure::Type(code)),
            RecordError::Length {
                record_type: RecordType::Reset,
                ..
            } => Some(ReceiveFailure::Reset),
            // The only other length that DATA does not allow is 0, which holds no NVXC frame.
            RecordError::Length {
                record_type: RecordType::Data,
                length,
            } => data(DataFailureKind::Application, length as i32),
            RecordError::Length { record_type, .. }
            | RecordError::BootstrapIdentity(record_type) => {
                Some(ReceiveFailure::Type(record_type.code()))
            }
            RecordError::ErrorCode(_) => Some(ReceiveFailure::Type(RecordType::Error.code())),
            RecordError::ReceiveWindow(_) => Some(ReceiveFailure::Type(RecordType::Ack.code())),
            RecordError::Credit(_) => Some(ReceiveFailure::Type(RecordType::Credit.code())),
        },
        SessionError::Unexpected(record_type) => Some(ReceiveFailure::Type(record_type.code())),
        // Only a RESET at the last sequence number leaves none for the broker's next record.
        SessionError::MissingIdentity(_)
        | SessionError::StaleEpoch { .. }
        | SessionError::SequenceOverflow(RecordType::Reset) => Some(ReceiveFailure::Reset),
        // DATA at the last sequence number, which leaves none for the broker's next record, is
        // reported as DATA out of sequence, with its sequence number.
        SessionError::SequenceOverflow(RecordType::Data) => {
            data(DataFailureKind::Sequence, u64::MAX as i32)
        }
        SessionError::Sequence { actual, .. } => data(DataFailureKind::Sequence, actual as i32),
        SessionError::Epoch { actual, .. } => data(DataFailureKind::Identity, actual as i32),
        // With one fault, a record of another instance carries the session's epoch, which the C
        // agent reported.
        SessionError::Instance { .. } => data(DataFailureKind::Identity, epoch as i32),
        SessionError::Window { length, .. } => data(DataFailureKind::Window, length as i32),
        SessionError::Truncated { .. }
        | SessionError::Credit { .. }
        | SessionError::SequenceOverflow(_)
        | SessionError::InvalidState => None,
    }
}

/// Returns the `errno` of an I/O failure, or `EIO` for one without, such as a write of no bytes.
fn io_error(error: &io::Error) -> Errno {
    error.raw_os_error().map_or(Errno::EIO, Errno::from_raw)
}

/// Constructs one typed DATA validation failure.
fn data_failure(kind: DataFailureKind, status: i32) -> DataFailure {
    DataFailure {
        kind,
        status,
        error: Errno::EPROTO,
    }
}
