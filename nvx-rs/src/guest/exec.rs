//! Driving a program as one execution, for any guest init or agent.
//!
//! An [`Execution`] drives a program that the caller [spawned](super::spawn) with the program's
//! ends of its [`Pipes`], or that the caller started itself and [adopts](Execution::adopt): it
//! tells the caller how the program's launch went, reads what the program writes to its standard
//! output and error, writes what the caller gives it to the program's standard input, and ends
//! once the program has ended and its output is all taken. It ends the program when the caller
//! cancels it, when its timeout passes, or when the caller loses the peer for which it runs it,
//! and it reports each step as an [`Event`].
//!
//! It needs neither NVX's control protocol nor an async runtime, and it keeps no policy: the
//! caller decides which programs run and how many at once, where their output goes, how much of
//! it to take, and what each outcome means. NVX's managed agent is one caller, which runs one
//! execution at a time and forwards its output and outcome as NVXC frames. A caller whose peer
//! runs the [execution engine](crate::exec)'s other side drives the execution through a
//! [`Binding`](binding::Binding) instead, which turns the engine's credit, input, endings, and
//! settlement into the execution's calls, and its events into the engine's inputs.
//!
//! # Driving
//!
//! The caller drives each execution without blocking, from its own loop or reactor. After it
//! acts on an execution, and whenever one of the execution's
//! [descriptors](Execution::descriptors) polls readable or its [deadline](Execution::deadline)
//! passes, it calls [`next_event`](Execution::next_event) until that returns `None`, which it
//! does only once every descriptor would block. Then it waits until one of them polls readable,
//! the deadline passes, or the caller has something to do, such as input to
//! [write](Execution::write_input) once the [input](Execution::input) polls writable. A reactor
//! that tracks readiness by edges may clear every descriptor's readiness when `next_event`
//! returns `None`. Executions share a loop without waiting for each other: the one whose output
//! the caller cannot take stalls alone.
//!
//! # Flow control
//!
//! The execution reads an output stream only as far as the caller allows with
//! [`grant`](Execution::grant), its flow-control seam: without an allowance, it leaves the stream
//! unread, so the program blocks once its pipe fills, and nothing else waits. Each output event
//! uses up as much of the stream's allowance, which the caller grants again as its sink takes
//! the bytes; a caller whose peer grants credit grants what the peer credits. The execution reads
//! into a buffer that the caller lends and holds no input, so its memory stays bounded. A stream
//! that the caller [discards](Execution::discard) is read into the same buffer and dropped
//! instead, so the buffer bounds every read.
//!
//! # Adoption
//!
//! A caller that starts a program by means of its own, such as a `clone3` into namespaces and a
//! cgroup that [`spawn`](super::spawn) does not make, [adopts](Execution::adopt) it: it hands
//! the execution the [`Child`] that names the program, and its own ends of the program's pipes,
//! which [`Pipes::from_ends`] takes. The caller saw how the program's launch went, so the
//! execution waits on no launch report: it reports the launch at once, with no failure, and then
//! drives the program as one that the caller spawned.
//!
//! # Status and ending
//!
//! By default, the execution collects the program's status itself, by a pidfd, and never another
//! child's, as [`process::Child`](super::process::Child) does. A caller that collects the status
//! of every child itself, such as a PID 1, hands the program's status to the execution with
//! [`observe`](Execution::observe): before it next calls into the execution, from the thread
//! that drives it, or whenever its collector takes the status, once [`Options::caller_reaps`]
//! has made that collector the program's only one.
//!
//! The execution ends the program with `SIGKILL`: the program alone, or the process group that
//! it leads too, as long as no collector has taken the program's status, through the program's
//! pidfd, which names them whatever later takes their IDs; and the processes of a cgroup that the
//! caller names, which also reaches descendants that left the group. Once the program has ended,
//! as its status tells, or as a collector of the caller's that took the status first does, the
//! execution closes the program's input, so that a descendant that still reads it sees its end,
//! and kills that cgroup again if the program's output is still open, as only another process
//! can hold it then. Ending the program ends neither stream: an execution ends once the caller
//! has taken or discarded what remains in them, and normal completion loses no byte.
//!
//! A caller that stops the program gracefully, as a container runtime does, first
//! [signals](Execution::signal) it, such as with `SIGTERM`, which reaches the program alone, or
//! its process group too, as the execution's kill does, but not the processes of the cgroup, and
//! each process once, unless the program leaves its group meanwhile. The signal ends neither the
//! program nor the execution: the execution records no cause, keeps the program's input open, and
//! leaves its timeout in place. The caller then drives the execution until a deadline of its own,
//! such as the end of a grace period: a program that handles the signal and exits ends with its
//! own status, and an outcome without a [cause](Outcome::cause), and loses no output. A program
//! whose status is still unknown by then, as one that ignores the signal, the caller ends with
//! [`terminate`](Execution::terminate) or [`cancel`](Execution::cancel), whose cause the outcome
//! reports.
//!
//! ```no_run
//! use nvx::guest::exec::{Event, Execution, Options, Pipes, Stream};
//! use nvx::guest::spawn::Command;
//!
//! # fn main() -> std::io::Result<()> {
//! let (pipes, ends) = Pipes::new(false)?;
//! let spawned = Command::new(c"/bin/echo".to_owned(), vec![c"echo".to_owned(), c"hi".to_owned()])
//!     .stdout(ends.stdout)
//!     .stderr(ends.stderr)
//!     .new_process_group()
//!     .spawn()?;
//! let mut execution = Execution::new(spawned, pipes, Options::new().process_group())?;
//! execution.grant(Stream::Stdout, 64 * 1024);
//! execution.grant(Stream::Stderr, 64 * 1024);
//! let mut buffer = [0u8; 4096];
//! loop {
//!     match execution.next_event(&mut buffer)? {
//!         Some(Event::Output(stream, bytes)) => {
//!             // The caller's sink took the bytes, so it allows as many again.
//!             let length = bytes.len();
//!             execution.grant(stream, length);
//!         }
//!         Some(Event::Ended(outcome)) => {
//!             assert!(outcome.status().success());
//!             break;
//!         }
//!         Some(_) => {}
//!         // Poll `execution.descriptors()` until one is readable or `execution.deadline()`.
//!         None => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use super::cgroup::{ExecutionCgroup, Scope};
use super::process::Child;
use super::spawn::{LaunchFailure, LaunchReport, Spawned};
use super::sys::{self, Errno, FileDescriptor, ProcessId};

