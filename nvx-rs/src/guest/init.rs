//! `nvx-init TIME_DAEMON [ANNOUNCER]`: NVX's PID 1, which initializes a layered root and serves
//! its managed control session through the same dispatcher as blockless VMs.
//!
//! A layered root's early boot, which the shell `/init` runs, mounts the kernel's file systems,
//! runs `nvx-time boot`, whose child becomes the time daemon, configures the network, and, when
//! the network publishes the workload's listeners, starts their announcer, `/sbin/nvx-ingress`.
//! To hand PID 1 over to `nvx-init`, the early boot replaces itself with it, so that `nvx-init`
//! inherits both helpers as its children, and passes it their process IDs: `TIME_DAEMON`, and
//! `ANNOUNCER` when the announcer runs. Each is a decimal process ID above 1, and the two differ.
//! It reads the layered root, decoded view, shares and mapping count from the kernel command
//! line, mounts the root's runtime with the time status, and switches once. The control terminal,
//! mapping descriptors, table and resolved account defaults stay open across that switch.
//! As PID 1, `nvx-init`:
//!
//! - reads `SIGCHLD` and `SIGTERM` from a signal descriptor. `SIGCHLD` wakes it to collect the
//!   children that ended, and `SIGTERM` asks it to end the VM, which it does once it collected
//!   the children that ended before: a helper among them ends the VM as a failure. No other
//!   signal reaches it: the kernel delivers to PID 1 only the signals that it handles or blocks;
//! - gives itself and each helper an out-of-memory score adjustment of -1000, which exempts them
//!   from the out-of-memory killer, and a nice value of -10, ahead of the workloads' 0;
//! - registers exactly those helpers, and ends the VM when either ends, whatever its status: the
//!   time daemon disciplines the guest's clock and watches it, and the announcer tells the gateway
//!   the workload's listeners, so the VM cannot go on without them. A helper that ended before
//!   `nvx-init` started, whose status it then collects first, ends the VM too, as does a process ID
//!   that is not one of PID 1's children, whose end PID 1 could not tell;
//! - collects the status of every child that ends, which includes every orphaned process, whose
//!   parent the kernel makes PID 1, from one `waitpid(-1)` loop, the process's only one;
//! - writes each diagnostic to the console, which it opens for each line without making it its
//!   controlling terminal, or to its standard error when the console does not open, and reads
//!   nothing from it;
//! - ends the VM itself, without another guest program: it writes every file system's modified
//!   data to its storage, with `sync`, and then stops the VM through OpenVMM's control port with a
//!   status: 0 after a host `STOP` or `SIGTERM`, or 125, as `nvx-init-agent`'s failures do, after
//!   a failure, which an `NVX-INIT-ERROR:` line on the console names first.
//!
//! Outside PID 1, `nvx-init` changes nothing: it fails with status 2 and a message.

use std::ffi::{CStr, CString};
use std::io::{self, Read};
use std::net::Ipv4Addr;
use std::os::fd::BorrowedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitCode, ExitStatus as ProcessStatus};
use std::time::{Duration, Instant};

use crate::activation::Publication;

use super::parse::parse_decimal;
use super::status::ExitStatus;
use super::sys::{
    self, ChildStatus, Errno, FileDescriptor, Mode, OpenFlags, Pollable, ProcessId, Signal,
    SignalDescriptor, kernel,
};

mod boot;

/// The process ID of the init process.
const INIT_PROCESS: i32 = 1;
/// The VM's exit status after a failure of PID 1 or the end of a helper: that of
/// `nvx-init-agent`'s failures.
const FAILURE: u8 = 125;
/// The out-of-memory score adjustment of PID 1 and its helpers, Linux's `OOM_SCORE_ADJ_MIN`,
/// which exempts them from the out-of-memory killer.
const OOM_SCORE_ADJUSTMENT: i32 = -1000;
/// The nice value of PID 1 and its helpers, ahead of the workloads' 0.
const NICE: i32 = -10;
/// The signals that PID 1 reads from its signal descriptor: the end of a child, and the request
/// to end the VM.
const SIGNALS: [Signal; 2] = [Signal::SIGCHLD, Signal::SIGTERM];

/// The helpers that the shell `/init` hands over: the only processes whose end ends the VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Helpers {
    /// The time daemon.
    time: ProcessId,
    /// The announcer of the workload's listeners, when the network publishes them.
    announcer: Option<ProcessId>,
}

