//! The control session of `nvx-rs sandbox`, a port of the former
//! `scripts/nvx_tools/control_session.py` on the requests, framing, sequencing, attachment, and
//! transports of [`crate::host::session`], which speaks version 2 of NVXC.
//!
//! It keeps the observable behavior of the Python session:
//!
//! - [`Session::connect_unless_exited`] retries the endpoint every 25 ms until its timeout, then attaches with
//!   the capability within a fresh timeout. On Windows, it gives up at once on the errors that
//!   Python does not retry. On Linux, unsupported peer verification fails at once, retaining
//!   its diagnostic instead of reporting that the endpoint did not appear.
//! - Each operation bounds its request and its response by one timeout. A transfer that times
//!   out fails with `managed control request timed out` while it sends and `managed control
//!   response timed out` while it receives.
//! - An endpoint that closes, as OpenVMM closes it when it shuts down, ends the operation with
//!   [`Exit::EndpointClosed`], while a session that OpenVMM resets fails with `managed control
//!   session was reset`.
//! - `exec` refuses invalid requests before it sends anything, with Python's messages, and
//!   accepts the exit categories of `MANAGED_EXIT_CATEGORIES`; a refusal keeps its status and its
//!   category.
//! - A session whose transfer failed is closed, and refuses further requests.
//!
//! Unlike the Python session, it also hands the guest agent a host mapping table, as `start`
//! does for the shares of a sandbox: in order, and in as many requests as it takes.
//! [`Session::reattach`] verifies a saved VM and observes its transport before authentication;
//! [`Session::connect_launched_cancellable`], with which a cold run waits for the endpoint of the
//! OpenVMM that it launched, also stops waiting once that OpenVMM exits, where Python waited until
//! its timeout. `exec` streams, where Python buffered the workload's output: the workload reads
//! the command's standard input, until its end, and its output reaches the command's standard
//! output and error as it comes, without a total cap; and Ctrl-C cancels it, which would
//! otherwise outlive the session.
//!
//! The transports also wait in slices of at most 250 ms, as Python's do, so that Ctrl-C ends a
//! wait within that time, except while a request is being sent, which its timeout bounds, and
//! while a workload streams, whose loop checks for Ctrl-C itself.

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::deadline::{Deadline, RETRY_INTERVAL, sleep};
use super::{Exit, Result, check_interrupt, output};
use crate::host::platform::{self, Transport};
use crate::host::process::Process;
use crate::host::runtime::Runtime;
use crate::host::sandbox::options::validate_exec_environment;
use crate::host::session::{ControlSession, ExecEvent, SessionError};
use crate::host::transport;
use crate::host::vm::{self, SavedVm};
use crate::protocol::v2::{self, Outcome};
use crate::protocol::{
    CAPABILITY_LEN, HostMap, Workload, WorkloadEnvironment, encode_exec_payload,
};
use crate::{ErrorCode, ErrorKind};

/// The exit categories that `exec` accepts, `MANAGED_EXIT_CATEGORIES`, but the output limit,
/// which version 2 of NVXC does not have. A cancellation, whose end this session never waits
/// for, is not among them.
pub(crate) const EXIT_CATEGORIES: [&str; 4] = ["exit", "timeout", "signal", "failed"];
/// The most arguments of a workload, `APP_MAX_ARGUMENTS`.
const MAX_ARGUMENTS: usize = 64;
/// The longest argument and working directory, in UTF-8 bytes.
const MAX_ARGUMENT_BYTES: usize = 4096;
/// The longest that one wait of a transport blocks before it checks for Ctrl-C.
const POLL_SLICE: Duration = Duration::from_millis(250);

/// The direction of the last transfer, for the message of a timeout.
const SENDING: u8 = 1;
/// See [`SENDING`].
const RECEIVING: u8 = 2;

/// Scoped foreground cancellation, including I/O already pending on its verified connection.
#[derive(Clone, Default)]
pub(super) struct AttachmentCancellation {
    /// Whether forwarding ended before the attachment did.
    cancelled: Arc<AtomicBool>,
    /// A duplicate of this attachment's connection, never another endpoint or process.
    #[cfg(any(target_os = "linux", windows))]
    transport: Arc<std::sync::Mutex<Option<platform::TransportHandle>>>,
}