pub mod binding;

/// One of a program's output streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stream {
    /// The program's standard output.
    Stdout,
    /// The program's standard error.
    Stderr,
}

impl Stream {
    /// Both streams, in the order in which an execution first reads them.
    const ALL: [Self; 2] = [Self::Stdout, Self::Stderr];

    /// Returns the stream's position in [`ALL`](Self::ALL).
    fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }

    /// Returns the other stream.
    fn other(self) -> Self {
        match self {
            Self::Stdout => Self::Stderr,
            Self::Stderr => Self::Stdout,
        }
    }
}

/// The pipes between an execution and its program.
///
/// [`new`](Self::new) creates them, and hands the program's ends to the caller, whose
/// [`Command`](super::spawn::Command) gives them to the program as its standard streams. The
/// execution's ends stay here, nonblocking, until [`Execution::new`] takes them. For a program
/// that the caller started itself, [`from_ends`](Self::from_ends) takes the caller's own ends of
/// the program's pipes instead, for [`Execution::adopt`].
#[derive(Debug)]
pub struct Pipes {
    /// The writer of the program's standard input, if the execution writes it.
    input: Option<FileDescriptor>,
    /// The readers of the program's standard output and error.
    outputs: [FileDescriptor; 2],
}

/// The program's ends of its execution's [`Pipes`], which its command takes as its standard
/// streams. They block, as a program expects of its standard streams, and the spawned child
/// keeps them only at its standard streams' numbers.
#[derive(Debug)]
pub struct ProgramEnds {
    /// The reader of the program's standard input, if its execution writes it: the command's
    /// [`stdin`](super::spawn::Command::stdin). Without it, the caller gives the program a
    /// standard input of its choice, such as the null device.
    pub stdin: Option<OwnedFd>,
    /// The writer of the program's standard output: the command's
    /// [`stdout`](super::spawn::Command::stdout).
    pub stdout: OwnedFd,
    /// The writer of the program's standard error: the command's
    /// [`stderr`](super::spawn::Command::stderr).
    pub stderr: OwnedFd,
}

impl Pipes {
    /// Creates a pipe for the program's standard output, one for its standard error, and, with
    /// `input`, one for its standard input, and returns the execution's ends and the program's.
    ///
    /// Every end closes on `exec`, except the ones that a spawned child places at its standard
    /// streams, and the execution's ends are nonblocking: a pipe end of its own has an open file
    /// description that no other descriptor shares, so the program's ends keep blocking.
    pub fn new(input: bool) -> io::Result<(Self, ProgramEnds)> {
        let (stdout_reader, stdout_writer) = sys::create_pipe()?;
        let (stderr_reader, stderr_writer) = sys::create_pipe()?;
        stdout_reader.set_nonblocking()?;
        stderr_reader.set_nonblocking()?;
        let (input, stdin) = if input {
            let (reader, writer) = sys::create_pipe()?;
            writer.set_nonblocking()?;
            (Some(writer), Some(reader.into()))
        } else {
            (None, None)
        };
        Ok((
            Self {
                input,
                outputs: [stdout_reader, stderr_reader],
            },
            ProgramEnds {
                stdin,
                stdout: stdout_writer.into(),
                stderr: stderr_writer.into(),
            },
        ))
    }

    /// Takes the caller's own ends of the pipes of a program that it started itself, for
    /// [`Execution::adopt`]: `input`, the writer of the program's standard input if the execution
    /// writes it, and `stdout` and `stderr`, the readers of the program's standard output and
    /// error.
    ///
    /// Each must be an end of a pipe whose open file description the program does not share, as
    /// one end of a new pipe is, since `from_ends` makes each nonblocking, and the program's own
    /// ends must keep blocking. It makes each close-on-exec too. Each must also be an end of a
    /// pipe that carries bytes rather than packets, which `O_DIRECT` makes its writer write, as no
    /// check can tell from the reader: a read that takes part of a packet drops the rest. An end
    /// of another kind of file, or one open for the other direction, is `EINVAL`, and an error
    /// closes every end.
    pub fn from_ends(input: Option<OwnedFd>, stdout: OwnedFd, stderr: OwnedFd) -> io::Result<Self> {
        let take = |descriptor: OwnedFd, writes: bool| -> io::Result<FileDescriptor> {
            let descriptor = FileDescriptor::from_owned(descriptor);
            descriptor.take_pipe_end(writes)?;
            Ok(descriptor)
        };
        let input = input.map(|writer| take(writer, true)).transpose()?;
        let stdout = take(stdout, false)?;
        let stderr = take(stderr, false)?;
        Ok(Self {
            input,
            outputs: [stdout, stderr],
        })
    }
}

/// How an execution ends its program, and when, and who collects its status.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Whether the program leads a process group of its own.
    process_group: bool,
    /// The cgroup whose processes ending the program kills too, and the scope that it kills.
    cgroup: Option<(ExecutionCgroup, Scope)>,
    /// How long the program may run.
    timeout: Option<Duration>,
    /// Whether the caller's own collector takes the program's status, which the execution then
    /// never waits for.
    caller_reaps: bool,
}

impl Options {
    /// Returns options that end the program alone, give it no timeout, and leave its status for
    /// the execution to collect.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ends the program's whole process group, whose leader the program is, as
    /// [`Command::new_process_group`](super::spawn::Command::new_process_group) makes it, not
    /// the program alone, and sends the group the program's [signals](Execution::signal) too. The
    /// execution signals the group through the program's pidfd, which names it whatever later
    /// takes its ID. Before Linux 6.9, which cannot signal a group through a pidfd, it signals the
    /// group by its ID instead, right after the program's pidfd showed the program unreaped.
    pub fn process_group(mut self) -> Self {
        self.process_group = true;
        self
    }

