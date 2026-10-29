//! The control session of `nvx-rs sandbox`, a port of the former
//! `scripts/nvx_tools/control_session.py` on the framing, sequencing, attachment, and transports
//! of [`crate::host::session`].
//!
//! It keeps the observable behavior of the Python session:
//!
//! - [`Session::connect`] retries the endpoint every 25 ms until its timeout, then attaches with
//!   the capability within a fresh timeout. On Windows, it gives up at once on the errors that
//!   Python does not retry.
//! - Each operation bounds its request and its response by one timeout. A transfer that times
//!   out fails with `managed control request timed out` while it sends and `managed control
//!   response timed out` while it receives.
//! - An endpoint that closes, as OpenVMM closes it when it shuts down, ends the operation with
//!   [`Exit::EndpointClosed`], while a session that OpenVMM resets fails with `managed control
//!   session was reset`.
//! - `exec` refuses invalid requests before it sends anything, with Python's messages, buffers
//!   the workload's output, and accepts the exit categories of `MANAGED_EXIT_CATEGORIES`; a
//!   refusal keeps its status and its category, which the guest sends in ASCII.
//! - A session whose transfer failed is closed, and refuses further requests.
//!
//! Unlike the Python session, it also hands the guest agent a host mapping table, as `start`
//! does for the shares of a sandbox: in order, and in as many requests as it takes. And
//! [`Session::connect_launched`], with which `start` waits for the endpoint of the OpenVMM that
//! it launched, stops waiting once that OpenVMM exits, as `nvx::host::session::connect` does,
//! where Python waited until its timeout.
//!
//! The transports also wait in slices of at most 250 ms, as Python's do, so that Ctrl-C ends a
//! wait within that time, except while a request is being sent, which its timeout bounds.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use super::deadline::{Deadline, RETRY_INTERVAL, sleep};
use super::{Exit, Result, check_interrupt};
use crate::ErrorCode;
use crate::host::platform::{self, Transport};
use crate::host::sandbox::options::validate_exec_environment;
use crate::host::session::{ControlSession, SessionError};
use crate::host::supervisor::Launched;
use crate::protocol::{
    APP_ERROR, APP_EXEC, APP_EXIT, APP_FEATURES, APP_HEADER_LEN, APP_MAPS, APP_PING, APP_READY,
    APP_STDERR, APP_STDOUT, APP_STOP, APP_STOPPED, CAPABILITY_LEN, GuestFeatures, HostMap,
    MAPPING_FAILED, MAX_OUTPUT_BYTES, OUTER_MAX_PAYLOAD, UNSUPPORTED_OPERATION, Workload,
    WorkloadEnvironment, encode_exec_payload, encode_maps_payloads,
};

/// The exit categories that `exec` accepts, `MANAGED_EXIT_CATEGORIES`. A cancellation, which this
/// session never requests, is not among them.
pub(crate) const EXIT_CATEGORIES: [&str; 5] =
    ["exit", "timeout", "output-limit", "signal", "failed"];
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