impl AttachmentCancellation {
    /// Refuses new work after the foreground owner cancelled attachment.
    fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(io::Error::other("foreground attachment cancelled"))
        } else {
            Ok(())
        }
    }

    /// Retains only the already verified connection before the handshake may use it.
    fn register(&self, transport: &dyn Transport) -> io::Result<()> {
        #[cfg(any(target_os = "linux", windows))]
        {
            let handle = transport.try_clone_handle()?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "attachment I/O cannot be cancelled",
                )
            })?;
            *self.transport.lock().expect("attachment lock poisoned") = Some(handle);
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = transport;
        self.check()
    }

    /// Releases the cancellation duplicate once attachment no longer has pending I/O.
    fn attached(&self) {
        #[cfg(any(target_os = "linux", windows))]
        self.transport
            .lock()
            .expect("attachment lock poisoned")
            .take();
    }

    /// Ends pending I/O; repeated calls also cover cancellation racing with an I/O submission.
    #[allow(unsafe_code)]
    pub(super) fn cancel(&self) -> io::Result<()> {
        self.cancelled.store(true, Ordering::SeqCst);
        #[cfg(any(target_os = "linux", windows))]
        if let Some(handle) = &*self.transport.lock().expect("attachment lock poisoned") {
            #[cfg(target_os = "linux")]
            {
                use nix::sys::socket::{Shutdown, shutdown};
                use std::os::fd::AsRawFd;

                match shutdown(handle.as_raw_fd(), Shutdown::Both) {
                    Ok(()) | Err(nix::errno::Errno::ENOTCONN) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
                use windows_sys::Win32::System::IO::CancelIoEx;

                // SAFETY: the retained handle belongs only to this attachment's connection.
                if unsafe { CancelIoEx(handle.as_raw_handle().cast(), std::ptr::null()) } == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(ERROR_NOT_FOUND as i32) {
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }
}

/// What the transport observed of the failure of a transfer.
#[derive(Default)]
struct Probe {
    /// [`SENDING`] or [`RECEIVING`]: the direction of the transfer in progress.
    direction: AtomicU8,
    /// Whether the endpoint closed the stream.
    closed: AtomicBool,
    /// Whether Ctrl-C ended a wait.
    interrupted: AtomicBool,
    /// Whether a workload streams, whose loop, rather than a wait of the transport, ends at
    /// Ctrl-C, so that it can cancel the workload first.
    streaming: AtomicBool,
    /// The foreground owner's cancellation, absent for ordinary sessions.
    cancellation: Option<AttachmentCancellation>,
}

/// A transport that records what [`Probe`] reports, and waits in slices that Ctrl-C can end.
struct ProbedTransport {
    /// The platform's transport.
    inner: Box<dyn Transport>,
    /// What this transport observed.
    probe: Arc<Probe>,
}

/// Returns whether `error` tells that the endpoint closed the stream, as Python's
/// `_PEER_CLOSED` and `_PIPE_CLOSED_ERRORS` do.
pub(super) fn closes(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::WriteZero
            | io::ErrorKind::UnexpectedEof
    ) || (cfg!(windows) && matches!(error.raw_os_error(), Some(109 | 232 | 233)))
}

impl ProbedTransport {
    /// Returns the error with which a wait that Ctrl-C ended fails.
    fn interrupted(&self) -> io::Error {
        self.probe.interrupted.store(true, Ordering::SeqCst);
        io::Error::other("interrupted")
    }
}

impl Transport for ProbedTransport {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        self.probe.direction.store(RECEIVING, Ordering::SeqCst);
        let deadline = timeout.map(|timeout| std::time::Instant::now() + timeout);
        loop {
            if platform::interrupted() && !self.probe.streaming.load(Ordering::SeqCst) {
                return Err(self.interrupted());
            }
            if let Some(cancellation) = &self.probe.cancellation {
                cancellation.check()?;
            }
            let left = deadline
                .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
            let slice = left.map_or(POLL_SLICE, |left| left.min(POLL_SLICE));
            match self.inner.read(buffer, Some(slice)) {
                Ok(count) => {
                    if count == 0 {
                        self.probe.closed.store(true, Ordering::SeqCst);
                    }
                    return Ok(count);
                }
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                    if left.is_some_and(|left| left <= slice) {
                        return Err(error);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    if closes(&error) {
                        self.probe.closed.store(true, Ordering::SeqCst);
                    }
                    return Err(error);
                }
            }
        }
    }

    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        self.probe.direction.store(SENDING, Ordering::SeqCst);
        if let Some(cancellation) = &self.probe.cancellation {
            cancellation.check()?;
        }
        // The platform's transports bound the whole write by `timeout`, however slowly the
        // endpoint drains it.
        match self.inner.write_all(data, timeout) {
            Ok(()) => {
                self.probe.direction.store(RECEIVING, Ordering::SeqCst);
                Ok(())
            }
            Err(error) => {
                if closes(&error) {
                    self.probe.closed.store(true, Ordering::SeqCst);
                }
                Err(error)
            }
        }
    }
}