    /// Kills every process in `scope` of `cgroup` too, with `cgroup.kill`, when the execution
    /// ends the program, and once the program has ended while its output is still open. The
    /// handle holds the cgroup's directory, so that those kills reach this cgroup alone, even
    /// once another cgroup takes its path. [`Scope::Workload`] reaches every execution of the
    /// workload, which a caller that runs several at once rarely wants. The program's
    /// [signals](Execution::signal) do not reach the cgroup's processes, as `cgroup.kill` sends
    /// only `SIGKILL`.
    pub fn cgroup(mut self, cgroup: &ExecutionCgroup, scope: Scope) -> Self {
        self.cgroup = Some((cgroup.clone(), scope));
        self
    }

    /// Ends the program once `timeout` has passed since [`Execution::new`] or
    /// [`Execution::adopt`], unless a collector has taken the program's status by then, or another
    /// cause ended the program first.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Leaves the program's status to the caller's own collector, such as the `waitpid(-1)`
    /// loop of a PID 1 on a thread of its own, which hands it to the execution with
    /// [`Execution::observe`]. The execution then never waits for the program, nor lists its
    /// pidfd among its descriptors, so that the program has one status collector: the caller
    /// learns that the program has ended from its own, as from `SIGCHLD`. The collector leaves
    /// the program's status alone until [`Execution::new`] has returned, which opens the
    /// program's pidfd by its ID, as [`Execution::adopt`] does for a child without one.
    pub fn caller_reaps(mut self) -> Self {
        self.caller_reaps = true;
        self
    }
}

/// What an execution waits on, as one of its [descriptors](Execution::descriptors).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Source {
    /// The program's launch report, which arrives once its program runs or its launch fails. An
    /// [adopted](Execution::adopt) program has none.
    Launch,
    /// An output stream, which has bytes, or which every writer closed.
    Output(Stream),
    /// The program's pidfd, which polls readable once the program has ended, while the
    /// execution collects the program's status itself.
    Exit,
}

/// Why an execution ended its program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cause {
    /// The caller [cancelled](Execution::cancel) the execution.
    Cancelled,
    /// The caller [terminated](Execution::terminate) the execution for a reason of its own.
    Terminated,
    /// The program's [timeout](Options::timeout) passed.
    TimedOut,
    /// The caller [disconnected](Execution::disconnect) the execution.
    Disconnected,
}

/// The processes that a kill reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KillTarget {
    /// The program alone.
    Program,
    /// The program's process group.
    ProcessGroup,
    /// The processes of the execution's [cgroup](Options::cgroup).
    Cgroup,
}

impl KillTarget {
    /// Every target, in the order in which the execution reports their failures.
    const ALL: [Self; 3] = [Self::Program, Self::ProcessGroup, Self::Cgroup];

    /// Returns the target's position in [`ALL`](Self::ALL).
    fn index(self) -> usize {
        match self {
            Self::Program => 0,
            Self::ProcessGroup => 1,
            Self::Cgroup => 2,
        }
    }
}

/// How an execution ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The program's status.
    status: ExitStatus,
    /// Why the execution ended the program, if it did.
    cause: Option<Cause>,
    /// The bytes of each stream that the execution discarded.
    discarded: [u64; 2],
}

impl Outcome {
    /// Returns the program's status: an exit, or the signal that ended it.
    pub fn status(&self) -> ExitStatus {
        self.status
    }

    /// Returns why the execution ended the program, or `None` if the program ended on its own.
    /// Even with a cause, the program may have exited by itself, before the execution's kill
    /// reached it or after a [failed kill](Event::KillFailed), which its status tells.
    pub fn cause(&self) -> Option<Cause> {
        self.cause
    }

    /// Returns how many bytes of `stream` the execution [discarded](Execution::discard): none,
    /// unless the caller discarded or disconnected the stream.
    pub fn discarded(&self, stream: Stream) -> u64 {
        self.discarded[stream.index()]
    }
}

/// One step of an execution, which [`Execution::next_event`] reports.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event<'a> {
    /// The program's launch report, once, as [`LaunchReport::wait`] returns it: no failure, which
    /// means that the program runs, unless its child ended before it could, as when the
    /// execution ended it first; the launch stage that failed, after which the child exits with
    /// status 125 without running the program; or an error, if the report could not be read. An
    /// [adopted](Execution::adopt) program, whose launch its caller saw, reports no failure, at
    /// once.
    Launch(io::Result<Option<LaunchFailure>>),
    /// Bytes that the program, or another process that shares its stream, wrote to the stream,
    /// in order. They use up as much of the stream's allowance.
    Output(Stream, &'a [u8]),
    /// Bytes of the stream that the execution read and dropped.
    Discarded(Stream, usize),
    /// Every writer of the stream closed it, after its last byte.
    Closed(Stream),
    /// Reading the stream failed, which ends it as if every writer closed it.
    ReadFailed(Stream, io::Error),
    /// A kill that ending the program needed failed. A target that is gone, such as a process
    /// group whose processes have all ended, is no failure. The cause for which the execution
    /// ended the program counts all the same, and the execution goes on.
    KillFailed(KillTarget, io::Error),
    /// The execution ended: the program's status is known, its launch was reported, and its
    /// output streams are closed. It is the last event, and the execution has closed the
    /// program's input.
    Ended(Outcome),
}

/// One of a program's output streams, as its execution reads it.
#[derive(Debug)]
struct Output {
    /// The stream's reader, until the stream closes.
    reader: Option<FileDescriptor>,
    /// How many more bytes the caller lets the execution read, or `usize::MAX` for any number.
    allowance: usize,
    /// Whether the execution reads and drops the stream's bytes.
    discarding: bool,
    /// How many bytes the execution has dropped.
    discarded: u64,
}

/// What one read of an output stream did.
enum Read {
    /// The stream had nothing to read now, or the execution may not read it.
    Idle,
    /// The execution read this many bytes into the caller's buffer.
    Data(usize),
    /// The execution read this many bytes into the caller's buffer, and dropped them.
    Discarded(usize),
    /// Every writer closed the stream.
    Closed,
    /// The read failed, which closed the stream.
    Failed(Errno),
}