impl Helpers {
    /// Parses `TIME_DAEMON [ANNOUNCER]`, the arguments after the program's name, or returns
    /// `None` for anything else.
    fn parse(arguments: &[CString]) -> Option<Self> {
        let process = |argument: &CString| {
            parse_decimal(argument.as_bytes())
                .and_then(|id| i64::try_from(id).ok())
                .and_then(ProcessId::from_positive)
                .filter(|process| process.raw() != INIT_PROCESS)
        };
        let helpers = match arguments {
            [time] => Self {
                time: process(time)?,
                announcer: None,
            },
            [time, announcer] => Self {
                time: process(time)?,
                announcer: Some(process(announcer)?),
            },
            _ => return None,
        };
        (helpers.announcer != Some(helpers.time)).then_some(helpers)
    }

    /// Returns each helper's process ID, with the helper's name.
    fn named(self) -> impl Iterator<Item = (&'static str, ProcessId)> {
        std::iter::once(("the time daemon", self.time))
            .chain(self.announcer.map(|announcer| ("the announcer", announcer)))
    }

    /// Returns the name of the helper `process`, or `None` for another process.
    fn name(self, process: ProcessId) -> Option<&'static str> {
        self.named()
            .find(|&(_, helper)| helper == process)
            .map(|(name, _)| name)
    }

    /// Kills every remaining userspace process but PID 1 and the registered helpers, repeating
    /// until none remains, including descendants that left their execution's process group.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "SL7.7a calls the capture sweep")
    )]
    pub(crate) fn sweep(self) -> Result<(), Errno> {
        if sys::current_process().raw() != INIT_PROCESS {
            return Err(Errno::EPERM);
        }
        self.sweep_with(ProcessId::live_userspace, sys::kill_process)
    }

    /// Runs the narrow process-table sweep with the supplied walk and kill operations.
    fn sweep_with(
        self,
        mut processes: impl FnMut() -> Result<Vec<ProcessId>, Errno>,
        mut kill: impl FnMut(ProcessId) -> Result<(), Errno>,
    ) -> Result<(), Errno> {
        loop {
            let mut found = false;
            for process in processes()? {
                if process.raw() == INIT_PROCESS || self.name(process).is_some() {
                    continue;
                }
                found = true;
                match kill(process) {
                    Ok(()) | Err(Errno::ESRCH) => {}
                    Err(error) => return Err(error),
                }
            }
            if !found {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Whether execution requires an account in the shared root's databases.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL7.6a selects an execution account mode")
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountMode {
    Existing,
    Numeric,
}

/// One output pipe of a plain execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

/// A descriptor that moves the execution on; status comes only from PID 1's collector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Launch,
    Output(Stream),
}

/// Why an execution was ended rather than completing on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    Cancelled,
    Terminated,
    TimedOut,
    Disconnected,
}

/// A target of ending-only termination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KillTarget {
    Program,
    ProcessGroup,
}

/// A failed child setup operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LaunchFailure {
    /// The syscall boundary's stage number.
    pub(crate) stage: u32,
    pub(crate) error: i32,
}

/// A failed start and, after a fork, the child still owned by the caller's sole collector.
///
/// The owner retains this failure and services its collector until `process` has a terminal
/// status before returning to idle. Killing a child does not collect its status.
#[derive(Debug)]
pub(crate) struct StartFailure {
    /// The first failure; cleanup never replaces it.
    pub(crate) error: io::Error,
    /// The child whose terminal status still belongs to the caller's collector.
    pub(crate) process: Option<ProcessId>,
    /// A failed attempt to end the child or its process group.
    pub(crate) cleanup: Option<Errno>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "SL7.6a settles failed starts before returning idle"
    )
)]
impl StartFailure {
    /// Checks without collecting. The owner hands over queued terminal statuses first; `false`
    /// without one means the child's status was lost, including automatic kernel collection.
    pub(crate) fn is_pending(&self) -> Result<bool, Errno> {
        self.process.map_or(Ok(false), ProcessId::is_unreaped)
    }

    /// Transfers the first error, owned child, and cleanup diagnostic to their control owner.
    pub(crate) fn into_parts(self) -> (io::Error, Option<ProcessId>, Option<Errno>) {
        (self.error, self.process, self.cleanup)
    }
}