/// A workload that [`Session::exec`] runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecRequest {
    /// The program and its arguments.
    pub(crate) arguments: Vec<String>,
    /// `--exec-timeout-ms`: the guest's deadline for the workload, in milliseconds, or 0 for none.
    pub(crate) timeout_ms: u32,
    /// `--cwd`.
    pub(crate) cwd: Option<String>,
    /// The environment, if one was given.
    pub(crate) environment: Option<Vec<String>>,
    /// `--inherit-default-environment`.
    pub(crate) inherit_default_environment: bool,
}

/// How a workload ended: `ManagedExecResult`, whose output already went to the standard streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecResult {
    /// The workload's status.
    pub(crate) returncode: i32,
    /// One of [`EXIT_CATEGORIES`].
    pub(crate) category: &'static str,
    /// The last byte that the workload wrote to standard error, if any, which tells whether it
    /// left a line open there.
    pub(crate) stderr_end: Option<u8>,
}

/// An authenticated control session: `ControlSession` of the former `control_session.py`.
pub(crate) struct Session {
    /// The session, until a transfer fails.
    inner: Option<ControlSession>,
    /// What its transport observed.
    probe: Arc<Probe>,
}

impl Session {
    /// Observes the same verified transport through attachment and later control operations.
    pub(crate) fn reattach(saved: &SavedVm, runtime: &Runtime, timeout: f64) -> Result<Self> {
        let probe = Arc::new(Probe::default());
        let connection = vm::attach_with_transport(
            saved,
            runtime,
            Deadline::after(timeout).instant(),
            |endpoint, reference, deadline| loop {
                if platform::interrupted() {
                    probe.interrupted.store(true, Ordering::SeqCst);
                    return Err(crate::Error::io("control connection interrupted"));
                }
                let until = (std::time::Instant::now() + POLL_SLICE).min(deadline);
                match transport::connect_verified(endpoint, reference, until) {
                    Ok(inner) => {
                        return Ok(Box::new(ProbedTransport {
                            inner,
                            probe: Arc::clone(&probe),
                        }));
                    }
                    Err(error)
                        if error.kind() == ErrorKind::TimedOut
                            && std::time::Instant::now() < deadline => {}
                    Err(error) => return Err(error),
                }
            },
        )
        .map_err(|error| {
            if probe.interrupted.load(Ordering::SeqCst) {
                Exit::Interrupted
            } else if probe.closed.load(Ordering::SeqCst)
                || error.kind() == ErrorKind::Exited
                || std::error::Error::source(&error)
                    .and_then(|source| source.downcast_ref::<io::Error>())
                    .is_some_and(closes)
            {
                Exit::EndpointClosed
            } else {
                super::sandbox::error_with_cause(error)
            }
        })?;
        Ok(Self {
            inner: Some(connection.into()),
            probe,
        })
    }

    /// Connects to `endpoint`, which `process`, the OpenVMM process that a cold run launched,
    /// serves, as [`Self::connect_unless_exited`] does, but stops waiting for the endpoint once
    /// OpenVMM has exited, as `nvx::host::session::connect` does: an OpenVMM that exits before it
    /// serves its endpoint, as one that refuses its configuration does, then ends the wait at
    /// once, rather than at its timeout. A check that fails keeps the wait going until the
    /// timeout, as there. The foreground's output owner can cancel both wait phases through
    /// `cancellation`.
    pub(super) fn connect_launched_cancellable(
        endpoint: &str,
        process: &Process,
        capability: &[u8; CAPABILITY_LEN],
        timeout: f64,
        cancellation: AttachmentCancellation,
    ) -> Result<Self> {
        let pid = process.reference().pid;
        Self::connect_unless_exited(
            endpoint,
            pid,
            capability,
            timeout,
            || process.has_exited().is_ok_and(|exited| exited),
            Some(cancellation),
        )
    }