impl Output {
    /// Starts reading the stream from `reader`, with no allowance.
    fn new(reader: FileDescriptor) -> Self {
        Self {
            reader: Some(reader),
            allowance: 0,
            discarding: false,
            discarded: 0,
        }
    }

    /// Returns whether the stream is open.
    fn is_open(&self) -> bool {
        self.reader.is_some()
    }

    /// Returns whether the execution reads the stream now: it is open, and the execution may
    /// read or drop its bytes.
    fn is_wanted(&self) -> bool {
        self.is_open() && (self.discarding || self.allowance > 0)
    }

    /// Reads the stream once into `buffer`: as far as the allowance lets it, or, while the
    /// execution discards the stream, as far as the buffer goes, dropping what it read.
    fn read(&mut self, buffer: &mut [u8]) -> Read {
        if !self.is_open() {
            return Read::Idle;
        }
        let length = if self.discarding {
            buffer.len()
        } else {
            self.allowance.min(buffer.len())
        };
        if length == 0 {
            return Read::Idle;
        }
        match self.read_into(&mut buffer[..length]) {
            Read::Data(length) if self.discarding => {
                self.discarded += length as u64;
                Read::Discarded(length)
            }
            Read::Data(length) => {
                if self.allowance != usize::MAX {
                    self.allowance -= length;
                }
                Read::Data(length)
            }
            other => other,
        }
    }

    /// Reads the open stream once into `buffer`, which is not empty, retrying interruptions.
    fn read_into(&mut self, buffer: &mut [u8]) -> Read {
        let Some(reader) = &self.reader else {
            return Read::Idle;
        };
        loop {
            match reader.read(buffer) {
                Ok(0) => {
                    self.reader = None;
                    return Read::Closed;
                }
                Ok(length) => return Read::Data(length),
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => return Read::Idle,
                Err(error) => {
                    self.reader = None;
                    return Read::Failed(error);
                }
            }
        }
    }
}

/// What an execution reports of its program's launch.
#[derive(Debug)]
enum Launch {
    /// The launch report of a program that the caller spawned, until it arrives.
    Report(LaunchReport),
    /// No failure, for an adopted program, whose launch its caller saw.
    Adopted,
}

/// One program that runs as an execution: see the [module](self) documentation.
///
/// Dropping an execution that has not ended ends its program, as
/// [`disconnect`](Self::disconnect) does, without waiting; its status is still the caller's to
/// collect, with its own collector or a [`Child`].
#[derive(Debug)]
pub struct Execution {
    /// The program, which the execution names by a pidfd, and whose status it keeps.
    child: Child,
    /// The program's launch, until the execution reports it.
    launch: Option<Launch>,
    /// The program's standard output and error.
    outputs: [Output; 2],
    /// The writer of the program's standard input, until it closes.
    input: Option<FileDescriptor>,
    /// The stream that the execution reads first in its next step.
    turn: Stream,
    /// Whether the program leads its own process group, which ending the program kills whole, and
    /// which the program's signals reach.
    process_group: bool,
    /// The cgroup whose processes ending the program kills too, and the scope that it kills.
    cgroup: Option<(ExecutionCgroup, Scope)>,
    /// When the program's timeout passes.
    deadline: Option<Instant>,
    /// Whether the caller's own collector takes the program's status.
    caller_reaps: bool,
    /// Whether a collector of the caller's took the program's status before handing it over.
    reaped: bool,
    /// Why the execution ended the program, once it did.
    cause: Option<Cause>,
    /// Whether the execution has learned that the program has ended.
    exited: bool,
    /// The latest kill failure of each target, which the next events report.
    failures: [Option<io::Error>; 3],
    /// Whether the execution has ended.
    ended: bool,
}

impl Execution {
    /// Starts driving the program that `spawned` names, which the caller spawned with the
    /// program's ends of `pipes`, as `options` say. Nothing collects the program's status before
    /// `new` returns: `new` opens a pidfd for the program by its ID, which names the program alone
    /// only until then, and the execution names the program by that pidfd from then on.
    ///
    /// If opening the pidfd fails, `new` ends what the options name before it returns the error:
    /// the program, and then the process group that it led if the options say so, by their IDs,
    /// unless `ESRCH` tells that the program's status is gone already, as its ID may name another
    /// process then; and the processes of the options' cgroup. A program that joined another
    /// process group ends too. It closes the pipes and the launch report before that kill, so
    /// that a failure for want of descriptors leaves it one. The program's status is still the
    /// caller's to collect, by `spawned`'s ID.
    pub fn new(spawned: Spawned, pipes: Pipes, options: Options) -> io::Result<Self> {
        let process = spawned.process();
        let report = spawned.into_report();
        let child = match Child::with_pidfd(process.raw().unsigned_abs()) {
            Ok(child) => child,
            Err(error) => {
                let unreaped = error.raw_os_error() != Some(Errno::ESRCH as i32);
                abandon(process, unreaped, (report, pipes), &options);
                return Err(error);
            }
        };
        Ok(Self::start(child, Launch::Report(report), pipes, options))
    }

