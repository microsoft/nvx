//! The credit and stdio engine of an execution, which every adapter shares, whatever its framing.
//!
//! An execution runs a workload on one side, the [`Runner`], for another side, the [`Peer`],
//! which sends its standard input and takes its standard output and error. The engine owns what
//! both sides must agree on: the state of each stream, the credit arithmetic that bounds the
//! bytes in flight, the order of each stream's data and end, and the execution's outcome. The
//! adapter owns everything else: its framing and transport, which messages it puts in one frame,
//! its admission of executions, and the processes, which a guest's adapter drives through
//! `guest::exec::binding` (Linux, feature `guest`).
//!
//! The engine needs only the standard library: no feature enables it, and it has no wire format,
//! async runtime, threads, global state, admission, or reaper. Each side is a state machine that
//! never blocks and never waits: the adapter hands it each input as it arrives, with the current
//! [`Instant`] where time matters, retrieves what it must do, one bounded item at a time, and
//! [ticks](Runner::tick) it once its [deadline](Runner::deadline) passes. So a stream, a sink, a
//! window, or a transport that blocks holds back only the bytes that wait on it, never a timer, a
//! cancellation, a reset, or another execution, and tests drive both sides deterministically.
//!
//! # Messages
//!
//! The two sides exchange these messages, each of which the adapter frames and tags with the
//! execution's [`Key`]:
//!
//! | From | Message | Meaning |
//! | --- | --- | --- |
//! | Peer | [credit](peer::Message::Credit) for output | the peer's consumer freed output units |
//! | Peer | [input](peer::Message::Input) | bytes of the workload's standard input |
//! | Peer | [end of input](peer::Message::InputEnd) | the input ends after its last byte |
//! | Peer | [cancel](peer::Message::Cancel) | end the workload |
//! | Runner | [credit](runner::Message::Credit) for input | the workload took input units |
//! | Runner | [started](runner::Message::Started) | the workload's program runs |
//! | Runner | [output](runner::Message::Output) | bytes of standard output or error |
//! | Runner | [end](runner::Message::End) | the stream ends after its last byte |
//! | Runner | [input closed](runner::Message::InputClosed) | the workload's input closed early |
//! | Runner | [terminal](runner::Message::Terminal) | the execution's [`Outcome`] |
//!
//! A message for an earlier generation of the execution's key is [stale](Receipt::Stale), and one
//! that the receiving side no longer needs, such as a credit after the execution ended, or any
//! message of the runner's once the peer detached from it or gave up waiting for it, is
//! [late](Receipt::Late): neither changes anything. A message that breaks the rules, such as data
//! beyond the window, is a [`Violation`], which changes nothing either, and after which the
//! adapter decides what to do, such as reset its session.
//!
//! # Credit
//!
//! Each data message carries at most [`Limits::chunk`] bytes, and costs credit in its pool: as
//! many units as its bytes, or one unit whatever its length, as the pool's [`Window`] counts. The
//! adapter chooses the pools: the input's, and either one window for each output stream, or one
//! pool that standard output and error [share](Limits::shared_output). Each receiver grants its
//! whole window first, then each unit again once its consumer took the unit's bytes: the runner
//! once the workload's input took them, the peer once its sink took them. A receiver refuses data
//! beyond what it granted, and a sender refuses a grant that would leave it more than the window,
//! so at most a window of units is ever in flight, and each side's buffers stay bounded, with no
//! limit on a stream's total length: see [`Runner::memory_bound`] and [`Peer::memory_bound`].
//!
//! A sender queues at most one chunk of each stream ahead of the credit, which the runner reads
//! from the workload as its [read allowance](Runner::read_allowance) lets it, and the peer takes
//! from its source as its [write allowance](Peer::write_allowance) does. Data leaves the queue as
//! the adapter retrieves it, as one message of whatever the queue holds, up to a chunk: an
//! adapter whose transport is busy retrieves less often, and so sends fewer, fuller messages.
//!
//! # Completion
//!
//! - The workload's input ends only after every byte that the runner accepted: the runner
//!   requests to [close it](runner::Request::CloseInput) once the peer ended it and the workload
//!   took the rest. A workload that closes its input early, or ends, makes the runner tell the
//!   peer that the input [closed](runner::Message::InputClosed), and drop what the peer sends
//!   after, so that the peer's source never waits. Closing the input never closes the other
//!   direction.
//! - Each stream keeps its order, and its end follows its last byte. A stream's end goes ahead
//!   of any later output of the other stream, which never holds it back; otherwise, the streams'
//!   messages have no order between them. The runner sends the terminal outcome after both
//!   streams' ends. An end does not report whether the runner lost bytes; only the terminal
//!   outcome does. A peer that detaches or gives up without that outcome conservatively marks
//!   both streams as truncated, even if their ends arrived.
//! - A cancellation, a timeout, a drop, a reset, or a disconnection proceeds whatever the streams
//!   do. The runner requests to [end](runner::Request::Terminate) the workload, and pending output
//!   still flows for [`Timers`]'s grace, then the rest is discarded, and the terminal outcome
//!   marks each stream that lost bytes as [truncated](Outcome::is_truncated). A reset, a drop, or
//!   a disconnection of the runner leaves no peer to deliver to, so the runner discards at once.
//!   A workload that ends on its own, with no such cause, loses no byte, however slowly the peer
//!   takes them, if its execution settles, or if no process holds its streams any more, as the
//!   adapter tells the runner once they [hung up](Runner::output_hung_up).
//! - The logical outcome, the delivery of the output, and the settlement of the execution are
//!   separate. Once the workload's status is known, the runner requests to
//!   [settle](runner::Request::Settle) its processes, whose result the adapter reports once its
//!   own check shows that the workload's descendants, and every process that holds one of its
//!   pipes, are gone; it may stop checking once the runner no longer
//!   [awaits](Runner::awaits_settlement) the result. The workload's exit, a signal to its process
//!   group, or a peer's acknowledgement never counts. The terminal outcome follows the status,
//!   the delivery, and the settlement, and says whether the execution is
//!   [reusable](Outcome::is_reusable) or fenced: a failed or late settlement fences it.
//! - Each side bounds every wait: a termination gives the workload its grace and then
//!   [`Timers`]'s settlement time to end, close its streams, and settle, after which the runner
//!   gives up on an execution whose workload has not, and fences it, while one whose cleanup is
//!   complete waits only for the transport to take its last messages; a workload that ended on
//!   its own, but whose execution did not settle, leaves its streams the settlement time and then
//!   the grace after its end to close or hang up, after which the runner gives up on those still
//!   open that a process may hold, sends what it read of them, and marks them truncated; a launch
//!   result that has not arrived by then fences the execution, whether or not it settled, as a
//!   process may hold what reports it too, and the runner gives up on the result, and on those
//!   streams, and sends the workload's output and outcome without it; a peer that ends the
//!   execution itself waits as long for the terminal outcome, after which it gives up and fences
//!   it too; and a peer that receives a terminal outcome with a cause, without having ended the
//!   execution itself, gives its sink the grace from then on to take the output that it holds,
//!   and discards the rest.
//!
//! # Example
//!
//! A peer and a runner, connected directly, as an adapter's framing and transport would connect
//! them:
//!
//! ```
//! use std::time::{Duration, Instant};
//!
//! use nvx::exec::{
//!     peer, runner, Key, Limits, Peer, Runner, Settlement, Status, Stream, Timers, Window,
//! };
//!
//! let now = Instant::now();
//! let key = Key::new(1, 0);
//! let window = Window::bytes(64 * 1024);
//! let limits = Limits::new(16 * 1024, window, window).unwrap();
//! let timers = Timers::new(Duration::from_secs(1), Duration::from_secs(5));
//! let mut peer = Peer::new(key, limits, timers, now);
//! let mut runner = Runner::new(key, limits, timers, now);
//! let mut buffer = vec![0u8; limits.chunk()];
//!
//! // The workload runs, writes to its standard output, and exits; its processes settle.
//! runner.launched(Ok(()));
//! assert_eq!(runner.output(Stream::Stdout, b"hello"), 5);
//! runner.output_closed(Stream::Stdout);
//! runner.output_closed(Stream::Stderr);
//! runner.exited(now, Some(Status::Exited(0)));
//! assert_eq!(runner.next_request(), Some(runner::Request::Settle));
//! runner.settled(Settlement::Settled);
//!
//! // The adapter carries each side's messages to the other until the peer finishes.
//! let mut stdout = Vec::new();
//! let outcome = loop {
//!     while let Some(message) = peer.next_message(&mut buffer) {
//!         match message {
//!             peer::Message::Credit { pool, units } => {
//!                 runner.credit(key, pool, units).unwrap();
//!             }
//!             message => unreachable!("{message:?}"),
//!         }
//!     }
//!     while let Some(message) = runner.next_message(&mut buffer) {
//!         match message {
//!             runner::Message::Credit { units } => {
//!                 peer.credit(key, units).unwrap();
//!             }
//!             runner::Message::Started => {
//!                 peer.started(key).unwrap();
//!             }
//!             runner::Message::Output { stream, length } => {
//!                 peer.output(key, stream, &buffer[..length]).unwrap();
//!             }
//!             runner::Message::End(stream) => {
//!                 peer.end_output(key, stream).unwrap();
//!             }
//!             runner::Message::Terminal(outcome) => {
//!                 peer.terminal(now, key, outcome).unwrap();
//!             }
//!             runner::Message::InputClosed => {
//!                 peer.input_closed(key).unwrap();
//!             }
//!             message => unreachable!("{message:?}"),
//!         }
//!     }
//!     // The peer's sink takes what arrived.
//!     let pending = peer.pending_output(Stream::Stdout).to_vec();
//!     stdout.extend_from_slice(&pending);
//!     peer.consumed(Stream::Stdout, pending.len());
//!     let last = std::iter::from_fn(|| peer.next_event()).last();
//!     if let Some(peer::Event::Finished(outcome)) = last {
//!         break outcome;
//!     }
//! };
//! assert_eq!(stdout, b"hello");
//! assert_eq!(outcome.status(), Some(Status::Exited(0)));
//! assert!(outcome.is_reusable() && !outcome.is_truncated(Stream::Stdout));
//! ```

