use std::os::unix::process::ExitStatusExt;
use std::time::{Duration, Instant};

use crate::guest::init::{self, Stream};
use crate::guest::sys::{self, Errno, FileDescriptor, c_string};
use crate::protocol::{
    self, APP_ACTIVATE, APP_CANCEL, APP_CAPTURE, APP_EXIT, APP_STDERR, APP_STDOUT, BUSY,
    CWD_FAILED, GuestExecPayload, LAUNCH_FAILED, OUTER_DATA,
};

use super::AgentConfig;
use super::diagnostics::with_description;
use super::launch;
use super::seam::System;
use super::session::{DataFailureKind, Event as SessionEvent, ReceiveFailure, Session};

/// Maximum bytes read from one output descriptor at a time.
const OUTPUT_CHUNK_BYTES: usize = 32 * 1024;
/// Readiness-poll interval, in milliseconds.
const POLL_INTERVAL_MS: u16 = 25;
/// The diagnostic that starts the message of a refused workload environment.
const ENVIRONMENT_DIAGNOSTIC: &[u8] = b"nvx-managed-agent: cannot configure workload environment: ";

/// Executes one validated workload request and streams its terminal outcome.
///
/// The agent drives its native plain-process workload in the C agent's loop: each turn polls the
/// workload and the control session, serves a control record, forwards the workload's output,
/// collects the statuses of the agent's children, and checks the workload's timeout. The agent
/// keeps the C agent's policy: the output cap, checked for each read of up to 32 KiB, and the
/// precedence of a cancellation, a timeout, and the cap over the workload's own status.
pub(crate) fn run(
    system: &mut impl System,
    session: &mut Session,
    config: &AgentConfig,
    request_id: u64,
    execution: &GuestExecPayload,
) -> Result<(), Errno> {
    if let Err(failure) = validate_launch_environment(system, config, execution) {
        match failure {
            EnvironmentFailure::Child(error, diagnostic) => {
                let message = with_description(diagnostic, error);
                session.send_app(APP_STDERR, request_id, 0, &message)?;
                return session.send_app(APP_EXIT, request_id, 125, b"exit");
            }
            EnvironmentFailure::Launch(error) => {
                super::diagnostics::error(system, "exec-launch", error as i32);
                return session.send_error(request_id, 125, LAUNCH_FAILED);
            }
        }
    }
    session.collect(|_, _| {})?;
    let mut workload = match launch(system, session, config, execution) {
        Ok(launched) => launched,
        Err(LaunchError::Start(failure)) => {
            settle_start(system, session, &failure)?;
            return session.send_error(request_id, 125, LAUNCH_FAILED);
        }
        Err(LaunchError::Other) => return session.send_error(request_id, 125, LAUNCH_FAILED),
    };
    let started = Instant::now();
    let mut state = RunState::default();
    let mut buffer = [0u8; OUTPUT_CHUNK_BYTES];
    while state.running(&workload) || (workload.has_open_output() && !state.ending()) {
        let session_ready = match poll(
            &workload,
            (!state.session_lost).then_some(session.descriptor()),
        ) {
            Ok(ready) => ready,
            Err(Errno::EINTR) => false,
            Err(_) => {
                workload.terminate();
                false
            }
        };
        if session_ready {
            match handle_control(system, session, request_id) {
                Ok(ExecControl::None) => {}
                Ok(ExecControl::Cancel) if state.running(&workload) && !state.cancelled => {
                    state.cancelled = true;
                    workload.cancel();
                }
                Ok(ExecControl::Cancel) => {}
                Ok(ExecControl::Capture) => {
                    workload.terminate();
                    session.sweep_capture().inspect_err(|error| {
                        super::diagnostics::error(system, "exec-capture-sweep", *error as i32);
                    })?;
                }
                Ok(ExecControl::SessionLost) => {
                    state.session_lost = true;
                    workload.disconnect();
                }
                Err(error) => {
                    workload.terminate();
                    if session.stopping() {
                        return Err(error);
                    }
                    if !state.status_lost
                        && let Err(failure) = workload.wait(session)
                    {
                        report_wait_failure(system, &failure);
                    }
                    report_kill_failures(system, &mut workload);
                    return Err(error);
                }
            }
        }
        let output = if state.session_lost || state.output_limited {
            &mut [][..]
        } else {
            &mut buffer[..]
        };
        state.forward(system, session, request_id, &mut workload, output)?;
        workload.collect(session)?;
        if state.running(&workload)
            && execution.timeout_ms != 0
            && started.elapsed() >= Duration::from_millis(u64::from(execution.timeout_ms))
        {
            state.timed_out = true;
            workload.terminate();
        }
    }
    let outcome = if state.ending() {
        if !state.session_lost && !state.output_limited {
            state.forward(system, session, request_id, &mut workload, &mut buffer)?;
        }
        state.forward(system, session, request_id, &mut workload, &mut [])?;
        report_kill_failures(system, &mut workload);
        None
    } else {
        state.finish(system, session, request_id, &mut workload, &mut buffer)?
    };

    let launch_report = state.launch.take().unwrap_or(Ok(None));
    if state.session_lost {
        return Ok(());
    }
    match launch_report {
        Ok(Some(failure)) => match failure.kind() {
            LaunchKind::WorkingDirectory => {
                return refuse_cwd(session, request_id, execution, failure.raw_os_error());
            }
            LaunchKind::Credentials => {
                super::diagnostics::error(system, "exec-launch", failure.raw_os_error());
                return session.send_error(request_id, 125, LAUNCH_FAILED);
            }
            LaunchKind::Exec => {
                let diagnostic = with_description(
                    b"nvx-managed-agent: cannot execute workload: ",
                    Errno::from_raw(failure.raw_os_error()),
                );
                session.send_app(APP_STDERR, request_id, 0, &diagnostic)?;
            }
            // The direct agent's other stages end the child with status 125 before the workload
            // runs, which the agent reports as the workload's exit status, as the C agent did.
            _ => {}
        },
        Err(error) => {
            super::diagnostics::error(system, "exec-launch-report", sys::errno_of(&error) as i32);
        }
        Ok(None) => {}
    }
    if state.cancelled {
        session.send_app(
            APP_EXIT,
            request_id,
            128 + sys::Signal::SIGKILL as i32,
            b"cancelled",
        )
    } else if state.timed_out {
        session.send_app(APP_EXIT, request_id, 124, b"timeout")
    } else if state.output_limited {
        session.send_app(APP_EXIT, request_id, 125, b"output-limit")
    } else {
        match outcome {
            Some(status) => match (status.code(), status.signal()) {
                (Some(code), _) => session.send_app(APP_EXIT, request_id, code, b"exit"),
                (None, Some(signal)) => {
                    session.send_app(APP_EXIT, request_id, 128 + signal, b"signal")
                }
                (None, None) => session.send_app(APP_EXIT, request_id, 125, b"failed"),
            },
            None => session.send_app(APP_EXIT, request_id, 125, b"failed"),
        }
    }
}