    /// Starts driving the program that `child` names, which the caller started itself, through
    /// `pipes`, which hold the caller's own ends of the program's pipes, as `options` say. The
    /// caller saw how the program's launch went, so the execution waits on no launch report:
    /// [`next_event`](Self::next_event) reports the launch at once, with no failure, and the
    /// caller calls it right after `adopt` returns, as no descriptor polls readable for it.
    ///
    /// The execution names the program by the child's pidfd. A child without one, which
    /// [`Child::new`] names by its ID, gets one that `adopt` opens by that ID, as
    /// [`new`](Self::new) does, so nothing may collect the program's status before `adopt`
    /// returns then. A child whose status a collector of the caller's took already, and handed
    /// over with [`Child::observe`], needs none: the program has ended, so the execution closes
    /// its input, kills the processes of the options' cgroup if its output is still open, and
    /// reads what remains of that output.
    ///
    /// If opening the pidfd fails, `adopt` ends what the options name before it returns the
    /// error, as [`new`](Self::new) does, and closes the pipes before that kill. The program's
    /// status is still the caller's to collect.
    ///
    /// ```no_run
    /// use std::os::unix::process::CommandExt;
    /// use std::process::{Command, Stdio};
    ///
    /// use nvx::guest::exec::{Event, Execution, Options, Pipes};
    /// use nvx::guest::process::Child;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let (stdout, stdout_writer) = std::io::pipe()?;
    /// let (stderr, stderr_writer) = std::io::pipe()?;
    /// let program = Command::new("/bin/echo")
    ///     .arg("hi")
    ///     .stdin(Stdio::null())
    ///     .stdout(stdout_writer)
    ///     .stderr(stderr_writer)
    ///     .process_group(0)
    ///     .spawn()?;
    /// // Nothing collects the program's status before the child holds its pidfd.
    /// let child = Child::with_pidfd(program.id())?;
    /// let pipes = Pipes::from_ends(None, stdout.into(), stderr.into())?;
    /// let mut execution = Execution::adopt(child, pipes, Options::new().process_group())?;
    /// let mut buffer = [0u8; 4096];
    /// assert!(matches!(execution.next_event(&mut buffer)?, Some(Event::Launch(Ok(None)))));
    /// // Drive it on as one that the caller spawned.
    /// # Ok(())
    /// # }
    /// ```
    pub fn adopt(mut child: Child, pipes: Pipes, options: Options) -> io::Result<Self> {
        if let Err(error) = child.open_pidfd() {
            let unreaped = error.raw_os_error() != Some(Errno::ESRCH as i32);
            abandon(child.process(), unreaped, pipes, &options);
            return Err(error);
        }
        let mut execution = Self::start(child, Launch::Adopted, pipes, options);
        if execution.child.status().is_some() {
            execution.exited();
        }
        Ok(execution)
    }

    /// Starts driving the program that `child` names, whose launch is `launch`, through `pipes`,
    /// as `options` say.
    fn start(child: Child, launch: Launch, pipes: Pipes, options: Options) -> Self {
        let [stdout, stderr] = pipes.outputs;
        Self {
            child,
            launch: Some(launch),
            outputs: [Output::new(stdout), Output::new(stderr)],
            input: pipes.input,
            turn: Stream::Stdout,
            process_group: options.process_group,
            cgroup: options.cgroup,
            deadline: options
                .timeout
                .and_then(|timeout| Instant::now().checked_add(timeout)),
            caller_reaps: options.caller_reaps,
            reaped: false,
            cause: None,
            exited: false,
            failures: [None, None, None],
            ended: false,
        }
    }

    /// Returns the program's process ID.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Returns the program's status, once the execution or the caller's collector collected it.
    pub fn status(&self) -> Option<ExitStatus> {
        self.child.status()
    }

    /// Returns whether either output stream is still open, for a caller that, as NVX's managed
    /// agent does, drives the execution until the program's status is known and its output
    /// closed, and then takes its last events.
    pub(crate) fn has_open_output(&self) -> bool {
        self.outputs.iter().any(Output::is_open)
    }

    /// Returns whether every writer of `stream` closed it, without reading it: the stream ends
    /// once the execution reads what its pipe still holds. A stream that closed hung up too.
    pub(crate) fn output_hung_up(&self, stream: Stream) -> bool {
        match &self.outputs[stream.index()].reader {
            Some(reader) => sys::hung_up(reader.borrowed()).unwrap_or(false),
            None => true,
        }
    }

    /// Lets the execution read `bytes` more of `stream`: its flow-control seam. It reads a
    /// stream only while the stream has an allowance, and each output event uses up as much of
    /// it. An allowance that reaches `usize::MAX` never runs out.
    pub fn grant(&mut self, stream: Stream, bytes: usize) {
        let output = &mut self.outputs[stream.index()];
        output.allowance = output.allowance.saturating_add(bytes);
    }

    /// Reads and drops the rest of `stream`, whatever its allowance, from the next event on. It
    /// reads into the buffer that the caller lends [`next_event`](Self::next_event), as output
    /// does, reports each read as [`Event::Discarded`], and counts the bytes in its [`Outcome`].
    pub fn discard(&mut self, stream: Stream) {
        self.outputs[stream.index()].discarding = true;
    }