use std::fmt;
use std::time::{Duration, Instant};

mod credit;
pub mod peer;
pub mod runner;

#[cfg(test)]
mod tests;

pub use peer::Peer;
pub use runner::Runner;

/// The identity of one execution: an identifier that the adapter assigns, such as a request ID or
/// a stream slot, and the generation of that identifier, which tells a later execution with the
/// same identifier, or the same execution in a later session, from an earlier one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    /// The execution's identifier.
    id: u64,
    /// The identifier's generation.
    generation: u64,
}

impl Key {
    /// Returns the key of execution `id` in `generation`.
    pub const fn new(id: u64, generation: u64) -> Self {
        Self { id, generation }
    }

    /// Returns the execution's identifier.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Returns the identifier's generation.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Checks the key of a message for this execution: `Ok(true)` for its own generation,
    /// `Ok(false)` for an earlier one, whose messages are stale, and [`Violation::Key`] for
    /// another identifier or a later generation.
    fn admits(self, message: Key) -> Result<bool, Violation> {
        if message.id != self.id {
            return Err(Violation::Key);
        }
        match message.generation.cmp(&self.generation) {
            std::cmp::Ordering::Less => Ok(false),
            std::cmp::Ordering::Equal => Ok(true),
            std::cmp::Ordering::Greater => Err(Violation::Key),
        }
    }
}