    /// Connects and attaches, but fails as soon as `exited`, which it calls
    /// after each attempt that finds no endpoint, tells that OpenVMM process `pid` has exited:
    /// with [`Exit::EndpointClosed`], as the session reports the `ProcessExited` of
    /// `nvx::host::session`.
    fn connect_unless_exited(
        endpoint: &str,
        pid: u32,
        capability: &[u8; CAPABILITY_LEN],
        timeout: f64,
        mut exited: impl FnMut() -> bool,
        cancellation: Option<AttachmentCancellation>,
    ) -> Result<Self> {
        let deadline = Deadline::after(timeout);
        let transport = loop {
            check_interrupt()?;
            if let Some(cancellation) = &cancellation {
                cancellation.check().map_err(Exit::from_os_error)?;
            }
            let slice = deadline
                .remaining()
                .map_or(POLL_SLICE, |left| left.min(POLL_SLICE));
            match platform::connect_endpoint(endpoint, pid, slice) {
                Ok(transport) => break transport,
                Err(error) => {
                    if (cfg!(target_os = "linux") && error.kind() == io::ErrorKind::Unsupported)
                        || (cfg!(windows)
                            && error
                                .raw_os_error()
                                .is_some_and(|code| raised_at_once(code as u32)))
                    {
                        return Err(Exit::from_os_error(error));
                    }
                    // An OpenVMM that has exited serves no endpoint, whatever time is left.
                    if exited() {
                        return Err(Exit::EndpointClosed);
                    }
                    if deadline.passed() {
                        return Err(Exit::error(
                            ErrorCode::TimedOut,
                            format!(
                                "managed control endpoint did not become available: {endpoint}"
                            ),
                        ));
                    }
                    sleep(RETRY_INTERVAL)?;
                }
            }
        };
        Self::attach(transport, capability, timeout, cancellation)
    }

    /// Attaches over a connected `transport` with `capability` within `timeout` seconds, as the
    /// second half of [`Self::connect_unless_exited`].
    fn attach(
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        timeout: f64,
        cancellation: Option<AttachmentCancellation>,
    ) -> Result<Self> {
        if let Some(cancellation) = &cancellation {
            cancellation
                .register(transport.as_ref())
                .map_err(Exit::from_os_error)?;
        }
        let probe = Arc::new(Probe {
            cancellation,
            ..Probe::default()
        });
        let transport = Box::new(ProbedTransport {
            inner: transport,
            probe: Arc::clone(&probe),
        });
        let deadline = Deadline::after(timeout);
        probe.direction.store(SENDING, Ordering::SeqCst);
        let mut session = Self { inner: None, probe };
        let attached = ControlSession::attach(transport, capability, deadline.instant())
            .map_err(|error| session.failure(error))?;
        session.inner = Some(attached);
        if let Some(cancellation) = &session.probe.cancellation {
            cancellation.attached();
        }
        Ok(session)
    }

    /// Returns the open session, or fails as a closed `ControlSession` does.
    fn session(&mut self) -> Result<&mut ControlSession> {
        self.inner
            .as_mut()
            .ok_or_else(|| Exit::error(ErrorCode::Protocol, "managed control session is closed"))
    }

    /// Runs `operation` on the open session, and closes the session if a transfer fails.
    fn call<T>(
        &mut self,
        operation: impl FnOnce(&mut ControlSession) -> std::result::Result<T, SessionError>,
    ) -> Result<T> {
        self.probe.direction.store(SENDING, Ordering::SeqCst);
        let result = operation(self.session()?);
        result.map_err(|error| self.fail(error))
    }

    /// Closes the session after a failed transfer, and returns the failure. A refusal of the guest
    /// agent, or an answer that the protocol does not allow, fails no transfer: the session stays,
    /// and its connection ends at a record that the protocol does not allow.
    fn fail(&mut self, error: SessionError) -> Exit {
        if !matches!(error, SessionError::Protocol(_)) {
            self.inner = None;
        }
        self.failure(error)
    }