/// The direct core, behind the NVXC outcome policy.
enum Workload {
    Direct(init::Exec),
}

/// Events needed by the unchanged NVXC policy.
enum Event<'a> {
    Output(Stream, &'a [u8]),
    Launch(std::io::Result<Option<LaunchFailure>>),
    ReadFailed,
    DirectKillFailed(std::io::Error),
    Ended(std::process::ExitStatus),
    Other,
}

enum LaunchFailure {
    Direct(init::LaunchFailure),
}

enum LaunchKind {
    WorkingDirectory,
    Exec,
    Credentials,
    Other,
}

impl LaunchFailure {
    fn kind(&self) -> LaunchKind {
        let stage = match self {
            Self::Direct(failure) => failure.stage,
        };
        // The syscall boundary's stable child-report stage numbers.
        match stage {
            18 => LaunchKind::WorkingDirectory,
            21 => LaunchKind::Exec,
            13..=17 | 19 => LaunchKind::Credentials,
            _ => LaunchKind::Other,
        }
    }

    fn raw_os_error(&self) -> i32 {
        match self {
            Self::Direct(failure) => failure.error,
        }
    }
}

impl Workload {
    fn status(&self) -> Option<std::process::ExitStatus> {
        match self {
            Self::Direct(workload) => workload.status(),
        }
    }