/// One of the workload's output streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stream {
    /// The workload's standard output.
    Stdout,
    /// The workload's standard error.
    Stderr,
}

impl Stream {
    /// Both streams, in the order in which the sides first serve them.
    pub const ALL: [Self; 2] = [Self::Stdout, Self::Stderr];

    /// Returns the stream's position in [`ALL`](Self::ALL).
    const fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}

/// A pool of credit, which a [credit message](peer::Message::Credit) names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pool {
    /// The workload's standard input.
    Input,
    /// The workload's standard output, which has a pool of its own unless the output streams
    /// [share](Limits::shared_output) one.
    Stdout,
    /// The workload's standard error, which has a pool of its own unless the output streams share
    /// one.
    Stderr,
    /// The one pool that standard output and error [share](Limits::shared_output).
    Output,
}

/// What a pool's credit counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unit {
    /// Data bytes: a data message costs as many units as its bytes.
    Bytes,
    /// Data messages: a data message costs one unit, whatever its length up to a chunk.
    Frames,
}

/// The size of a pool's window: the most units that may be in flight at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Window {
    /// What the window counts.
    unit: Unit,
    /// The units of the window.
    size: u32,
}

impl Window {
    /// Returns a window of `size` bytes.
    pub const fn bytes(size: u32) -> Self {
        Self {
            unit: Unit::Bytes,
            size,
        }
    }