    /// Returns what Python raises for `error`.
    fn failure(&self, error: SessionError) -> Exit {
        if self.probe.interrupted.load(Ordering::SeqCst) {
            return Exit::Interrupted;
        }
        let closed = self.probe.closed.load(Ordering::SeqCst);
        match error {
            SessionError::TimedOut => Exit::error(
                ErrorCode::TimedOut,
                if self.probe.direction.load(Ordering::SeqCst) == SENDING {
                    "managed control request timed out"
                } else {
                    "managed control response timed out"
                },
            ),
            SessionError::Closed => Exit::EndpointClosed,
            SessionError::Io(error) | SessionError::Refused(error) => {
                if closed || closes(&error) {
                    Exit::EndpointClosed
                } else {
                    Exit::from_os_error(error)
                }
            }
            SessionError::Reset => {
                Exit::error(ErrorCode::Protocol, "managed control session was reset")
            }
            SessionError::AuthenticationFailed => Exit::error(
                ErrorCode::Protocol,
                "control capability authentication failed",
            ),
            SessionError::ProcessExited => Exit::EndpointClosed,
            SessionError::Protocol(message) => {
                Exit::error(ErrorCode::Protocol, protocol_message(message))
            }
        }
    }

    /// Waits until the guest agent acknowledges readiness, as `ControlSession.ping` does.
    pub(crate) fn ping(&mut self, timeout: f64) -> Result {
        let deadline = Deadline::after(timeout).instant();
        self.call(|session| session.ping(deadline))
    }

    /// Hands the guest agent the host mapping table `maps`, in order and in as many `MAPS`
    /// requests as it takes, and waits until the agent has mounted every entry, all within
    /// `timeout` seconds. The agent refuses workloads until then, and refuses the rest of the
    /// table after an entry that it could not mount.
    pub(crate) fn map_host_paths(&mut self, maps: &[HostMap], timeout: f64) -> Result {
        let deadline = Deadline::after(timeout).instant();
        self.call(|session| session.map_host_paths(maps, deadline))
    }

    /// Checks the workload of `request` and returns the payload of its exec request, as
    /// [`exec`](Self::exec) sends it, so that a caller can refuse a request before it starts a
    /// VM.
    pub(crate) fn exec_payload(request: &ExecRequest) -> Result<Vec<u8>> {
        let refused = |message: &str| Exit::error(ErrorCode::Unsupported, message);
        let arguments = &request.arguments;
        if !(1..=MAX_ARGUMENTS).contains(&arguments.len()) {
            return Err(refused("managed exec requires 1 through 64 arguments"));
        }
        for argument in arguments {
            if argument.is_empty() || argument.len() > MAX_ARGUMENT_BYTES || argument.contains('\0')
            {
                return Err(refused(
                    "managed exec argument is empty or exceeds 4096 bytes",
                ));
            }
        }
        if !arguments[0].starts_with('/') {
            return Err(refused("managed exec entrypoint must be absolute"));
        }
        if let Some(cwd) = &request.cwd
            && (cwd.is_empty()
                || cwd.len() > MAX_ARGUMENT_BYTES
                || cwd.contains('\0')
                || !cwd.starts_with('/'))
        {
            return Err(refused(
                "managed exec working directory must be an absolute path of at most 4096 bytes",
            ));
        }
        if let Some(entries) = &request.environment {
            validate_exec_environment(entries)?;
        }
        let environment = match &request.environment {
            None => WorkloadEnvironment::Default,
            Some(entries) if request.inherit_default_environment => {
                WorkloadEnvironment::Layered(entries)
            }
            Some(entries) => WorkloadEnvironment::Replaced(entries),
        };
        let workload = Workload {
            argv: arguments.clone(),
            timeout_ms: request.timeout_ms,
            cwd: request.cwd.as_deref(),
            environment,
        };
        // The checks above leave only the size of the request for the encoder to refuse.
        encode_exec_payload(&workload)
            .map_err(|_| refused("managed exec request exceeds the protocol limit"))
    }