    /// Writes as much of `bytes` to the program's standard input as its pipe takes now, with
    /// one write, and returns how many it took. A full pipe is `WouldBlock`: the caller writes
    /// again once [`input`](Self::input) polls writable.
    ///
    /// Once the input is closed, by [`close_input`](Self::close_input), by the end of the
    /// program or the execution, or because the program closed its end, a write is `EPIPE`. The
    /// write never raises `SIGPIPE`, whatever the process's disposition of it.
    pub fn write_input(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(input) = &self.input else {
            return Err(Errno::EPIPE.into());
        };
        loop {
            match sys::write_without_sigpipe(input, bytes) {
                Ok(length) => return Ok(length),
                Err(Errno::EINTR) => {}
                Err(Errno::EPIPE) => {
                    self.input = None;
                    return Err(Errno::EPIPE.into());
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Closes the program's standard input, which the program reads to its end after every byte
    /// that a write took.
    pub fn close_input(&mut self) {
        self.input = None;
    }

    /// Lends the writer of the program's standard input while it is open, which a caller with
    /// input to write polls for writing. It polls with an error once the program has closed its
    /// end.
    pub fn input(&self) -> Option<BorrowedFd<'_>> {
        self.input.as_ref().map(FileDescriptor::borrowed)
    }

    /// Ends the program for a cancellation, and closes its input. Returns whether the
    /// cancellation is why the execution ended the program, as it is unless a collector had
    /// taken the program's status, or the execution had ended the program, already.
    pub fn cancel(&mut self) -> bool {
        self.end(Cause::Cancelled)
    }

    /// Ends the program for a reason of the caller's own, such as a limit on its output, and
    /// closes its input. Returns whether that is why the execution ended the program, as for
    /// [`cancel`](Self::cancel).
    pub fn terminate(&mut self) -> bool {
        self.end(Cause::Terminated)
    }

    /// Ends the program because the caller lost the peer for which it ran it: the execution
    /// discards both output streams from now on, closes the input, and ends the program while it
    /// is unreaped, and the processes of its cgroup.
    pub fn disconnect(&mut self) {
        for output in &mut self.outputs {
            output.discarding = true;
        }
        self.end(Cause::Disconnected);
    }

    /// Sends the program the signal numbered `signal`, such as `SIGTERM`, and returns whether the
    /// program was unreaped. The signal reaches what the execution's kill reaches but the
    /// processes of its [cgroup](Options::cgroup): the program alone, or its
    /// [process group](Options::process_group) too, through the program's pidfd, or, before Linux
    /// 6.9, the group by its ID, right after the pidfd showed the program unreaped.
    ///
    /// Each process receives the signal once, so that no handler runs twice for one call: a
    /// program that leads its group receives the group's signal alone, once a wait through its
    /// pidfd, which needs no permission to signal it, has shown it unreaped, and one that left its
    /// group receives a signal of its own. A program that leaves its group while it is signalled
    /// receives a signal of its own too, so that it never misses one, though the group's signal
    /// may have reached it first.
    ///
    /// The signal ends neither the program nor the execution: the execution records no cause,
    /// keeps the program's input open, and leaves its timeout in place, so a program that ends
    /// after the signal ends with its own status, and its outcome reports no
    /// [cause](Outcome::cause), as for a program that ended by itself. `SIGKILL` is `EINVAL`, and
    /// sends nothing, as the execution ends its program with a cause, through
    /// [`cancel`](Self::cancel) and [`terminate`](Self::terminate), so that its outcome tells why.
    ///
    /// Once a collector has taken the program's status, it sends nothing, and returns `false`, so
    /// that it never reaches a process that took the program's ID, or its group's, since. Finding
    /// the status taken changes nothing in the execution, which learns of the program's end as
    /// ever: from its own collection of the status, or from the caller's
    /// [`observe`](Self::observe). A signal that the kernel refuses to the program or to its
    /// process group, such as one that the caller may not send, or a number that names no signal,
    /// is an error, which no event reports. The group's signal fails only if the kernel refuses it
    /// to every process of the group, as `kill` does. A lookup of the program's group that fails
    /// other than because the program is gone is an error too: a first one sends nothing, and one
    /// after the group's signal sends the program no signal of its own.
    pub fn signal(&mut self, signal: i32) -> io::Result<bool> {
        if signal == sys::Signal::SIGKILL as i32 {
            return Err(Errno::EINVAL.into());
        }
        if !self.uncollected() {
            return Ok(false);
        }
        let Some(pidfd) = self.child.pidfd() else {
            return Ok(false);
        };
        // The program's group is looked up by the program's ID before the pidfd shows the program
        // unreaped, which proves that the ID still named the program then.
        let process = self.child.process();
        if self.process_group && leads_its_group(process)? {
            if !sys::is_unreaped(pidfd)? {
                return Ok(false);
            }
            self.signal_group(pidfd, signal)?;
            // The program may have left its group since the lookup, before the group's signal.
            if !leads_its_group(process)? {
                match signal_through_pidfd(pidfd, signal, false) {
                    Ok(()) | Err(Errno::ESRCH) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            return Ok(true);
        }
        let program = match signal_through_pidfd(pidfd, signal, false) {
            Ok(()) => Ok(()),
            Err(Errno::ESRCH) => return Ok(false),
            Err(error) => Err(error),
        };
        let group = if self.process_group {
            self.signal_group(pidfd, signal)
        } else {
            Ok(())
        };
        program.and(group).map(|()| true).map_err(io::Error::from)
    }

    /// Hands the execution the status that the caller's own collector collected for the process
    /// `id`, and returns whether it was the program's final status, as
    /// [`Child::observe`](super::process::Child::observe) does. Once it has the program's
    /// status, the execution closes the program's input, and kills the processes of its cgroup
    /// if the program's output is still open.
    pub fn observe(&mut self, id: u32, status: ExitStatus) -> bool {
        let observed = self.child.observe(id, status);
        if observed {
            self.exited();
        }
        observed
    }

    /// Waits until the program ends, collects its status, and returns it, for a caller that
    /// leaves the status to the execution, and must stop at once once it has ended the program.
    /// A collector of the caller's that took the status first makes it fail with `ECHILD`, as
    /// [`next_event`](Self::next_event) does, after which the execution leaves the status to that
    /// collector, and treats the program as ended.
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        match self.child.wait() {
            Ok(status) => {
                self.exited();
                Ok(status)
            }
            Err(error) => {
                if error.raw_os_error() == Some(Errno::ECHILD as i32) {
                    self.taken();
                }
                Err(error)
            }
        }
    }

    /// Returns the descriptors that move the execution on, each with what it waits on, which the
    /// caller polls for reading: any event on one, such as a hangup, means that the next event
    /// may follow. They change as the execution proceeds, and none is left once it has ended.
    ///
    /// An output stream is among them only while the execution may read it, so a caller grants
    /// an allowance before it waits on the stream.
    pub fn descriptors(&self) -> impl Iterator<Item = (Source, BorrowedFd<'_>)> {
        let active = !self.ended;
        let launch = match &self.launch {
            Some(Launch::Report(report)) if active => Some((Source::Launch, report.descriptor())),
            _ => None,
        };
        let outputs = Stream::ALL.map(|stream| {
            let output = &self.outputs[stream.index()];
            output
                .reader
                .as_ref()
                .filter(|_| active && output.is_wanted())
                .map(|reader| (Source::Output(stream), reader.borrowed()))
        });
        let exit = self
            .child
            .pidfd()
            .filter(|_| active && self.collects())
            .map(|pidfd| (Source::Exit, pidfd));
        launch
            .into_iter()
            .chain(outputs.into_iter().flatten())
            .chain(exit)
    }

    /// Returns when the program's timeout passes, while it can still end the program: until a
    /// collector takes the program's status, or another cause ends the program.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
            .filter(|_| !self.ended && self.cause.is_none() && self.uncollected())
    }

    /// Takes the next step of the execution without blocking, and returns its event, or `None`
    /// if no step can be taken until a descriptor polls readable, the deadline passes, or the
    /// caller acts on the execution. Every read of an output stream goes into `buffer`, which
    /// bounds its event, whether the execution returns the bytes or discards them; an empty
    /// buffer reads nothing.
    ///
    /// Within a step, the execution reports a failed kill first, then the launch report, then
    /// collects the program's status, unless the caller reaps the program, ends it if its
    /// timeout has passed, and reads the output streams in turn, one read at a time, so that
    /// neither starves the other. It ends once nothing is left. After that, it returns `None`.
    ///
    /// It fails only if collecting the program's status fails. A collector of the caller's that
    /// took the status first makes it fail with `ECHILD`, once: the execution then leaves the
    /// status to that collector, as [`Options::caller_reaps`] does, and waits until the caller
    /// hands it over with [`observe`](Self::observe). The program has ended, though, so the
    /// execution closes its input, and kills the processes of its cgroup if its output is still
    /// open, as once it has the status.
    pub fn next_event<'b>(&mut self, buffer: &'b mut [u8]) -> io::Result<Option<Event<'b>>> {
        if self.ended {
            return Ok(None);
        }
        if let Some(event) = self.kill_failure() {
            return Ok(Some(event));
        }
        if let Some(report) = self.launch() {
            return Ok(Some(Event::Launch(report)));
        }
        if self.collect()?
            && let Some(event) = self.kill_failure()
        {
            return Ok(Some(event));
        }
        if self
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.end(Cause::TimedOut);
            if let Some(event) = self.kill_failure() {
                return Ok(Some(event));
            }
        }
        for _ in Stream::ALL {
            let stream = self.turn;
            self.turn = stream.other();
            match self.outputs[stream.index()].read(&mut buffer[..]) {
                Read::Idle => {}
                Read::Data(length) => return Ok(Some(Event::Output(stream, &buffer[..length]))),
                Read::Discarded(length) => return Ok(Some(Event::Discarded(stream, length))),
                Read::Closed => return Ok(Some(Event::Closed(stream))),
                Read::Failed(error) => return Ok(Some(Event::ReadFailed(stream, error.into()))),
            }
        }
        let Some(status) = self.child.status() else {
            return Ok(None);
        };
        if self.launch.is_some() || self.outputs.iter().any(Output::is_open) {
            return Ok(None);
        }
        self.ended = true;
        self.input = None;
        Ok(Some(Event::Ended(Outcome {
            status,
            cause: self.cause,
            discarded: self.outputs.each_ref().map(|output| output.discarded),
        })))
    }