    /// Returns a window of `size` data messages.
    pub const fn frames(size: u32) -> Self {
        Self {
            unit: Unit::Frames,
            size,
        }
    }

    /// Returns what the window counts.
    pub const fn unit(&self) -> Unit {
        self.unit
    }

    /// Returns the units of the window.
    pub const fn size(&self) -> u32 {
        self.size
    }

    /// Returns the units that a data message of `length` bytes costs.
    fn cost(&self, length: usize) -> u32 {
        match self.unit {
            Unit::Bytes => credit::units(length),
            Unit::Frames => 1,
        }
    }

    /// Returns the most data bytes that the window holds with chunks of `chunk` bytes.
    fn bytes_with(&self, chunk: usize) -> usize {
        match self.unit {
            Unit::Bytes => self.size as usize,
            Unit::Frames => (self.size as usize).saturating_mul(chunk),
        }
    }
}

/// The limits on which both sides of an execution agree: the maximum chunk, the input's window,
/// and the output's windows, one for each stream unless the streams share one pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Limits {
    /// The most data bytes that one data message carries.
    chunk: u32,
    /// The window of the workload's standard input.
    input: Window,
    /// The window of each output pool.
    output: Window,
    /// Whether standard output and error share one pool.
    shared: bool,
}

impl Limits {
    /// Returns limits with data messages of at most `chunk` bytes, the input's `input` window,
    /// and one `output` window for each output stream, or `None` if the chunk or a window is
    /// empty.
    pub const fn new(chunk: u32, input: Window, output: Window) -> Option<Self> {
        if chunk == 0 || input.size == 0 || output.size == 0 {
            return None;
        }
        Some(Self {
            chunk,
            input,
            output,
            shared: false,
        })
    }

    /// Makes standard output and error share one pool with the output window, which both
    /// streams' data messages charge.
    pub const fn shared_output(mut self) -> Self {
        self.shared = true;
        self
    }

    /// Returns the most data bytes that one data message carries.
    pub const fn chunk(&self) -> usize {
        self.chunk as usize
    }

    /// Returns the window of the workload's standard input.
    pub const fn input(&self) -> Window {
        self.input
    }

    /// Returns the window of each output pool.
    pub const fn output(&self) -> Window {
        self.output
    }

    /// Returns whether standard output and error share one pool.
    pub const fn is_shared(&self) -> bool {
        self.shared
    }

    /// Returns the output pools, in the order of their positions.
    fn output_pools(&self) -> &'static [Pool] {
        if self.shared {
            &[Pool::Output]
        } else {
            &[Pool::Stdout, Pool::Stderr]
        }
    }

    /// Returns the position of `stream`'s output pool.
    fn pool_of(&self, stream: Stream) -> usize {
        if self.shared { 0 } else { stream.index() }
    }

    /// Returns the position of the output pool `pool`, or [`Violation::Pool`] if the limits have
    /// no such output pool.
    fn output_pool(&self, pool: Pool) -> Result<usize, Violation> {
        match (pool, self.shared) {
            (Pool::Output, true) => Ok(0),
            (Pool::Stdout, false) => Ok(0),
            (Pool::Stderr, false) => Ok(1),
            _ => Err(Violation::Pool),
        }
    }

    /// Returns the checked length of a data message of `length` bytes: [`Violation::Chunk`] if
    /// it is empty or longer than a chunk.
    fn check_chunk(&self, length: usize) -> Result<(), Violation> {
        if length == 0 || length > self.chunk() {
            return Err(Violation::Chunk);
        }
        Ok(())
    }

    /// Returns the most data bytes that the input's window holds.
    fn input_bytes(&self) -> usize {
        self.input.bytes_with(self.chunk())
    }

    /// Returns the most data bytes that the output's windows hold together.
    fn output_bytes(&self) -> usize {
        let pools = if self.shared { 1 } else { 2 };
        self.output.bytes_with(self.chunk()).saturating_mul(pools)
    }
}