impl From<io::Error> for StartFailure {
    fn from(error: io::Error) -> Self {
        Self {
            error,
            process: None,
            cleanup: None,
        }
    }
}

impl From<Errno> for StartFailure {
    fn from(error: Errno) -> Self {
        io::Error::from(error).into()
    }
}

impl std::fmt::Display for LaunchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "launch stage {}: {}",
            self.stage,
            io::Error::from_raw_os_error(self.error)
        )
    }
}

/// The terminal status and any requested ending, after launch and output have settled.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL7.6a consumes execution outcomes")
)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Outcome {
    pub(crate) status: ProcessStatus,
    pub(crate) cause: Option<Cause>,
    pub(crate) discarded: [u64; 2],
}

/// One nonblocking step of the init-owned execution.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL7.6a consumes execution events")
)]
#[derive(Debug)]
pub(crate) enum Event<'a> {
    Launch(io::Result<Option<LaunchFailure>>),
    Output(Stream, &'a [u8]),
    Discarded(Stream, usize),
    Closed(Stream),
    ReadFailed(Stream, io::Error),
    KillFailed(KillTarget, io::Error),
    Ended(Outcome),
}

/// The inputs of one plain-process execution in the VM's shared root.
pub(crate) struct ExecRequest {
    /// Program path.
    pub(crate) program: CString,
    /// Arguments, including argument zero.
    pub(crate) arguments: Vec<CString>,
    /// Exact environment.
    pub(crate) environment: Vec<CString>,
    /// Absolute working directory in the shared root.
    pub(crate) directory: CString,
    /// Real, effective, and saved user ID.
    pub(crate) uid: u32,
    /// Real, effective, and saved group ID.
    pub(crate) gid: u32,
    /// Whether the image must contain this account.
    pub(crate) account: AccountMode,
    /// How long the leader may run.
    pub(crate) timeout: Option<Duration>,
    /// A pipe for streaming input, or the null device.
    pub(crate) input: bool,
    /// The program search, when requested by the caller.
    pub(crate) search_path: Option<CString>,
    /// Whether the child records its physical working directory in `PWD`.
    pub(crate) physical_pwd: bool,
}

impl ExecRequest {
    fn check(&self) -> io::Result<()> {
        if self.arguments.is_empty()
            || self.directory.as_bytes().first() != Some(&b'/')
            || self.uid == u32::MAX
            || self.gid == u32::MAX
        {
            return Err(Errno::EINVAL.into());
        }
        match self.account {
            AccountMode::Numeric => Ok(()),
            AccountMode::Existing => {
                let root = sys::open(
                    c"/",
                    OpenFlags::O_PATH | OpenFlags::O_CLOEXEC,
                    Mode::empty(),
                )?;
                let read = |path| -> io::Result<Vec<u8>> {
                    let file = sys::open_beneath(
                        &root,
                        path,
                        OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_NOCTTY,
                        Mode::empty(),
                        true,
                    )?;
                    let (regular, length) = sys::file_size(&file)?;
                    if !regular || length > 1 << 20 {
                        return Err(Errno::EINVAL.into());
                    }
                    let mut bytes = Vec::new();
                    file.take((1 << 20) + 1).read_to_end(&mut bytes)?;
                    if bytes.len() > 1 << 20 {
                        return Err(Errno::EFBIG.into());
                    }
                    Ok(bytes)
                };
                let passwd = read(c"etc/passwd")?;
                let group = read(c"etc/group")?;
                let mut matching = passwd.split(|&byte| byte == b'\n').filter_map(|line| {
                    if matches!(line.trim_ascii_start().first(), None | Some(b'#')) {
                        return None;
                    }
                    let fields: Vec<_> = line.split(|&byte| byte == b':').collect();
                    (parse_decimal(fields.get(2)?) == Some(self.uid.into())).then_some(fields)
                });
                let fields = matching.next().ok_or(Errno::ENOENT)?;
                if matching.next().is_some()
                    || fields.len() != 7
                    || fields[0].is_empty()
                    || parse_decimal(fields[3]) != Some(self.gid.into())
                    || fields[5].first() != Some(&b'/')
                    || !group.split(|&byte| byte == b'\n').any(|line| {
                        if matches!(line.trim_ascii_start().first(), None | Some(b'#')) {
                            return false;
                        }
                        line.split(|&byte| byte == b':')
                            .nth(2)
                            .and_then(parse_decimal)
                            == Some(self.gid.into())
                    })
                {
                    return Err(Errno::EINVAL.into());
                }
                Ok(())
            }
        }
    }
}