    fn has_open_output(&self) -> bool {
        match self {
            Self::Direct(workload) => workload.has_open_output(),
        }
    }

    fn descriptors(&self) -> Vec<std::os::fd::BorrowedFd<'_>> {
        match self {
            Self::Direct(workload) => workload.descriptors().map(|(_, fd)| fd).collect(),
        }
    }

    fn next_event<'a>(&mut self, buffer: &'a mut [u8]) -> std::io::Result<Option<Event<'a>>> {
        match self {
            Self::Direct(workload) => workload.next_event(buffer).map(|event| {
                event.map(|event| match event {
                    init::Event::Output(stream, bytes) => Event::Output(stream, bytes),
                    init::Event::Launch(report) => {
                        Event::Launch(report.map(|failure| failure.map(LaunchFailure::Direct)))
                    }
                    init::Event::ReadFailed(..) => Event::ReadFailed,
                    init::Event::KillFailed(init::KillTarget::ProcessGroup, error) => {
                        Event::DirectKillFailed(error)
                    }
                    init::Event::Ended(outcome) => Event::Ended(outcome.status),
                    _ => Event::Other,
                })
            }),
        }
    }

    fn observe(&mut self, id: u32, status: std::process::ExitStatus) {
        match self {
            Self::Direct(workload) => workload.observe(id, status),
        };
    }

    fn collect(&mut self, session: &Session) -> Result<(), Errno> {
        session.collect(|id, status| self.observe(id, status))
    }

    fn terminate(&mut self) {
        match self {
            Self::Direct(workload) => workload.end(),
        }
    }

    fn cancel(&mut self) {
        match self {
            Self::Direct(workload) => workload.cancel(),
        }
    }

    fn disconnect(&mut self) {
        match self {
            Self::Direct(workload) => workload.reset(),
        }
    }

    fn discard(&mut self, stream: Stream) {
        match self {
            Self::Direct(workload) => workload.discard(stream),
        }
    }

    fn wait(&mut self, session: &Session) -> std::io::Result<std::process::ExitStatus> {
        loop {
            self.collect(session)?;
            if let Some(status) = self.status() {
                return Ok(status);
            }
            if let Some(Event::DirectKillFailed(error)) = self.next_event(&mut [])? {
                return Err(error);
            }
            match poll(self, None) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn kill_failure(&mut self) -> Option<Event<'static>> {
        match self {
            Self::Direct(workload) => workload.kill_failure().map(|event| match event {
                init::Event::KillFailed(init::KillTarget::ProcessGroup, error) => {
                    Event::DirectKillFailed(error)
                }
                _ => Event::Other,
            }),
        }
    }
}

/// Feeds the agent's sole status collector to its active or failed-start owner.
pub(super) fn collect(mut observe: impl FnMut(u32, std::process::ExitStatus)) {
    while let Some(reaped) = sys::reap_any_nonblocking() {
        observe(
            reaped.process.raw().unsigned_abs(),
            std::process::ExitStatus::from_raw(reaped.wait_status),
        );
    }
}