/// The local timers of one side of an execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Timers {
    /// How long the execution may run before the side ends it, if it ever does.
    timeout: Option<Duration>,
    /// How long pending output still flows once a termination begins, and how long the output
    /// streams of an execution that did not settle may stay open after the settlement time.
    grace: Duration,
    /// How long the execution may then take to settle.
    settlement: Duration,
}

impl Timers {
    /// Returns timers with no timeout, a `grace` for pending output once a termination begins,
    /// and a `settlement` time after it, or after the workload's end, within which the runner
    /// expects the execution to settle, and the peer expects the terminal outcome of an execution
    /// that it ended. The output streams of a workload that ended on its own, but whose
    /// execution did not settle, may stay open for the grace after the settlement time.
    pub const fn new(grace: Duration, settlement: Duration) -> Self {
        Self {
            timeout: None,
            grace,
            settlement,
        }
    }

    /// Ends the execution for a [timeout](Cause::TimedOut) once `timeout` has passed since the
    /// side was created: the runner, unless the workload's end reaches it before a
    /// [tick](Runner::tick) fires the timeout, and the peer, if the execution has not finished.
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Returns the timeout, if any.
    pub const fn timeout_duration(&self) -> Option<Duration> {
        self.timeout
    }

    /// Returns the grace of pending output.
    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// Returns the settlement time.
    pub const fn settlement(&self) -> Duration {
        self.settlement
    }
}

/// Why a side ended an execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cause {
    /// The peer, or its caller, cancelled the execution.
    Cancelled,
    /// The execution's owner dropped it: the peer's caller dropped its handle, or the runner's
    /// adapter abandoned the execution.
    Dropped,
    /// The execution's timeout passed.
    TimedOut,
    /// The session between the sides was reset.
    Reset,
    /// The transport between the sides was lost.
    Disconnected,
}

impl fmt::Display for Cause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "cancelled",
            Self::Dropped => "dropped",
            Self::TimedOut => "timed out",
            Self::Reset => "reset",
            Self::Disconnected => "disconnected",
        })
    }
}

/// The status with which a workload's program ended, as its collector reaped it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    /// The program exited with this code.
    Exited(i32),
    /// A signal of this number ended the program.
    Signaled(i32),
}

/// Why a workload's program did not start: the adapter's number of the launch stage that failed,
/// and the operating system's error number of the failure.
///
/// NVX's guest binding, `guest::exec::binding` (Linux, feature `guest`), numbers the stages as
/// `guest::spawn::LaunchStage::code` does, which `LaunchStage::from_code` reverses, and reports 0
/// for a launch whose report it could not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LaunchFailure {
    /// The adapter's number of the stage that failed.
    stage: u32,
    /// The error number of the failure.
    error: i32,
}

impl LaunchFailure {
    /// Returns the failure of the stage that the adapter numbers `stage`, with the error number
    /// `error`.
    pub const fn new(stage: u32, error: i32) -> Self {
        Self { stage, error }
    }

    /// Returns the adapter's number of the stage that failed.
    pub const fn stage(&self) -> u32 {
        self.stage
    }

    /// Returns the error number of the failure.
    pub const fn error(&self) -> i32 {
        self.error
    }
}

/// The result of settling an execution's processes, which the runner's adapter reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Settlement {
    /// No process of the execution is left: the workload, its descendants, and every process
    /// that held one of its pipes are gone.
    Settled,
    /// The adapter could not settle the execution, such as because a kill failed.
    Failed,
}

/// How an execution ended: the runner's terminal outcome, or the peer's own if none arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Outcome {
    /// The workload's status, if a collector reaped it.
    status: Option<Status>,
    /// Why a side ended the workload, if one did.
    cause: Option<Cause>,
    /// Why the workload's program did not start, if it did not.
    launch_failure: Option<LaunchFailure>,
    /// Whether each stream lost bytes, or the peer could not confirm that it ended whole.
    truncated: [bool; 2],
    /// Whether the execution settled, so that its resources may be reused.
    reusable: bool,
}

