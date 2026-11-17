use std::cell::{Cell, RefCell};
use std::io::{self, Read, Write};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::guest::init::{self, Capture, CaptureStep, RestoreHandoff, Restored, Runtime};
use crate::guest::nvxs::{GuestConnection, GuestConnectionError};
use crate::guest::restore::{GuestRestoreAction, GuestRestoreInput, GuestRestoreState};
use crate::guest::snapshot::{Captured, Disposition};
use crate::guest::sys::{self, Errno, FileDescriptor, Pollable};
use crate::protocol::nvxs::{GuestEvent, RecordError, RecordType, SessionError};
use crate::protocol::{
    self, APP_ERROR, APP_EXIT, APP_READY, APP_STDERR, APP_STDOUT, APP_STOPPED, OUTER_MAX_PAYLOAD,
    OUTER_RECEIVE_WINDOW, v2,
};

/// Bytes that the send queue holds before the agent stops reading an execution's output, until
/// the control console takes them.
const SEND_QUEUE_BYTES: usize = 64 * 1024;

/// The agent's control session: the guest's role of the shared NVXS session on the control
/// console, and the NVXC frames that its DATA records carry.
///
/// It serves requests of NVXC version 2, which keeps the codes of the kinds that version 1 had,
/// and refuses a frame of any other version as a malformed request. While an execution runs, the
/// session's records go through a send queue, which never waits for the console, so that the
/// agent keeps serving control requests while the host does not read; the record that it sends
/// next without streaming waits until the console has taken the queue and itself.
pub(crate) struct Session<'r> {
    /// The guest's NVXS connection on the owned control-console descriptor.
    connection: GuestConnection<Control<'r>>,
    /// Present only for nvx-init, never the old managed agent.
    capture: Option<Capture>,
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
    /// Bounds partial I/O until the restored activation's Ready has been written.
    deadline: Cell<Option<Instant>>,
    /// Records that the console has not taken yet, in order.
    queue: RefCell<Vec<u8>>,
    /// Whether writes add to the queue rather than wait for the console.
    queueing: Cell<bool>,
}

impl Control<'_> {
    fn remaining(&self) -> Result<Option<Duration>, Errno> {
        self.deadline
            .get()
            .map(|deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or(Errno::ETIMEDOUT)
            })
            .transpose()
    }

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

    /// Writes as much of the queue as the console takes without waiting.
    fn send_queued(&self) -> io::Result<()> {
        let mut queue = self.queue.borrow_mut();
        while !queue.is_empty() {
            match self.descriptor.write(&queue) {
                Ok(0) => return Err(Errno::EIO.into()),
                Ok(written) => drop(queue.drain(..written)),
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

impl Read for Control<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Read::read(&mut self.descriptor, bytes);
        }
        loop {
            if self.runtime.is_some() {
                self.service_leader()?;
            }
            let timeout = self.remaining()?;
            if let Some(runtime) = &self.runtime {
                let signals = sys::poll(
                    [Pollable::File(&self.descriptor), runtime.borrow().wake()],
                    timeout,
                )?[1];
                if signals.error {
                    return Err(Errno::EIO.into());
                }
                if signals.readable {
                    continue;
                }
            } else {
                sys::poll([Pollable::File(&self.descriptor)], timeout)?;
            }
            self.remaining()?;
            match self.descriptor.read(bytes) {
                Err(Errno::EAGAIN) => {}
                result => return result.map_err(io::Error::from),
            }
        }
    }
}