/// The parent's nonblocking, fixed-size launch report; only the child owns its writer.
struct LaunchReport {
    reader: FileDescriptor,
    bytes: [u8; sys::REPORT_LENGTH],
    length: usize,
}

impl LaunchReport {
    fn read(&mut self) -> Option<io::Result<Option<LaunchFailure>>> {
        while self.length < self.bytes.len() {
            match self.reader.read(&mut self.bytes[self.length..]) {
                Ok(0) if self.length == 0 => return Some(Ok(None)),
                Ok(0) => return Some(Err(Errno::EPROTO.into())),
                Ok(length) => self.length += length,
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => return None,
                Err(error) => return Some(Err(error.into())),
            }
        }
        let code = u32::from_ne_bytes(self.bytes[..4].try_into().expect("four bytes"));
        let error = i32::from_ne_bytes(self.bytes[4..].try_into().expect("four bytes"));
        Some(
            matches!(code, 1 | 4 | 13..=16 | 18..=21)
                .then_some(Some(LaunchFailure { stage: code, error }))
                .ok_or_else(|| Errno::EPROTO.into()),
        )
    }
}

/// One execution, with no arbitrary-signal interface or per-execution containment.
pub(crate) struct Exec {
    leader: ProcessId,
    /// Pins the original group even after PID 1 collects the leader and its number is reused.
    group: FileDescriptor,
    launch: Option<LaunchReport>,
    input: Option<FileDescriptor>,
    outputs: [Option<FileDescriptor>; 2],
    discarding: [bool; 2],
    discarded: [u64; 2],
    turn: usize,
    status: Option<ProcessStatus>,
    status_lost: bool,
    cause: Option<Cause>,
    deadline: Option<Instant>,
    failures: [Option<io::Error>; 2],
    ended: bool,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "SL7.6a drives nvx-init's exec core")
)]
impl Exec {
    /// Forks the program in its own process group, resetting its priority before taking its
    /// identity, with empty groups and capabilities and `no_new_privs`, before entering its cwd.
    pub(crate) fn start(request: ExecRequest) -> Result<Self, StartFailure> {
        request.check()?;
        let mut spec = sys::ChildSpec::new(request.program, request.arguments);
        spec.set_environment(request.environment);
        spec.set_directory(request.directory);
        spec.set_workload(request.uid, request.gid);
        if let Some(path) = request.search_path {
            spec.set_search_path(path);
        }
        if request.physical_pwd {
            spec.export_physical_pwd();
        }
        #[cfg(test)]
        tests::prepare_child(&mut spec);
        let input = if request.input {
            let (reader, writer) = sys::create_pipe()?;
            writer.set_nonblocking()?;
            spec.set_stream(0, reader);
            Some(writer)
        } else {
            spec.set_stream(
                0,
                sys::open(
                    c"/dev/null",
                    OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
                    Mode::empty(),
                )?,
            );
            None
        };
        let (stdout, writer) = sys::create_pipe()?;
        stdout.set_nonblocking()?;
        spec.set_stream(1, writer);
        let (stderr, writer) = sys::create_pipe()?;
        stderr.set_nonblocking()?;
        spec.set_stream(2, writer);
        let started = sys::PreparedChild::new(spec)?.spawn()?;
        Self::from_started(
            started.process,
            started.report,
            input,
            [Some(stdout), Some(stderr)],
            request.timeout,
        )
    }

    /// Finishes parent-side descriptors without surrendering the collector's child on failure.
    fn from_started(
        leader: ProcessId,
        report: FileDescriptor,
        input: Option<FileDescriptor>,
        outputs: [Option<FileDescriptor>; 2],
        timeout: Option<Duration>,
    ) -> Result<Self, StartFailure> {
        let group = match sys::open_pidfd(leader).and_then(|group| {
            report.set_nonblocking()?;
            Ok(group)
        }) {
            Ok(group) => group,
            Err(error) => {
                let cleanup = [sys::kill_process(leader), sys::kill_process_group(leader)]
                    .into_iter()
                    .filter_map(Result::err)
                    .find(|error| *error != Errno::ESRCH);
                return Err(StartFailure {
                    error: error.into(),
                    process: Some(leader),
                    cleanup,
                });
            }
        };
        Ok(Self {
            leader,
            group,
            launch: Some(LaunchReport {
                reader: report,
                bytes: [0; sys::REPORT_LENGTH],
                length: 0,
            }),
            input,
            outputs,
            discarding: [false; 2],
            discarded: [0; 2],
            turn: 0,
            status: None,
            status_lost: false,
            cause: None,
            deadline: timeout.and_then(|timeout| Instant::now().checked_add(timeout)),
            failures: [None, None],
            ended: false,
        })
    }