    /// Takes the oldest kill failure that the execution has yet to report, as its event.
    pub(crate) fn kill_failure(&mut self) -> Option<Event<'static>> {
        KillTarget::ALL.into_iter().find_map(|target| {
            self.failures[target.index()]
                .take()
                .map(|error| Event::KillFailed(target, error))
        })
    }

    /// Takes the program's launch report without blocking once it has arrived, as
    /// [`next_event`](Self::next_event) does within a step, and returns it, or `None` while it is
    /// pending or once it was taken. An adopted program's launch, with no failure, is never
    /// pending.
    pub(crate) fn launch(&mut self) -> Option<io::Result<Option<LaunchFailure>>> {
        let ready = match self.launch.as_ref()? {
            Launch::Report(report) => arrived(report),
            Launch::Adopted => true,
        };
        if !ready {
            return None;
        }
        match self.launch.take().expect("the launch is pending") {
            Launch::Report(report) => Some(report.wait()),
            Launch::Adopted => Some(Ok(None)),
        }
    }

    /// Collects the program's status without blocking, if the execution collects it itself and
    /// the program has ended, as [`next_event`](Self::next_event) does within a step, and returns
    /// whether it collected it now. It fails as `next_event` does if collecting the status fails.
    pub(crate) fn collect(&mut self) -> io::Result<bool> {
        if !self.collects() {
            return Ok(false);
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.exited();
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(error) => {
                if error.raw_os_error() == Some(Errno::ECHILD as i32) {
                    self.taken();
                }
                Err(error)
            }
        }
    }

    /// Gives the options' cgroup back to the caller, whose check found it empty after the
    /// program ended: the execution kills its processes no more, not even when it drops, so
    /// that the caller may reuse the cgroup at once.
    pub(crate) fn release_cgroup(&mut self) {
        self.cgroup = None;
    }

    /// Ends the program for `cause`, unless the execution has ended, and returns whether `cause`
    /// is why it ended the program: the first cause while the program is unreaped.
    fn end(&mut self, cause: Cause) -> bool {
        if self.ended {
            return false;
        }
        self.input = None;
        let first = self.kill() && self.cause.is_none();
        if first {
            self.cause = Some(cause);
        }
        first
    }

    /// Kills what the execution may: the program, and the process group that it leads if the
    /// options say so, while no collector has taken the program's status, and the cgroup's
    /// processes. It records each failure for the next events, and returns whether the program
    /// was unreaped.
    fn kill(&mut self) -> bool {
        let unreaped = self.signal_program();
        if let Some((cgroup, scope)) = &self.cgroup
            && let Err(error) = cgroup.kill(*scope)
        {
            self.failures[KillTarget::Cgroup.index()] = Some(error);
        }
        unreaped
    }

    /// Sends `SIGKILL` to the program through its pidfd, unless a collector is known to have
    /// taken the program's status, and returns whether the program was unreaped.
    ///
    /// The pidfd names the program alone, whatever later takes its ID, so the signal fails with
    /// `ESRCH` once a collector of the caller's has taken the status, which the execution notes.
    /// Then, if the options say so, it [signals the process group](Self::signal_group) that the
    /// program leads. A signal that the kernel refuses otherwise, such as one that it does not
    /// permit, is a failure to kill its target, which the next events report. Only `ESRCH` tells
    /// that the program is gone, so it still counts as unreaped then: the cause for which the
    /// execution ended it stands, and a timeout fires once.
    fn signal_program(&mut self) -> bool {
        if !self.uncollected() {
            return false;
        }
        let Some(pidfd) = self.child.pidfd() else {
            return false;
        };
        let kill = sys::Signal::SIGKILL as i32;
        match signal_through_pidfd(pidfd, kill, false) {
            Ok(()) => {}
            Err(Errno::ESRCH) => {
                self.reaped = true;
                return false;
            }
            Err(error) => self.failures[KillTarget::Program.index()] = Some(error.into()),
        }
        if self.process_group
            && let Err(error) = self.signal_group(pidfd, kill)
        {
            self.failures[KillTarget::ProcessGroup.index()] = Some(error.into());
        }
        true
    }

    /// Sends `signal` to the process group that the program leads, right after the program's
    /// `pidfd` showed the program unreaped: through the pidfd, which names that group even if a
    /// collector takes the program's status meanwhile, or, from a kernel that cannot signal a
    /// group through a pidfd, before Linux 6.9, by the group's ID, which the program, unreaped a
    /// moment before, kept from any other group. A group whose processes have all ended is no
    /// failure.
    fn signal_group(&self, pidfd: BorrowedFd<'_>, signal: i32) -> Result<(), Errno> {
        let signalled = match signal_through_pidfd(pidfd, signal, true) {
            Err(Errno::EINVAL) => sys::signal_process_group(self.child.process(), signal),
            signalled => signalled,
        };
        match signalled {
            Err(Errno::ESRCH) => Ok(()),
            signalled => signalled,
        }
    }

    /// Returns whether the program's status is unknown to the execution, and no collector of
    /// the caller's is known to have taken it.
    fn uncollected(&self) -> bool {
        self.child.status().is_none() && !self.reaped
    }

    /// Returns whether the execution collects the program's status itself, by its pidfd: until
    /// it knows the status, unless the caller reaps the program, or a collector of the caller's
    /// took the status first.
    fn collects(&self) -> bool {
        !self.caller_reaps && self.uncollected()
    }

    /// Notes that a collector of the caller's took the program's status first, as a collection
    /// that fails with `ECHILD` tells, so that the program has ended.
    fn taken(&mut self) {
        self.reaped = true;
        self.exited();
    }

    /// Notes that the program has ended, the first time that the execution learns it: from the
    /// program's status, or because a collector of the caller's took the status first. It closes
    /// the program's input, which only another process can read now, and kills the processes of
    /// the cgroup if the program's output is still open, as only another process can hold it now.
    fn exited(&mut self) {
        if self.exited {
            return;
        }
        self.exited = true;
        self.input = None;
        if self.outputs.iter().any(Output::is_open)
            && let Some((cgroup, scope)) = &self.cgroup
            && let Err(error) = cgroup.kill(*scope)
        {
            self.failures[KillTarget::Cgroup.index()] = Some(error);
        }
    }
}