/// What the transport observed of the failure of a transfer.
#[derive(Default)]
struct Probe {
    /// [`SENDING`] or [`RECEIVING`]: the direction of the transfer in progress.
    direction: AtomicU8,
    /// Whether the endpoint closed the stream.
    closed: AtomicBool,
    /// Whether Ctrl-C ended a wait.
    interrupted: AtomicBool,
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
fn closes(error: &io::Error) -> bool {
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
            if platform::interrupted() {
                return Err(self.interrupted());
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

/// How a workload ended: `ManagedExecResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecResult {
    /// The workload's status.
    pub(crate) returncode: i32,
    /// One of [`EXIT_CATEGORIES`].
    pub(crate) category: &'static str,
    /// The workload's standard output.
    pub(crate) stdout: Vec<u8>,
    /// The workload's standard error.
    pub(crate) stderr: Vec<u8>,
}

/// An authenticated control session: `ControlSession` of the former `control_session.py`.
pub(crate) struct Session {
    /// The session, until a transfer fails.
    inner: Option<ControlSession>,
    /// What its transport observed.
    probe: Arc<Probe>,
}

impl Session {
    /// Connects to `endpoint`, which OpenVMM process `pid` serves, and attaches with
    /// `capability`, as `ControlSession.connect(endpoint, capability, timeout)` does: the endpoint
    /// gets `timeout` seconds to appear, and the attachment `timeout` seconds more.
    ///
    /// Like `nvx::host::session`, it refuses an endpoint that another process serves, which it
    /// treats as one that has not appeared yet, rather than handing that process the capability.
    pub(crate) fn connect(
        endpoint: &str,
        pid: u32,
        capability: &[u8; CAPABILITY_LEN],
        timeout: f64,
    ) -> Result<Self> {
        Self::connect_unless_exited(endpoint, pid, capability, timeout, || false)
    }

    /// Connects to `endpoint`, which `launched`, the OpenVMM process that a start launched,
    /// serves, as [`connect`](Self::connect) does, but stops waiting for the endpoint once
    /// OpenVMM has exited, as `nvx::host::session::connect` does: an OpenVMM that exits before it
    /// serves its endpoint, as one that refuses its configuration does, then ends the wait at
    /// once, rather than at its timeout. A check that fails keeps the wait going until the
    /// timeout, as there.
    pub(crate) fn connect_launched(
        endpoint: &str,
        launched: &mut Launched,
        capability: &[u8; CAPABILITY_LEN],
        timeout: f64,
    ) -> Result<Self> {
        let pid = launched.id();
        Self::connect_unless_exited(endpoint, pid, capability, timeout, || {
            launched
                .wait_timeout(Duration::ZERO)
                .is_ok_and(|status| status.is_some())
        })
    }

    /// Connects as [`connect`](Self::connect) does, but fails as soon as `exited`, which it calls
    /// after each attempt that finds no endpoint, tells that OpenVMM process `pid` has exited:
    /// with [`Exit::EndpointClosed`], as the session reports the `ProcessExited` of
    /// `nvx::host::session`.
    fn connect_unless_exited(
        endpoint: &str,
        pid: u32,
        capability: &[u8; CAPABILITY_LEN],
        timeout: f64,
        mut exited: impl FnMut() -> bool,
    ) -> Result<Self> {
        let deadline = Deadline::after(timeout);
        let transport = loop {
            check_interrupt()?;
            let slice = deadline
                .remaining()
                .map_or(POLL_SLICE, |left| left.min(POLL_SLICE));
            match platform::connect_endpoint(endpoint, pid, slice) {
                Ok(transport) => break transport,
                Err(error) => {
                    if cfg!(windows)
                        && error
                            .raw_os_error()
                            .is_some_and(|code| raised_at_once(code as u32))
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
        Self::attach(transport, capability, timeout)
    }

    /// Attaches over a connected `transport` with `capability` within `timeout` seconds, as the
    /// second half of [`connect`](Self::connect).
    fn attach(
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        timeout: f64,
    ) -> Result<Self> {
        let probe = Arc::new(Probe::default());
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
        Ok(session)
    }

    /// Returns the open session, or fails as a closed `ControlSession` does.
    fn session(&mut self) -> Result<&mut ControlSession> {
        self.inner
            .as_mut()
            .ok_or_else(|| Exit::error(ErrorCode::Protocol, "managed control session is closed"))
    }

    /// Sends a request and closes the session if the transfer fails.
    fn send(&mut self, kind: u8, request_id: u64, payload: &[u8], deadline: Deadline) -> Result {
        self.probe.direction.store(SENDING, Ordering::SeqCst);
        let sent = self
            .session()?
            .send_request(kind, request_id, payload, deadline.instant());
        sent.map_err(|error| self.fail(error))
    }

    /// Reads the next response and closes the session if the transfer fails.
    fn receive(&mut self, deadline: Deadline) -> Result<(u8, u64, i32, Vec<u8>)> {
        self.probe.direction.store(RECEIVING, Ordering::SeqCst);
        let received = self.session()?.read_response(Some(deadline.instant()));
        received.map_err(|error| self.fail(error))
    }

    /// Reads the next response, which must answer `request_id`, and returns its kind, status,
    /// and payload.
    fn reply(&mut self, request_id: u64, deadline: Deadline) -> Result<(u8, i32, Vec<u8>)> {
        let (kind, response_id, status, payload) = self.receive(deadline)?;
        if response_id != request_id {
            return Err(Exit::error(
                ErrorCode::Protocol,
                "managed guest returned a mismatched request ID",
            ));
        }
        Ok((kind, status, payload))
    }

    /// Sends a request of `kind` without a payload and waits up to `timeout` seconds until the
    /// guest agent acknowledges `what` with an empty `reply` of status 0.
    fn acknowledged(&mut self, kind: u8, reply: u8, what: &str, timeout: f64) -> Result {
        let request_id = request_id()?;
        let deadline = Deadline::after(timeout);
        self.send(kind, request_id, &[], deadline)?;
        let (kind, response_id, status, payload) = self.receive(deadline)?;
        if kind != reply || response_id != request_id || status != 0 || !payload.is_empty() {
            return Err(Exit::error(
                ErrorCode::Protocol,
                format!("managed guest did not acknowledge {what}"),
            ));
        }
        Ok(())
    }

    /// Closes the session after a failed transfer and returns the failure.
    fn fail(&mut self, error: SessionError) -> Exit {
        self.inner = None;
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
        self.acknowledged(APP_PING, APP_READY, "readiness", timeout)
    }

    /// Asks the guest agent which control features its image provides. An agent that predates
    /// the request refuses it as an unsupported operation, so its image provides none.
    pub(crate) fn features(&mut self, timeout: f64) -> Result<GuestFeatures> {
        let request_id = request_id()?;
        let deadline = Deadline::after(timeout);
        self.send(APP_FEATURES, request_id, &[], deadline)?;
        let (kind, status, payload) = self.reply(request_id, deadline)?;
        let invalid = || {
            Exit::error(
                ErrorCode::Protocol,
                "managed guest returned an invalid features response",
            )
        };
        match kind {
            APP_READY if status == 0 => GuestFeatures::decode(&payload).map_err(|_| invalid()),
            APP_ERROR if payload == UNSUPPORTED_OPERATION => Ok(GuestFeatures::NONE),
            _ => Err(invalid()),
        }
    }

    /// Hands the guest agent the host mapping table `maps`, in order and in as many `MAPS`
    /// requests as it takes, and waits until the agent has mounted every entry, all within
    /// `timeout` seconds. The agent refuses workloads until then, and refuses the rest of the
    /// table after an entry that it could not mount.
    pub(crate) fn map_host_paths(&mut self, maps: &[HostMap], timeout: f64) -> Result {
        let payloads = encode_maps_payloads(maps, OUTER_MAX_PAYLOAD - APP_HEADER_LEN)
            .map_err(|error| Exit::error(ErrorCode::Protocol, error.message()))?;
        let deadline = Deadline::after(timeout);
        for payload in payloads {
            let request_id = request_id()?;
            self.send(APP_MAPS, request_id, &payload, deadline)?;
            let (kind, status, payload) = self.reply(request_id, deadline)?;
            match kind {
                APP_READY if status == 0 && payload.is_empty() => {}
                APP_ERROR if payload == MAPPING_FAILED.as_bytes() => {
                    return Err(Exit::error(
                        ErrorCode::Protocol,
                        format!(
                            "managed guest could not mount a mapped host path (error {status})"
                        ),
                    ));
                }
                APP_ERROR => {
                    return Err(Exit::error(
                        ErrorCode::Protocol,
                        format!(
                            "managed guest did not mount the host paths: {} (status {status})",
                            String::from_utf8_lossy(&payload)
                        ),
                    ));
                }
                _ => {
                    return Err(Exit::error(
                        ErrorCode::Protocol,
                        "managed guest returned an invalid host mapping response",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Runs a workload and returns how it ended, as `ControlSession.exec` does: the request and
    /// every response must arrive within `response_timeout` seconds of each other's start.
    pub(crate) fn exec(
        &mut self,
        request: &ExecRequest,
        response_timeout: f64,
    ) -> Result<ExecResult> {
        let refused = |message: &str| Exit::error(ErrorCode::Unsupported, message);
        if !(response_timeout > 0.0 && response_timeout < f64::INFINITY) {
            return Err(refused(
                "managed exec response timeout must be positive and finite",
            ));
        }
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
        let payload = encode_exec_payload(&workload)
            .map_err(|_| refused("managed exec request exceeds the protocol limit"))?;
        let request_id = request_id()?;
        let deadline = Deadline::after(response_timeout);
        self.send(APP_EXEC, request_id, &payload, deadline)?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            let (kind, status, response) = self.reply(request_id, deadline)?;
            let violation = |message: &str| Exit::error(ErrorCode::Protocol, message);
            match kind {
                APP_STDOUT | APP_STDERR => {
                    // The guest agent forwards at most this much output, so more breaks the
                    // protocol, as `nvx::host::instance` also finds.
                    if stdout.len() + stderr.len() + response.len() > MAX_OUTPUT_BYTES {
                        return Err(violation(&format!(
                            "managed guest exceeded the {MAX_OUTPUT_BYTES}-byte output limit"
                        )));
                    }
                    let output = if kind == APP_STDOUT {
                        &mut stdout
                    } else {
                        &mut stderr
                    };
                    output.extend_from_slice(&response);
                }
                APP_EXIT => {
                    if !response.is_ascii() {
                        return Err(violation("managed guest returned an invalid exit category"));
                    }
                    let category = EXIT_CATEGORIES
                        .iter()
                        .find(|category| category.as_bytes() == response)
                        .ok_or_else(|| {
                            violation("managed guest returned an unsupported exit category")
                        })?;
                    return Ok(ExecResult {
                        returncode: status,
                        category,
                        stdout,
                        stderr,
                    });
                }
                APP_ERROR => {
                    // `bytes.decode("ascii", "replace")`.
                    let category: String = response
                        .iter()
                        .map(|&byte| {
                            if byte.is_ascii() {
                                char::from(byte)
                            } else {
                                '\u{fffd}'
                            }
                        })
                        .collect();
                    return Err(Exit::error(
                        ErrorCode::ExecRefused,
                        format!(
                            "managed guest rejected exec (status={status}, category={category})"
                        ),
                    ));
                }
                _ => {
                    return Err(violation("managed guest returned an invalid exec response"));
                }
            }
        }
    }

    /// Asks the guest agent to shut the VM down, as `ControlSession.stop` does.
    pub(crate) fn stop(&mut self, timeout: f64) -> Result {
        self.acknowledged(APP_STOP, APP_STOPPED, "stop", timeout)
    }
}

/// Returns Python's message for a violation of the protocol that `nvx::host::session` reports.
///
/// The two sessions report the same violations of records that have one fault, and their
/// messages differ only where Python folds a case into a broader one.
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
        _ => message,
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