    /// Returns the leader's process ID.
    pub(crate) fn id(&self) -> u32 {
        self.leader.raw().unsigned_abs()
    }

    /// Takes the leader's terminal status from PID 1's sole collector and kills the rest of its
    /// group even after a normal exit, before waiting for the pipes to close.
    ///
    /// The collector hands the status over before driving the execution again. Otherwise the
    /// non-reaping lost-status check can correctly see that a collector took it but not who did.
    pub(crate) fn observe(&mut self, id: u32, status: ProcessStatus) -> bool {
        if id != self.id()
            || self.status.is_some()
            || (status.code().is_none() && status.signal().is_none())
        {
            return false;
        }
        self.status = Some(status);
        self.input = None;
        self.kill_group();
        true
    }

    /// Takes the next launch, pipe, kill-failure, or status event, including timeout termination.
    pub(crate) fn next_event<'a>(&mut self, buffer: &'a mut [u8]) -> io::Result<Option<Event<'a>>> {
        if self.ended {
            return Ok(None);
        }
        if self.status.is_none() && !self.status_lost && !sys::is_unreaped(self.group.borrowed())? {
            self.status_lost = true;
            self.input = None;
            self.kill_group();
            return Err(Errno::ECHILD.into());
        }
        if self
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.end_with(Cause::TimedOut);
        }
        if let Some(event) = self.kill_failure() {
            return Ok(Some(event));
        }
        if let Some(result) = self.launch.as_mut().and_then(LaunchReport::read) {
            self.launch = None;
            return Ok(Some(Event::Launch(result)));
        }
        for _ in 0..2 {
            let index = self.turn;
            self.turn ^= 1;
            let stream = [Stream::Stdout, Stream::Stderr][index];
            let Some(reader) = &self.outputs[index] else {
                continue;
            };
            if buffer.is_empty() {
                continue;
            }
            match reader.read(buffer) {
                Ok(0) => {
                    self.outputs[index] = None;
                    return Ok(Some(Event::Closed(stream)));
                }
                Ok(length) if self.discarding[index] => {
                    self.discarded[index] = self.discarded[index].saturating_add(length as u64);
                    return Ok(Some(Event::Discarded(stream, length)));
                }
                Ok(length) => return Ok(Some(Event::Output(stream, &buffer[..length]))),
                Err(Errno::EINTR) => self.turn = index,
                Err(Errno::EAGAIN) => {}
                Err(error) => {
                    self.outputs[index] = None;
                    return Ok(Some(Event::ReadFailed(stream, error.into())));
                }
            }
        }
        if let Some(status) = self.status
            && self.launch.is_none()
            && !self.has_open_output()
        {
            self.ended = true;
            return Ok(Some(Event::Ended(Outcome {
                status,
                cause: self.cause,
                discarded: self.discarded,
            })));
        }
        Ok(None)
    }

    /// Lends the descriptors that the caller polls readable.
    pub(crate) fn descriptors(&self) -> impl Iterator<Item = (Source, BorrowedFd<'_>)> {
        let outputs = self
            .outputs
            .iter()
            .zip([Stream::Stdout, Stream::Stderr])
            .filter_map(|(reader, stream)| {
                Some((Source::Output(stream), reader.as_ref()?.borrowed()))
            });
        self.launch
            .iter()
            .map(|report| (Source::Launch, report.reader.borrowed()))
            .chain(outputs)
    }

    /// Lends the input pipe, which the caller polls writable.
    pub(crate) fn input(&self) -> Option<BorrowedFd<'_>> {
        self.input.as_ref().map(FileDescriptor::borrowed)
    }

    /// Writes input without blocking or raising `SIGPIPE`.
    pub(crate) fn write_input(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let input = self.input.as_ref().ok_or(Errno::EPIPE)?;
        loop {
            match sys::write_without_sigpipe(input, bytes) {
                Err(Errno::EINTR) => {}
                Err(Errno::EPIPE) => {
                    self.input = None;
                    return Err(Errno::EPIPE.into());
                }
                result => return result.map_err(Into::into),
            }
        }
    }

    /// Delivers end of input.
    pub(crate) fn close_input(&mut self) {
        self.input = None;
    }

    /// Returns the pending timeout's deadline.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
            .filter(|_| self.status.is_none() && !self.status_lost && self.cause.is_none())
    }

    /// Ends the execution and kills its process group.
    pub(crate) fn end(&mut self) {
        self.end_with(Cause::Terminated);
    }

    /// Cancels the execution and kills its process group.
    pub(crate) fn cancel(&mut self) {
        self.end_with(Cause::Cancelled);
    }

    /// Ends the execution on session reset, killing its group and closing discarded output,
    /// including pipes that an escaped process keeps open.
    pub(crate) fn reset(&mut self) {
        self.discarding = [true; 2];
        self.outputs = [None, None];
        self.end_with(Cause::Disconnected);
    }

    pub(crate) fn status(&self) -> Option<ProcessStatus> {
        self.status
    }

    pub(crate) fn has_open_output(&self) -> bool {
        self.outputs.iter().any(Option::is_some)
    }

    pub(crate) fn output_hung_up(&self, stream: Stream) -> io::Result<bool> {
        self.outputs[stream as usize]
            .as_ref()
            .map_or(Ok(true), |reader| {
                sys::hung_up(reader.borrowed()).map_err(Into::into)
            })
    }

    pub(crate) fn discard(&mut self, stream: Stream) {
        self.discarding[stream as usize] = true;
    }

    pub(crate) fn kill_failure(&mut self) -> Option<Event<'static>> {
        self.failures
            .iter_mut()
            .zip([KillTarget::Program, KillTarget::ProcessGroup])
            .find_map(|(error, target)| error.take().map(|error| Event::KillFailed(target, error)))
    }

    fn end_with(&mut self, cause: Cause) {
        if self.ended {
            return;
        }
        self.input = None;
        self.deadline = None;
        if self.status.is_none() && !self.status_lost {
            match sys::kill_through_pidfd(self.group.borrowed(), false) {
                Err(Errno::ESRCH) => {}
                result => {
                    if self.cause.is_none() {
                        self.cause = Some(cause);
                    }
                    if let Err(error) = result {
                        self.failures[0] = Some(error.into());
                    }
                }
            }
        }
        self.kill_group();
    }

    fn kill_group(&mut self) {
        match sys::kill_through_pidfd(self.group.borrowed(), true) {
            Ok(()) | Err(Errno::ESRCH) => {}
            Err(error) => self.failures[1] = Some(error.into()),
        }
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        if !self.ended {
            self.reset();
            while let Some(Event::KillFailed(target, error)) = self.kill_failure() {
                sys::write_standard_error(
                    format!("nvx-init: cannot end {target:?}: {error}\n").as_bytes(),
                );
            }
        }
    }
}