/// Keeps a failed native launch owned until its child is collected or explicitly lost.
fn settle_start(
    system: &mut impl System,
    session: &Session,
    failure: &init::StartFailure,
) -> Result<(), Errno> {
    super::diagnostics::error(system, "exec-launch", sys::errno_of(&failure.error) as i32);
    if let Some(error) = failure.cleanup {
        super::diagnostics::error(system, "exec-kill", error as i32);
    }
    if let Some(process) = failure.process {
        session.watch(process.raw().unsigned_abs());
        loop {
            let mut settled = false;
            session.collect(|id, status| {
                settled |= id == process.raw().unsigned_abs()
                    && (status.code().is_some() || status.signal().is_some());
            })?;
            if settled {
                break;
            }
            if !failure.is_pending()? {
                report_wait_failure(system, &Errno::ECHILD.into());
                break;
            }
            match sys::readable_or_hung_up(&[], POLL_INTERVAL_MS) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

/// What the agent learned of one workload while it ran: the C agent's flags, which decide the
/// outcome that it reports, and the workload's launch report and outcome.
#[derive(Default)]
struct RunState {
    /// Bytes of output forwarded so far, which the 1 MiB cap bounds.
    output_bytes: usize,
    /// Whether the host cancelled the workload while it ran.
    cancelled: bool,
    /// Whether the workload's timeout passed while it ran.
    timed_out: bool,
    /// Whether the workload's output exceeded the cap.
    output_limited: bool,
    /// Whether a reset detached the workload's session, after which its output is discarded.
    session_lost: bool,
    /// Whether the agent cannot collect the workload's status, as when another collector took
    /// it, after which it stops waiting for it.
    status_lost: bool,
    /// The workload's launch report, once it arrived.
    launch: Option<std::io::Result<Option<LaunchFailure>>>,
    /// The workload's outcome, once its execution ended.
    outcome: Option<std::process::ExitStatus>,
}

impl RunState {
    /// Whether the agent ended the workload, fixing its outcome or abandoning its session.
    fn ending(&self) -> bool {
        self.cancelled || self.timed_out || self.output_limited || self.session_lost
    }

    /// Returns whether the workload may still be running: its status is neither known nor lost.
    /// A workload whose status is lost has ended too, so that a cancellation or a timeout after
    /// that, while its output drains, changes its outcome no more than after a known status.
    fn running(&self, workload: &Workload) -> bool {
        workload.status().is_none() && !self.status_lost
    }

    /// Takes the workload's events until it has no step to take, forwarding its output as NVXC
    /// frames, until a read that the agent cannot forward: one beyond the cap, after which it
    /// reads and drops the rest of the output, or a failure to read or send, which ends the
    /// workload and the turn, as the C agent's did. A failure to collect the workload's status,
    /// the execution's only one, ends the turn too.
    fn forward(
        &mut self,
        system: &mut impl System,
        session: &mut Session,
        request_id: u64,
        workload: &mut Workload,
        buffer: &mut [u8],
    ) -> Result<(), Errno> {
        loop {
            if let Some((id, status)) = session.take_status() {
                workload.observe(id, status);
            }
            let event = match workload.next_event(buffer) {
                Ok(Some(event)) => event,
                Ok(None) => return Ok(()),
                Err(error) => {
                    self.lose_status(system, &error);
                    return Ok(());
                }
            };
            match event {
                Event::Output(stream, bytes) => {
                    if self.output_bytes > protocol::MAX_OUTPUT_BYTES - bytes.len() {
                        self.output_limited = true;
                        // The cap ends forwarding: read and drop the rest of both streams,
                        // as a lost session does, so the client receives a prefix of what the
                        // workload wrote, not a stream with a silent gap where a later chunk
                        // fit the leftover room.
                        workload.discard(Stream::Stdout);
                        workload.discard(Stream::Stderr);
                        workload.terminate();
                        return Ok(());
                    }
                    self.output_bytes += bytes.len();
                    let kind = match stream {
                        Stream::Stdout => APP_STDOUT,
                        Stream::Stderr => APP_STDERR,
                    };
                    if session.send_app(kind, request_id, 0, bytes).is_err() {
                        workload.terminate();
                        return Ok(());
                    }
                }
                Event::ReadFailed => {
                    workload.terminate();
                    return Ok(());
                }
                Event::Launch(report) => self.launch = Some(report),
                Event::DirectKillFailed(error) => {
                    super::diagnostics::error(system, "exec-kill", sys::errno_of(&error) as i32);
                    return Err(sys::errno_of(&error));
                }
                Event::Ended(outcome) => self.outcome = Some(outcome),
                Event::Other => {}
            }
        }
    }

    /// Notes that the agent cannot collect the workload's status, and reports it the first time.
    fn lose_status(&mut self, system: &mut impl System, error: &std::io::Error) {
        if !self.status_lost {
            self.status_lost = true;
            report_wait_failure(system, error);
        }
    }

    /// Takes the events that remain once the workload's status is known, or lost, and its
    /// output closed, up to the end of its execution, and returns its outcome, or `None` if its
    /// status is lost.
    fn finish(
        &mut self,
        system: &mut impl System,
        session: &mut Session,
        request_id: u64,
        workload: &mut Workload,
        buffer: &mut [u8],
    ) -> Result<Option<std::process::ExitStatus>, Errno> {
        loop {
            self.forward(system, session, request_id, workload, buffer)?;
            if self.outcome.is_some() || self.status_lost {
                return Ok(self.outcome);
            }
            match poll(workload, None) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// Waits at most one poll interval until one of the workload's descriptors or the session's,
/// if given, polls ready, and returns whether the session's did.
fn poll(workload: &Workload, session: Option<&FileDescriptor>) -> Result<bool, Errno> {
    let mut descriptors = workload.descriptors();
    if let Some(session) = session {
        descriptors.push(session.borrowed());
    }
    let ready = sys::readable_or_hung_up(&descriptors, POLL_INTERVAL_MS)?;
    Ok(session.is_some() && ready.last().copied().unwrap_or(false))
}

/// Reports the kill failures that the workload's execution has yet to report.
fn report_kill_failures(system: &mut impl System, workload: &mut Workload) {
    while let Some(event) = workload.kill_failure() {
        if let Event::DirectKillFailed(error) = event {
            super::diagnostics::error(system, "exec-kill", sys::errno_of(&error) as i32);
        }
    }
}

/// Reports a failure to collect the status of a workload, which leaves the agent without it.
fn report_wait_failure(system: &mut impl System, error: &std::io::Error) {
    super::diagnostics::error(system, "exec-wait", sys::errno_of(error) as i32);
}

/// Why a workload did not launch.
enum LaunchError {
    /// A native failure that may still own a child until the agent collects it.
    Start(init::StartFailure),
    /// Any other failure, which the agent reports as a failed launch alone, as the C agent did.
    Other,
}

impl From<Errno> for LaunchError {
    fn from(_: Errno) -> Self {
        Self::Other
    }
}

/// Starts a native direct workload.
fn launch(
    system: &mut impl System,
    session: &Session,
    config: &AgentConfig,
    execution: &GuestExecPayload,
) -> Result<Workload, LaunchError> {
    let workload = system
        .start_direct(launch::direct_request(config, execution)?)
        .map_err(LaunchError::Start)?;
    session.watch(workload.id());
    Ok(Workload::Direct(workload))
}

/// Failure discovered before the fork boundary.
#[derive(Debug)]
enum EnvironmentFailure {
    /// Failure whose exact stderr text belongs to the would-be child.
    Child(Errno, &'static [u8]),
    /// Parent-side launch validation failure.
    Launch(Errno),
}

/// Validates all environment mutations before allocating child resources: the workload's
/// `HOME`, `USER`, `LOGNAME`, and `PWD`, and the request's entries.
fn validate_launch_environment(
    system: &mut impl System,
    config: &AgentConfig,
    execution: &GuestExecPayload,
) -> Result<(), EnvironmentFailure> {
    for (name, value) in [
        (c"HOME", config.home.as_c_str()),
        (c"USER", config.user.as_c_str()),
        (c"LOGNAME", config.user.as_c_str()),
    ] {
        if let Err(error) = system.validate_environment(name, value) {
            return Err(EnvironmentFailure::Child(error, ENVIRONMENT_DIAGNOSTIC));
        }
    }
    if execution.environment.is_none() || execution.inherit_default_env {
        let pwd = c_string(execution.cwd.as_deref().unwrap_or(b"/"))
            .map_err(EnvironmentFailure::Launch)?;
        system
            .validate_environment(c"PWD", &pwd)
            .map_err(EnvironmentFailure::Launch)?;
    }
    if let Some(entries) = &execution.environment {
        for entry in entries {
            let equals = entry
                .iter()
                .position(|&byte| byte == b'=')
                .ok_or(EnvironmentFailure::Launch(Errno::EINVAL))?;
            let name = c_string(&entry[..equals]).map_err(EnvironmentFailure::Launch)?;
            let value = c_string(&entry[equals + 1..]).map_err(EnvironmentFailure::Launch)?;
            if let Err(error) = system.validate_environment(&name, &value) {
                return Err(EnvironmentFailure::Child(error, ENVIRONMENT_DIAGNOSTIC));
            }
        }
    }
    Ok(())
}

/// Sends the C-compatible stderr and application error for a cwd failure.
fn refuse_cwd(
    session: &mut Session,
    request_id: u64,
    execution: &GuestExecPayload,
    status: i32,
) -> Result<(), Errno> {
    let cwd = execution.cwd.as_deref().unwrap_or(b"/");
    let prefix = [
        b"nvx-managed-agent: cannot enter working directory ".as_slice(),
        cwd,
        b": ",
    ]
    .concat();
    let diagnostic = with_description(&prefix, Errno::from_raw(status));
    session.send_app(APP_STDERR, request_id, 0, &diagnostic)?;
    session.send_error(request_id, status, CWD_FAILED)
}

/// Control action observed while a workload is active.
#[derive(Clone, Copy)]
enum ExecControl {
    /// No action applies to the active workload.
    None,
    /// The host cancelled the active workload.
    Cancel,
    /// A reset detached this workload's session.
    SessionLost,
    /// Init accepted a capture that requires group termination and the repeated sweep.
    Capture,
}

/// Processes one control-console record while a workload runs.
fn handle_control(
    system: &mut impl System,
    session: &mut Session,
    exec_id: u64,
) -> Result<ExecControl, Errno> {
    let payload = match session.next_event() {
        Ok(SessionEvent::Reset) => {
            session.acknowledge()?;
            return Ok(ExecControl::SessionLost);
        }
        Ok(SessionEvent::Data(payload)) => payload,
        Err(_) if session.stopping() => return Err(Errno::ECANCELED),
        Err(failure) => return Err(report_control_failure(system, failure)),
    };
    let request = match session.accept_request(&payload) {
        Ok(request) => request,
        Err(failure) => {
            if failure.kind != DataFailureKind::Credit {
                super::diagnostics::error(system, "exec-record", i32::from(OUTER_DATA));
            }
            return Err(failure.into_error());
        }
    };
    if request.kind == APP_CANCEL {
        return Ok(
            if request.payload.is_empty() && request.request_id == exec_id {
                ExecControl::Cancel
            } else {
                ExecControl::None
            },
        );
    }
    if request.kind == APP_CAPTURE && session.has_capture() {
        return session.queue_capture(request, true).map(|accepted| {
            if accepted {
                ExecControl::Capture
            } else {
                ExecControl::None
            }
        });
    }
    if request.kind == APP_ACTIVATE && session.has_restored() {
        // The started activation never applies again: an identical retry gets its cached Ready,
        // and any other fails it, which ends the workload with the session.
        session.activate(request)?;
        return Ok(ExecControl::None);
    }
    session.send_error(request.request_id, Errno::EBUSY as i32, BUSY)?;
    Ok(ExecControl::None)
}

/// Reports a control record that ended the session while a workload ran, at the C agent's stage,
/// and returns the error that ends the session.
fn report_control_failure(system: &mut impl System, failure: ReceiveFailure) -> Errno {
    match failure {
        ReceiveFailure::Read(error) => {
            super::diagnostics::error(system, "exec-outer-read", error as i32);
            error
        }
        ReceiveFailure::Type(code) => {
            super::diagnostics::error(system, "exec-record", i32::from(code));
            Errno::EPROTO
        }
        // The C agent reported a RESET that it refused only as the error that ends the session.
        ReceiveFailure::Reset => Errno::EPROTO,
        ReceiveFailure::Data(failure) => {
            super::diagnostics::error(system, "exec-record", i32::from(OUTER_DATA));
            failure.into_error()
        }
    }
}

#[cfg(test)]
mod tests;