    /// Runs a workload and returns how it ended, as `ControlSession.exec` does: the request and
    /// every response must arrive within `response_timeout` seconds of its start. The workload
    /// reads this process's standard input until its end, and its standard output and error
    /// reach this process's as they come. Ctrl-C cancels it. An error that follows its output
    /// starts a line of its own, as callers find errors.
    pub(crate) fn exec(
        &mut self,
        request: &ExecRequest,
        response_timeout: f64,
    ) -> Result<ExecResult> {
        let mut end = None;
        let write = |stderr, data: &[u8]| {
            if stderr {
                end = data.last().copied();
                output::write_stderr(data)
            } else {
                output::write_stdout(data)
            }
        };
        let result = self.exec_with(
            request,
            response_timeout,
            read_stdin,
            platform::interrupted,
            write,
        );
        if result.is_err() {
            output::end_stderr_line(end.as_slice());
        }
        result
    }

    /// Runs a workload as [`exec`](Self::exec) does, with the chunks that `input` returns the
    /// receiver of, until it disconnects, as its standard input, or a read of it fails, `write`
    /// for its standard output, or its standard error if its first argument holds, and
    /// `interrupted` for Ctrl-C.
    fn exec_with(
        &mut self,
        request: &ExecRequest,
        response_timeout: f64,
        input: impl FnOnce() -> mpsc::Receiver<io::Result<Vec<u8>>>,
        interrupted: impl Fn() -> bool,
        mut write: impl FnMut(bool, &[u8]) -> io::Result<()>,
    ) -> Result<ExecResult> {
        if !(response_timeout > 0.0 && response_timeout < f64::INFINITY) {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                "managed exec response timeout must be positive and finite",
            ));
        }
        let payload = Self::exec_payload(request)?;
        let request_id = request_id()?;
        let deadline = Deadline::after(response_timeout);
        self.call(|session| session.start_exec(request_id, &payload, deadline.instant()))?;
        self.probe.streaming.store(true, Ordering::SeqCst);
        let result = self.stream(request_id, deadline, &input(), &interrupted, &mut write);
        self.probe.streaming.store(false, Ordering::SeqCst);
        result
    }

    /// Streams the execution of `request_id` until it ends or `deadline` passes, as
    /// [`exec_with`](Self::exec_with) does, with no more than the window of its standard input
    /// unacknowledged, which [`ControlSession::send_stdin`] keeps.
    fn stream(
        &mut self,
        request_id: u64,
        deadline: Deadline,
        input: &mpsc::Receiver<io::Result<Vec<u8>>>,
        interrupted: &impl Fn() -> bool,
        write: &mut impl FnMut(bool, &[u8]) -> io::Result<()>,
    ) -> Result<ExecResult> {
        let (mut pending, mut open, mut stderr_end) = (Vec::new(), true, None);
        loop {
            if interrupted() {
                self.cancel(request_id);
                return Err(Exit::Interrupted);
            }
            if open && pending.is_empty() {
                match input.try_recv() {
                    Ok(Ok(chunk)) => pending = chunk,
                    // An end of the input would hand the workload what it read as all of it.
                    Ok(Err(error)) => {
                        self.cancel(request_id);
                        return Err(Exit::error(
                            ErrorCode::Io,
                            format!("cannot read standard input: {error}"),
                        ));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        open = false;
                        self.call(|session| session.close_stdin(request_id, deadline.instant()))?;
                    }
                }
            }
            if !pending.is_empty() {
                let sent = self
                    .call(|session| session.send_stdin(request_id, &pending, deadline.instant()))?;
                pending.drain(..sent);
            }
            self.probe.direction.store(RECEIVING, Ordering::SeqCst);
            let slice = deadline.instant().min(Instant::now() + RETRY_INTERVAL);
            let written = match self.session()?.next_exec_event(request_id, Some(slice)) {
                Err(SessionError::TimedOut) if !deadline.passed() => Ok(()),
                Err(error) => return Err(self.fail(error)),
                Ok(ExecEvent::Stdout(data)) => write(false, &data),
                Ok(ExecEvent::Stderr(data)) => {
                    stderr_end = data.last().copied();
                    write(true, &data)
                }
                Ok(ExecEvent::Acknowledged(_)) => Ok(()),
                Ok(ExecEvent::Exit { category, status }) => {
                    return Ok(ExecResult {
                        returncode: status,
                        category: exit_category(category)?,
                        stderr_end,
                    });
                }
                Ok(ExecEvent::Rejected { status, category }) => {
                    return Err(Exit::error(
                        ErrorCode::ExecRefused,
                        format!(
                            "managed guest rejected exec (status={status}, category={category})"
                        ),
                    ));
                }
            };
            // Ctrl-C ends a write, after which the next turn cancels the workload.
            if let Err(error) = written
                && !interrupted()
            {
                return Err(Exit::from_os_error(error));
            }
        }
    }

    /// Asks the guest agent, within one wait slice, to end the execution of `request_id`, which
    /// would otherwise outlive the session, which ends with the command.
    fn cancel(&mut self, request_id: u64) {
        if let Some(session) = &mut self.inner {
            let _ = session.cancel_exec(request_id, Instant::now() + POLL_SLICE);
        }
    }

    /// Asks the guest agent to shut the VM down, as `ControlSession.stop` does.
    pub(crate) fn stop(&mut self, timeout: f64) -> Result {
        let deadline = Deadline::after(timeout).instant();
        self.call(|session| session.stop(deadline))
    }
}