/// The operations of PID 1 on the system, which tests replace.
trait System {
    /// Blocks [`SIGNALS`], with the default disposition of `SIGCHLD`, and opens the descriptor
    /// that reads them.
    fn listen(&mut self) -> Result<(), Errno>;
    /// Gives `process`, or PID 1 for `None`, [`OOM_SCORE_ADJUSTMENT`] and [`NICE`].
    fn raise_priority(&mut self, process: Option<ProcessId>) -> Result<(), Errno>;
    /// Collects the status of the child `process` if it ended, returns `None` while it runs, and
    /// fails with `ECHILD` if `process` is not a child.
    fn adopt(&mut self, process: ProcessId) -> Result<Option<ChildStatus>, Errno>;
    /// Collects the status of one child that ended, without waiting, or returns `None` when none
    /// did.
    fn reap(&mut self) -> Option<(ProcessId, ChildStatus, i32)>;
    /// Writes the diagnostic `line` to the console.
    fn report(&mut self, line: &[u8]);
    /// Writes every file system's modified data to its storage, and then stops the VM with
    /// `status`. In production it does not return.
    fn shut_down(&mut self, status: u8);
}

/// The production operations.
struct RealSystem {
    /// The descriptor that reads [`SIGNALS`], once [`listen`](System::listen) opened it.
    signals: Option<SignalDescriptor>,
}