impl Outcome {
    /// Returns the outcome of a workload that ended with `status`, if known, with no cause, no
    /// launch failure, no truncation, and fenced: the form in which a peer's adapter decodes a
    /// terminal record, before it applies the record's other fields.
    pub const fn new(status: Option<Status>) -> Self {
        Self {
            status,
            cause: None,
            launch_failure: None,
            truncated: [false, false],
            reusable: false,
        }
    }

    /// Returns the outcome with `cause` as why a side ended the workload.
    pub const fn with_cause(mut self, cause: Cause) -> Self {
        self.cause = Some(cause);
        self
    }

    /// Returns the outcome with `failure` as why the workload's program did not start.
    pub const fn with_launch_failure(mut self, failure: LaunchFailure) -> Self {
        self.launch_failure = Some(failure);
        self
    }

    /// Returns the outcome with `stream` marked as truncated.
    pub const fn with_truncated(mut self, stream: Stream) -> Self {
        self.truncated[stream.index()] = true;
        self
    }

    /// Returns the outcome of an execution that settled, and so is reusable.
    pub const fn with_reusable(mut self) -> Self {
        self.reusable = true;
        self
    }

    /// Returns the workload's status, or `None` if no collector reaped it, as when its collector
    /// lost it or a side gave up on the execution.
    pub const fn status(&self) -> Option<Status> {
        self.status
    }

    /// Returns why a side ended the workload, or `None` if it ended on its own. A workload with a
    /// cause may still have exited on its own before the side's end reached it, which its status
    /// tells.
    pub const fn cause(&self) -> Option<Cause> {
        self.cause
    }

    /// Returns why the workload's program did not start, or `None` if it started, or if no
    /// launch result arrived.
    pub const fn launch_failure(&self) -> Option<LaunchFailure> {
        self.launch_failure
    }

    /// Returns whether `stream` lost bytes, or the peer could not confirm that it ended whole.
    /// A peer that finishes without the runner's terminal outcome conservatively marks both
    /// streams as truncated, even if their ends arrived.
    pub const fn is_truncated(&self, stream: Stream) -> bool {
        self.truncated[stream.index()]
    }

    /// Returns whether the execution settled, so that its resources may be reused, or `false` if
    /// it is fenced.
    pub const fn is_reusable(&self) -> bool {
        self.reusable
    }
}

/// What the side did with a message that it accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Receipt {
    /// The message changed the side's state.
    Applied,
    /// The message belongs to an earlier generation of the execution's key, and changed nothing.
    Stale,
    /// The side no longer needed the message, as for a credit after the execution ended, input
    /// after the workload's input closed, or any message of the runner's once the peer detached
    /// from it or gave up waiting for it: it changed nothing beyond the window it charged.
    Late,
}

/// A message that breaks the rules, which the side refuses without changing anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Violation {
    /// The message's key names another execution, or a later generation of this one.
    Key,
    /// A credit names a pool that the execution's limits do not have.
    Pool,
    /// A credit grants no units, or more units than are in flight, which would leave its sender
    /// more than the window.
    Credit,
    /// A data message is empty, or longer than a chunk.
    Chunk,
    /// A data message exceeds the credit that its receiver granted.
    Window,
    /// Data or an end follows the stream's end, or a second terminal outcome follows the first.
    Ended,
    /// A message comes out of order, such as a second start, or a terminal outcome before both
    /// streams' ends.
    Order,
}

impl fmt::Display for Violation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Key => "the message names another execution or a later generation",
            Self::Pool => "the credit names a pool that the execution does not have",
            Self::Credit => "the credit grants no units or more units than are in flight",
            Self::Chunk => "the data message is empty or longer than a chunk",
            Self::Window => "the data message exceeds the granted credit",
            Self::Ended => "the message follows the end of its stream or of the execution",
            Self::Order => "the message comes out of order",
        })
    }
}

impl std::error::Error for Violation {}

/// Returns the instant `duration` after `now`, or `None` if no instant that late exists.
fn after(now: Instant, duration: Duration) -> Option<Instant> {
    now.checked_add(duration)
}

/// Returns the earlier of two optional instants.
fn earliest(first: Option<Instant>, second: Option<Instant>) -> Option<Instant> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (first, second) => first.or(second),
    }
}