/// Returns the category of [`EXIT_CATEGORIES`] of a workload that ended with `outcome`, or fails
/// for a cancellation, whose end no execution of this session reports.
fn exit_category(outcome: Outcome) -> Result<&'static str> {
    Ok(match outcome {
        Outcome::Exit => "exit",
        Outcome::Timeout => "timeout",
        Outcome::Signal => "signal",
        Outcome::Failed => "failed",
        Outcome::Cancelled => {
            return Err(Exit::error(
                ErrorCode::Protocol,
                "managed guest returned an unsupported exit category",
            ));
        }
    })
}

/// Returns the receiver of the chunks of this process's standard input, which a thread of its own
/// reads until the input ends, or a read fails, whose error ends them, or no one receives them
/// any longer.
fn read_stdin() -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let mut chunk = vec![0; v2::MAX_PAYLOAD];
            match input.read(&mut chunk) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(0) => return,
                Ok(count) => {
                    chunk.truncate(count);
                    if sender.send(Ok(chunk)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            }
        }
    });
    receiver
}

/// Returns Python's message for a violation of the protocol that `nvx::host::session` reports.
///
/// The two sessions report the same violations of records that have one fault, and their
/// messages differ only where Python folds a case into a broader one, and where Python named the
/// guest agent `managed guest`.
fn protocol_message(message: String) -> String {
    match message.as_str() {
        "control attach response carried a payload" => {
            "control attach response carried an invalid payload".to_owned()
        }
        "control endpoint returned an oversized outer record" => {
            "control endpoint returned an invalid outer record".to_owned()
        }
        // Python checks the length of the frame with the record that carries it.
        "control endpoint returned a truncated application frame" => {
            "control endpoint returned an invalid data record".to_owned()
        }
        _ => match message.strip_prefix("the guest agent ") {
            Some(rest) => format!("managed guest {rest}"),
            None => message,
        },
    }
}

/// Returns whether Python's `_NamedPipeStream.connect` raises the Windows error `code` at once
/// instead of retrying: those that CPython maps to an `errno` other than `ENOENT`, `EACCES`,
/// `EBUSY`, `EAGAIN`, and `EINVAL`, its default.
fn raised_at_once(code: u32) -> bool {
    matches!(
        code,
        // E2BIG, ENOEXEC, EBADF, ECHILD, and ENOMEM.
        10 | 11 | 188
            ..=202 | 6 | 114 | 130 | 128 | 129 | 7 | 8 | 9 | 1816
        // EEXIST, EXDEV, ENOTDIR, EMFILE, ENOSPC, EPIPE, ENOTEMPTY, and EILSEQ.
        | 80 | 183 | 17 | 267 | 4 | 112 | 109 | 232 | 145 | 1113
    )
}

/// Returns a request ID: `secrets.randbits(64) or 1`.
fn request_id() -> Result<u64> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|error| {
        Exit::error(
            ErrorCode::Io,
            format!("cannot generate a request ID: {error}"),
        )
    })?;
    Ok(u64::from_le_bytes(bytes).max(1))
}

#[cfg(test)]
mod tests;