impl Drop for Execution {
    /// Ends the program of an execution that has not ended, as [`Execution::disconnect`] does.
    fn drop(&mut self) {
        if !self.ended {
            self.kill();
        }
    }
}

/// Ends what the program of an execution that could not start may leave running, while it is
/// `unreaped`: the program, by its ID, and then, if the options say so, the process group that it
/// led when it started, as the execution's own kill does through the pidfd, so that a program
/// that joined another group since ends too; and the processes of the options' cgroup. It closes
/// the execution's `descriptors` before the cgroup's kill, which opens `cgroup.kill`, so that a
/// failure for want of descriptors leaves that kill one.
fn abandon<D>(process: ProcessId, unreaped: bool, descriptors: D, options: &Options) {
    if unreaped {
        let _ = sys::kill_process(process);
        if options.process_group {
            let _ = sys::kill_process_group(process);
        }
    }
    drop(descriptors);
    if let Some((cgroup, scope)) = &options.cgroup {
        let _ = cgroup.kill(*scope);
    }
}

/// Returns whether `report` has arrived: its pipe polls readable or hung up.
fn arrived(report: &LaunchReport) -> bool {
    loop {
        match sys::readable_or_hung_up(&[report.descriptor()], 0) {
            Ok(ready) => return ready[0],
            Err(Errno::EINTR) => {}
            Err(_) => return false,
        }
    }
}

/// Returns whether `process` leads its process group, by a lookup by its ID, unless a test has
/// given the lookup's next answer, as for a program that leaves its group right after it. A
/// process that the lookup finds no more, as `ESRCH` tells, leads none; any other failure of the
/// lookup is an error.
fn leads_its_group(process: ProcessId) -> Result<bool, Errno> {
    #[cfg(test)]
    if let Some(leads) = LEADS.take() {
        return Ok(leads);
    }
    match sys::process_group_of(process) {
        Ok(group) => Ok(group == process),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Sends the signal numbered `signal` through `pidfd`, as [`sys::signal_through_pidfd`] does,
/// unless a test has the kernel refuse this thread's signals through pidfds, or only those to
/// process groups, as a kernel before Linux 6.9 does. A test sees what this thread sent.
fn signal_through_pidfd(
    pidfd: BorrowedFd<'_>,
    signal: i32,
    process_group: bool,
) -> Result<(), Errno> {
    #[cfg(test)]
    SENT.with_borrow_mut(|sent| sent.push((signal, process_group)));
    #[cfg(test)]
    if let Some(error) = REFUSED.get() {
        return Err(error);
    }
    #[cfg(test)]
    if process_group && BEFORE_LINUX_6_9.get() {
        return Err(Errno::EINVAL);
    }
    sys::signal_through_pidfd(pidfd, signal, process_group)
}

#[cfg(test)]
thread_local! {
    /// The error with which a test has the kernel refuse this thread's signals through pidfds.
    static REFUSED: std::cell::Cell<Option<Errno>> = const { std::cell::Cell::new(None) };
    /// Whether a test has the kernel refuse this thread's signals to process groups through
    /// pidfds, as a kernel before Linux 6.9 does.
    static BEFORE_LINUX_6_9: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The signals that this thread sent through pidfds, in order, each with whether it went to
    /// a process group.
    static SENT: std::cell::RefCell<Vec<(i32, bool)>> = const {
        std::cell::RefCell::new(Vec::new())
    };
    /// The answer that a test has the next lookup of whether a program leads its process group
    /// give on this thread.
    static LEADS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests;