impl Write for Control<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Write::write(&mut self.descriptor, bytes);
        }
        if self.queueing.get() {
            self.queue.get_mut().extend_from_slice(bytes);
            self.send_queued()?;
            return Ok(bytes.len());
        }
        let queued = std::mem::take(self.queue.get_mut());
        self.write_all(&queued)?;
        loop {
            self.remaining()?;
            if self.runtime.is_some() {
                self.service_leader()?;
            }
            match self.descriptor.write(bytes) {
                Err(Errno::EAGAIN) => {
                    if let Some(runtime) = &self.runtime {
                        loop {
                            let events = sys::poll(
                                [
                                    Pollable::Writable(&self.descriptor),
                                    runtime.borrow().wake(),
                                ],
                                self.remaining()?,
                            )?;
                            self.remaining()?;
                            if events[1].error {
                                return Err(Errno::EIO.into());
                            }
                            self.service_leader()?;
                            if events[0].writable || !events[1].readable {
                                break;
                            }
                        }
                    } else {
                        sys::poll([Pollable::Writable(&self.descriptor)], self.remaining()?)?;
                        self.remaining()?;
                    }
                }
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

/// An NVXC request that the session accepted.
pub(crate) enum Request<'a> {
    /// A request other than an execution's standard input, with the code that version 2 gives
    /// its kind, as version 1 did.
    App(protocol::AppFrame<'a>),
    /// Version 2's standard input of the execution of this ID: a `STDIN` frame's bytes, or `None`
    /// for its `STDIN_END`.
    Stdin(u64, Option<&'a [u8]>),
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
                deadline: Cell::new(None),
                queue: RefCell::new(Vec::new()),
                queueing: Cell::new(false),
            }),
            capture: None,
            last_error: None,
        }
    }

    /// Enables only init's capture path on this existing session.
    pub(crate) fn with_capture(mut self, capture: Capture) -> Self {
        self.capture = Some(capture);
        self
    }

    /// Whether the serving caller is init rather than the old agent.
    pub(crate) fn has_capture(&self) -> bool {
        self.capture.is_some()
    }

    pub(crate) fn has_restored(&self) -> bool {
        self.capture
            .as_ref()
            .is_some_and(|capture| capture.restored.is_some())
    }

    /// Whether root-dependent requests must remain refused.
    pub(crate) fn rootless(&self) -> bool {
        self.capture.as_ref().is_some_and(Capture::rootless)
    }

    /// Whether execution cleanup must return the accepted capture request.
    pub(crate) fn capture_pending(&self) -> bool {
        self.capture.as_ref().is_some_and(Capture::pending)
    }

    /// The fatal capture status that the PID-1 caller must use.
    pub(crate) fn capture_status(&self) -> Option<u8> {
        self.capture.as_ref().and_then(Capture::shutdown_status)
    }

    /// The status of the failed restored activation's class, which the PID-1 caller uses once
    /// the session has failed, or `None` without a restored activation that failed.
    pub(crate) fn activation_status(&mut self) -> Option<u8> {
        self.capture
            .as_mut()?
            .restored
            .as_mut()?
            .failure_status(Instant::now())
    }

    /// Refuses rooted callers before validating the flag or reserving workload cleanup.
    pub(crate) fn queue_capture(
        &mut self,
        request: protocol::AppFrame<'_>,
        active: bool,
    ) -> Result<bool, Errno> {
        if !self.rootless() {
            self.send_error(
                request.request_id,
                Errno::ENOTSUP as i32,
                "unsupported-operation",
            )?;
            return Ok(false);
        }
        let kill = match protocol::decode_capture_payload(request.payload) {
            Ok(kill) => kill,
            Err(_) => {
                self.send_error(
                    request.request_id,
                    Errno::EINVAL as i32,
                    protocol::INVALID_REQUEST,
                )?;
                return Ok(false);
            }
        };
        if (active && !kill)
            || !self
                .capture
                .as_mut()
                .expect("init owns capture")
                .queue(request.request_id, kill)
        {
            self.send_error(request.request_id, Errno::EBUSY as i32, protocol::BUSY)?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Calls init's existing repeated sweep, never a host-side process sweep.
    pub(crate) fn sweep_capture(&self) -> Result<(), Errno> {
        self.capture.as_ref().ok_or(Errno::EPERM)?.sweep()
    }

    /// Acknowledges, captures synchronously, and handles the result on this connection.
    pub(crate) fn finish_capture(&mut self) -> Result<Option<super::dispatch::Outcome>, Errno> {
        self.finish_capture_with(|capture, session| {
            capture.perform(|request_id| session.send_app(protocol::APP_READY, request_id, 0, &[]))
        })
    }

    /// Keeps capture state across the operation and the existing session's RESET.
    fn finish_capture_with(
        &mut self,
        operation: impl FnOnce(&mut Capture, &mut Self) -> Result<Option<CaptureStep>, Errno>,
    ) -> Result<Option<super::dispatch::Outcome>, Errno> {
        let Some(mut capture) = self.capture.take() else {
            return Ok(None);
        };
        let result = (|| {
            match operation(&mut capture, self)? {
                None => {}
                Some(CaptureStep::Refused { request_id, error }) => {
                    init::report_capture(error);
                    let category = if error == Errno::EBUSY {
                        protocol::BUSY
                    } else {
                        "capture-failed"
                    };
                    self.send_error(request_id, error as i32, category)?;
                }
                Some(CaptureStep::Returned { request_id, result }) => match result {
                    Ok(Captured::Continued) => {
                        self.send_error(request_id, Errno::EAGAIN as i32, "capture-continued")?;
                    }
                    Ok(Captured::Restored(repaired)) => {
                        capture.restored(repaired);
                        let restored = capture.restored.as_mut().expect("the repair was retained");
                        self.descriptor().set_nonblocking()?;
                        self.connection
                            .get_ref()
                            .deadline
                            .set(restored.activation.deadline());
                        match self.next_event() {
                            Ok(Event::Reset) => {
                                restored.reset(Instant::now()).map_err(activation_failure)?;
                                self.acknowledge()?;
                            }
                            Ok(Event::Data(_)) => return Err(Errno::EPROTO),
                            Err(failure) => {
                                return Err(super::dispatch::report_failure_for_capture(failure));
                            }
                        }
                    }
                    Err(failure) => {
                        init::report_capture(&failure);
                        match failure.disposition {
                            Disposition::Resumed => {
                                self.send_error(request_id, Errno::EIO as i32, "capture-failed")?;
                            }
                            Disposition::MustEnd { status } => {
                                capture.must_end(status);
                                return Ok(Some(super::dispatch::Outcome::Stopped));
                            }
                        }
                    }
                },
            }
            Ok(None)
        })();
        self.capture = Some(capture);
        result
    }

    /// Runs init's restored space setup or returns its cached activation result.
    pub(crate) fn activate(
        &mut self,
        request: protocol::AppFrame<'_>,
    ) -> Result<Option<RestoreHandoff>, Errno> {
        self.activate_with(request, |restored, session, record, workload| {
            let runtime = session
                .connection
                .get_ref()
                .runtime
                .as_ref()
                .ok_or_else(|| crate::Error::unsupported("activation needs the init runtime"))?;
            restored.apply(record, workload, |publication, gateway| {
                runtime.borrow_mut().restore_announcer(publication, gateway)
            })
        })
    }

    /// Keeps one application and its result while replacement sessions join before Ready.
    fn activate_with<R>(
        &mut self,
        request: protocol::AppFrame<'_>,
        apply: impl FnOnce(
            &mut Restored,
            &mut Self,
            &crate::guest_restore::GuestRestoreRecord,
            protocol::workload::WorkloadIdentity,
        ) -> crate::Result<R>,
    ) -> Result<Option<R>, Errno> {
        let mut capture = self.capture.take().ok_or(Errno::ENOTSUP)?;
        let result = (|| {
            let restored = capture.restored.as_mut().ok_or(Errno::EPROTO)?;
            let (action, workload) = restored
                .accept(request.payload, Instant::now())
                .map_err(activation_failure)?;
            let (retained, ready, replies) = match action {
                GuestRestoreAction::Apply(record) => {
                    let retained = match apply(restored, self, &record, workload) {
                        Ok(retained) => retained,
                        Err(error) => {
                            init::report_activation(&error);
                            return Err(activation_failure(
                                restored
                                    .activation
                                    .complete(Instant::now(), Err(error.into()))
                                    .expect_err("a failed application cannot become ready"),
                            ));
                        }
                    };
                    let mut replies = vec![request.request_id];
                    while sys::readable_or_hung_up(&[self.descriptor().borrowed()], 0)?[0] {
                        match self
                            .next_event()
                            .map_err(super::dispatch::report_failure_for_capture)?
                        {
                            Event::Reset => {
                                restored.reset(Instant::now()).map_err(activation_failure)?;
                                self.acknowledge()?;
                                replies.clear();
                            }
                            Event::Data(bytes) => {
                                let retry = match self
                                    .accept_request(&bytes)
                                    .map_err(DataFailure::into_error)?
                                {
                                    Request::App(retry) if retry.kind == protocol::APP_ACTIVATE => {
                                        retry
                                    }
                                    _ => {
                                        return Err(activation_failure(
                                            restored
                                                .activation
                                                .input(Instant::now(), GuestRestoreInput::Request)
                                                .expect_err("an activation takes no request yet"),
                                        ));
                                    }
                                };
                                if !matches!(
                                    restored
                                        .accept(retry.payload, Instant::now())
                                        .map_err(activation_failure)?
                                        .0,
                                    GuestRestoreAction::Join
                                ) {
                                    return Err(Errno::EPROTO);
                                }
                                replies.push(retry.request_id);
                            }
                        }
                    }
                    let ready = restored
                        .activation
                        .complete(Instant::now(), Ok(()))
                        .map_err(activation_failure)?;
                    (Some(retained), ready, replies)
                }
                GuestRestoreAction::Ready(ready) => (None, ready, vec![request.request_id]),
                _ => return Err(Errno::EPROTO),
            };
            for id in replies {
                self.send_app(protocol::APP_READY, id, 0, &ready.encode())?;
            }
            self.connection.get_ref().deadline.set(None);
            Ok(retained)
        })();
        self.capture = Some(capture);
        result
    }

    /// Takes the first valid mapped EXEC as the start transition, never starting twice.
    pub(crate) fn start_activation(&mut self) -> Result<(), Errno> {
        if let Some(restored) = self
            .capture
            .as_mut()
            .and_then(|capture| capture.restored.as_mut())
            && restored.activation.state() != GuestRestoreState::Started
        {
            restored.activation.start().map_err(activation_failure)?;
        }
        Ok(())
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
            Ok(GuestEvent::Reset(_)) => {
                if let Some(capture) = &mut self.capture {
                    capture.reset();
                    if let Some(restored) = &mut capture.restored {
                        restored
                            .reset(Instant::now())
                            .map_err(|error| ReceiveFailure::Read(activation_failure(error)))?;
                    }
                }
                Ok(Event::Reset)
            }
            Ok(GuestEvent::Data(payload)) => Ok(Event::Data(payload)),
            Err(error) => Err(self.receive_failure(error)),
        }
    }

    /// Acknowledges the broker's reset with NVX's receive window.
    pub(crate) fn acknowledge(&mut self) -> Result<(), Errno> {
        let acknowledged = self.connection.acknowledge(OUTER_RECEIVE_WINDOW);
        acknowledged.map_err(|error| self.error(error))
    }

    /// Decodes the NVXC request that a DATA payload carries, of version 2, then returns the
    /// payload's bytes to the receive window before the agent serves the request, as the C agent
    /// did. A frame of another version, such as version 1, is malformed.
    pub(crate) fn accept_request<'a>(
        &mut self,
        payload: &'a [u8],
    ) -> Result<Request<'a>, DataFailure> {
        // A DATA payload has at most `OUTER_MAX_PAYLOAD` bytes, so its length fits either type.
        let status = payload.len() as i32;
        let request = v2::Frame::decode(payload)
            .ok()
            .map(|(request_id, frame)| match frame {
                v2::Frame::Stdin(bytes) => Request::Stdin(request_id, Some(bytes)),
                v2::Frame::StdinEnd => Request::Stdin(request_id, None),
                // Version 2 keeps version 1's header, and the codes of the kinds that both
                // have, which its decoder checked.
                _ => Request::App(protocol::AppFrame {
                    kind: payload[5],
                    request_id,
                    status: 0,
                    payload: &payload[protocol::APP_HEADER_LEN..],
                }),
            })
            .ok_or_else(|| data_failure(DataFailureKind::Application, status))?;
        let credited = self.connection.credit(payload.len() as u32);
        credited.map_err(|error| DataFailure {
            kind: DataFailureKind::Credit,
            status,
            error: self.error(error),
        })?;
        Ok(request)
    }

    /// Encodes and sends one application frame of version 2, in one DATA record.
    ///
    /// `READY` starts with the protocol version, an `ERROR` of the `busy` category is `BUSY`, and
    /// an `EXIT` names its outcome as version 1 did.
    pub(crate) fn send_app(
        &mut self,
        kind: u8,
        request_id: u64,
        status: i32,
        payload: &[u8],
    ) -> Result<(), Errno> {
        let frame = match kind {
            APP_READY => v2::Frame::Ready {
                version: v2::VERSION,
                activation: payload,
            },
            APP_STDOUT => v2::Frame::Stdout(payload),
            APP_STDERR => v2::Frame::Stderr(payload),
            APP_EXIT => v2::Frame::Exit {
                status,
                outcome: outcome(payload),
            },
            APP_STOPPED => v2::Frame::Stopped,
            APP_ERROR if payload == protocol::BUSY.as_bytes() => v2::Frame::Busy,
            APP_ERROR => v2::Frame::Error {
                status,
                category: payload,
            },
            _ => return Err(Errno::EINVAL),
        };
        let frame = frame.encode(request_id).map_err(|_| Errno::EMSGSIZE)?;
        let sent = self.connection.send(&frame);
        sent.map_err(|error| self.error(error))
    }

    /// Acknowledges `count` bytes written into the standard input of the execution of
    /// `request_id`, in version 2.
    pub(crate) fn send_ack(&mut self, request_id: u64, count: u32) -> Result<(), Errno> {
        let frame = v2::Frame::StdinAck(count)
            .encode(request_id)
            .map_err(|_| Errno::EINVAL)?;
        let sent = self.connection.send(&frame);
        sent.map_err(|error| self.error(error))
    }

    /// Streams, while an execution of version 2 runs: every record goes through the send queue,
    /// which never waits for the console, and which the next record sent without streaming
    /// follows.
    pub(crate) fn stream(&self, on: bool) {
        self.connection.get_ref().queueing.set(on);
    }

    /// Whether the send queue holds [`SEND_QUEUE_BYTES`], so that the agent reads no more of an
    /// execution's output.
    pub(crate) fn queue_full(&self) -> bool {
        self.connection.get_ref().queue.borrow().len() >= SEND_QUEUE_BYTES
    }

    /// Whether the send queue holds bytes, so that the agent polls the console writable.
    pub(crate) fn queued(&self) -> bool {
        !self.connection.get_ref().queue.borrow().is_empty()
    }

    /// Writes as much of the send queue as the console takes without waiting.
    pub(crate) fn send_queued(&mut self) -> Result<(), Errno> {
        let sent = self.connection.get_ref().send_queued();
        sent.map_err(|error| self.error(GuestConnectionError::Io(error)))
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

fn activation_failure(error: impl std::fmt::Display) -> Errno {
    init::report_activation(error);
    Errno::EPROTO
}

/// Returns version 2's outcome of the execution whose version-1 `EXIT` category is `category`.
fn outcome(category: &[u8]) -> v2::Outcome {
    match category {
        b"exit" => v2::Outcome::Exit,
        b"signal" => v2::Outcome::Signal,
        b"timeout" => v2::Outcome::Timeout,
        b"cancelled" => v2::Outcome::Cancelled,
        _ => v2::Outcome::Failed,
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

#[cfg(test)]
mod tests;