impl System for RealSystem {
    fn listen(&mut self) -> Result<(), Errno> {
        // An inherited `SIG_IGN` would have the kernel collect each child's status itself.
        sys::reset_child_signal();
        self.signals = Some(SignalDescriptor::new(&SIGNALS)?);
        Ok(())
    }

    fn raise_priority(&mut self, process: Option<ProcessId>) -> Result<(), Errno> {
        let path = match process {
            None => kernel::OOM_SCORE_ADJUSTMENT.to_owned(),
            Some(process) => {
                sys::c_string(format!("/proc/{}/oom_score_adj", process.raw()).as_bytes())?
            }
        };
        sys::write_number(&path, OOM_SCORE_ADJUSTMENT)?;
        kernel::set_nice(Some(process.unwrap_or_else(sys::current_process)), NICE)
    }

    fn adopt(&mut self, process: ProcessId) -> Result<Option<ChildStatus>, Errno> {
        sys::try_wait(process)
    }

    fn reap(&mut self) -> Option<(ProcessId, ChildStatus, i32)> {
        sys::reap_any_nonblocking().map(|child| (child.process, child.status, child.wait_status))
    }

    fn report(&mut self, line: &[u8]) {
        write_console(kernel::CONSOLE_DEVICE, line);
    }

    fn shut_down(&mut self, status: u8) {
        kernel::sync();
        sys::power_off(status)
    }
}

/// The existing PID-1 signal and helper owner, shared with the control loop.
pub(crate) struct Runtime {
    system: RealSystem,
    helpers: Helpers,
    terminating: bool,
}

impl Runtime {
    pub(crate) fn new(arguments: &[CString]) -> Result<Self, String> {
        let mut system = RealSystem { signals: None };
        let helpers = prepare(&mut system, arguments)?;
        Ok(Self {
            system,
            helpers,
            terminating: false,
        })
    }

    /// Starts ingress from the applied root's publication and gateway, before its root switch.
    /// The fork keeps the initramfs root and PID 1's priority; the sole collector owns its exit.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "SL7.7b starts the restored announcer before the root switch"
        )
    )]
    pub(crate) fn restore_announcer(
        &mut self,
        publication: Publication,
        gateway: Option<Ipv4Addr>,
    ) -> crate::Result<()> {
        if publication == Publication::Unpublished {
            return Ok(());
        }
        if self.helpers.announcer.is_some() {
            return Err(crate::Error::unsupported(
                "an announcer is already registered",
            ));
        }
        let gateway = gateway
            .ok_or_else(|| crate::Error::unsupported("published listeners require a gateway"))?;
        let program = super::ingress::PROGRAM.to_owned();
        #[cfg(test)]
        let program = tests::restore::announcer_program(program);
        let arguments = [
            c"nvx-ingress".to_owned(),
            CString::new(gateway.to_string()).expect("an IPv4 address has no NUL"),
        ];
        let fork = sys::fork().map_err(|error| {
            crate::Error::io("cannot fork the restored announcer")
                .with_source(io::Error::from(error))
        })?;
        if let sys::ForkResult::Parent(process) = fork {
            self.helpers.announcer = Some(process);
            return Ok(());
        }
        let error = sys::exec(&program, &arguments, None);
        write_console(
            kernel::CONSOLE_DEVICE,
            format!("NVX-INIT-ERROR: cannot exec /sbin/nvx-ingress: {error}\n").as_bytes(),
        );
        sys::exit_immediately(FAILURE);
    }

    /// Poll alongside control input so a blocked peer cannot hide a helper's end.
    pub(crate) fn wake(&self) -> Pollable<'_> {
        Pollable::Signals(self.system.signals.as_ref().expect("listening"))
    }

    /// Hands terminal statuses to the active execution; a helper failure wins over shutdown.
    pub(crate) fn service(
        &mut self,
        mut observed: impl FnMut(u32, ProcessStatus),
    ) -> Result<bool, String> {
        collect(&mut self.system, self.helpers, &mut observed)?;
        let signals = self.system.signals.as_ref().expect("listening");
        while let Some(event) = signals.read() {
            self.terminating |= event.signal == Some(Signal::SIGTERM);
        }
        collect(&mut self.system, self.helpers, observed)?;
        Ok(self.terminating)
    }
}

/// Runs `nvx-init` with the process's arguments. As PID 1, it serves until the VM must end, and
/// then ends it, so it does not return; elsewhere it fails with status 2.
#[doc(hidden)]
pub fn main() -> ExitCode {
    sys::restore_sigpipe();
    if sys::current_process().raw() != INIT_PROCESS {
        sys::write_standard_error(b"nvx-init: runs only as PID 1\n");
        return ExitStatus::Usage.into();
    }
    let arguments = sys::args();
    match Runtime::new(arguments.get(1..).unwrap_or_default()) {
        Ok(mut runtime) => {
            let result = serve_layered(&mut runtime);
            finish(&mut runtime.system, result);
        }
        Err(failure) => finish(&mut RealSystem { signals: None }, Err(failure)),
    }
    ExitStatus::Failure.into()
}

fn serve_layered(runtime: &mut Runtime) -> Result<(), String> {
    let command_line = std::fs::read("/proc/cmdline")
        .map_err(|error| format!("cannot read the boot command line: {error}"))?;
    let boot = boot::cold_boot(&command_line).map_err(|error| error.to_string())?;
    runtime.system.report(
        format!(
            "NVX-INIT-ROOT: switched mappings={}\n",
            boot.table.expected()
        )
        .as_bytes(),
    );
    super::managed_agent::AgentConfig::layered(&boot.command_line, boot.user, boot.home)
        .map_err(|error| format!("invalid layered identity: {}", error.desc()))?
        .serve_layered(
            runtime,
            boot.control,
            (boot.table, boot.mapper, boot.export),
        )
}

/// Keeps diagnostics, sync and direct power-off with the original PID-1 owner.
fn finish(system: &mut impl System, result: Result<(), String>) {
    let status = match result {
        Ok(()) => 0,
        Err(failure) => {
            system.report(format!("NVX-INIT-ERROR: {failure}\n").as_bytes());
            FAILURE
        }
    };
    system.shut_down(status);
}

/// Takes over the existing helpers before the idle or control loop begins.
fn prepare(system: &mut impl System, arguments: &[CString]) -> Result<Helpers, String> {
    system
        .listen()
        .map_err(|error| format!("cannot read signals: {}", error.desc()))?;
    system
        .raise_priority(None)
        .map_err(|error| format!("cannot raise the priority of PID 1: {}", error.desc()))?;
    let helpers = Helpers::parse(arguments).ok_or("usage: nvx-init TIME_DAEMON [ANNOUNCER]")?;
    for (name, process) in helpers.named() {
        let pid = process.raw();
        match system.adopt(process) {
            Ok(None) => {}
            Ok(Some(status)) => return Err(ended(name, process, status)),
            Err(error) => {
                return Err(format!(
                    "{name}, process {pid}, is not a child of PID 1: {}",
                    error.desc()
                ));
            }
        }
        system.raise_priority(Some(process)).map_err(|error| {
            format!(
                "cannot raise the priority of {name}, process {pid}: {}",
                error.desc()
            )
        })?;
    }
    Ok(helpers)
}

/// The one waitpid loop, which keeps helper failure policy and forwards raw terminal statuses.
fn collect(
    system: &mut impl System,
    helpers: Helpers,
    mut observed: impl FnMut(u32, ProcessStatus),
) -> Result<(), String> {
    while let Some((process, status, raw)) = system.reap() {
        if let Some(name) = helpers.name(process) {
            return Err(ended(name, process, status));
        }
        observed(process.raw().unsigned_abs(), ProcessStatus::from_raw(raw));
    }
    Ok(())
}

/// Describes the end of the helper `name`, process `process`, with `status`.
fn ended(name: &str, process: ProcessId, status: ChildStatus) -> String {
    let how = match status {
        ChildStatus::Exited(code) => format!("exited with status {code}"),
        ChildStatus::Signaled(signal) => format!("was killed by signal {signal}"),
        ChildStatus::Other => "ended".to_owned(),
    };
    format!("{name}, process {}, {how}", process.raw())
}

/// Writes `line` to the console device `console`, which it opens for the line without making it
/// the process's controlling terminal, or to standard error if it cannot.
fn write_console(console: &CStr, line: &[u8]) {
    let flags = OpenFlags::O_WRONLY | OpenFlags::O_NOCTTY | OpenFlags::O_CLOEXEC;
    let written =
        sys::open(console, flags, Mode::empty()).and_then(|console| console.write_all(line));
    if written.is_err() {
        sys::write_standard_error(line);
    }
}

#[cfg(test)]
mod tests;
