//! The side of an execution that runs its workload: see the [module](super) documentation.

use std::collections::VecDeque;
use std::time::Instant;

use super::credit::{self, Credit, Granted, Inbox};
use super::{
    Cause, Key, LaunchFailure, Limits, Outcome, Pool, Receipt, Settlement, Status, Stream, Timers,
    Violation, after, earliest,
};

/// A message from the runner to the peer, which the adapter frames and sends in order, as
/// [`Runner::next_message`] returns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Message {
    /// Credit of the workload's input: `units` more of the input's window, which the peer may
    /// spend on input. The first one opens the whole window.
    Credit {
        /// The units of the credit.
        units: u32,
    },
    /// The workload's program runs: its launch succeeded.
    Started,
    /// Bytes of `stream`, in order: the first `length` bytes of the buffer that the adapter lent
    /// [`Runner::next_message`].
    Output {
        /// The stream that the bytes belong to.
        stream: Stream,
        /// The number of bytes, at least one and at most a chunk.
        length: usize,
    },
    /// The end of `stream`, after its last byte.
    End(Stream),
    /// The workload's input closed before the peer ended it, because the workload closed it,
    /// ended, or was ended: the peer sends no more input, and the runner drops what arrives.
    InputClosed,
    /// The execution's terminal outcome, after both streams' ends: the runner's last message.
    /// Once the runner is detached from the peer, by a [reset](Runner::reset), a
    /// [disconnection](Runner::disconnect), or a [drop](Runner::dropped), it is the only message
    /// that the runner still returns, and the adapter keeps it rather than send it.
    Terminal(Outcome),
}

/// A request to the adapter, which applies it to the workload, as [`Runner::next_request`]
/// returns them. The runner makes each one once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Request {
    /// End the workload, and its processes, for `cause`, closing its input.
    Terminate(Cause),
    /// Read and drop the rest of `stream`, reporting the bytes with [`Runner::discarded`], until
    /// it closes.
    Discard(Stream),
    /// Close the workload's input: the peer ended it, and the workload took every byte that the
    /// runner accepted.
    CloseInput,
    /// Settle the execution's processes: end what is left of them, and report with
    /// [`Runner::settled`] once the adapter's own check shows that none is left.
    Settle,
}

/// One output stream, as the runner reads and sends it.
#[derive(Debug, Default)]
struct Output {
    /// The bytes read from the workload and not sent yet: at most one chunk.
    queue: VecDeque<u8>,
    /// Whether the workload's stream ended: every writer closed it, or reading it failed.
    closed: bool,
    /// Whether every writer closed the workload's stream, though the adapter has not read it to
    /// its end: it ends once the adapter reads what it still holds.
    hung_up: bool,
    /// Whether the stream's end message was returned.
    ended: bool,
    /// Whether the runner drops the stream's bytes.
    discarding: bool,
    /// Whether the request to discard the stream waits to be returned.
    discard_requested: bool,
    /// Whether the stream lost bytes.
    truncated: bool,
}

impl Output {
    /// Returns whether the stream is done: it closed, and its queue is empty.
    fn is_done(&self) -> bool {
        self.closed && self.queue.is_empty()
    }

    /// Drops the queue, and notes whether that lost bytes.
    fn drop_queue(&mut self) {
        if !self.queue.is_empty() {
            self.truncated = true;
        }
        self.queue = VecDeque::new();
    }
}

/// The workload's standard input, as the runner receives and writes it.
#[derive(Debug)]
struct Input {
    /// The bytes that the peer sent and the workload has not taken.
    inbox: Inbox,
    /// The input's window.
    window: Granted,
    /// Whether the peer ended the input.
    peer_ended: bool,
    /// Whether the workload's input is closed, or the runner requested to close it.
    closed: bool,
    /// Whether the request to close the input waits to be returned.
    close_requested: bool,
    /// Whether the message that the input closed early waits to be returned.
    closed_message: bool,
}

/// Where the settlement of the execution stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Settling {
    /// The runner has not requested it.
    Idle,
    /// The runner requested it, and expects its result by this instant, if any.
    Requested(Option<Instant>),
    /// The execution settled.
    Settled,
    /// The execution is fenced: its settlement failed, or came too late.
    Fenced,
}

/// The side of an execution that runs its workload: see the [module](super) documentation.
///
/// The adapter creates a runner when it accepts the peer's request to run a workload, and hands
/// it the peer's messages and the workload's events. In return, it takes the runner's
/// [requests](Self::next_request), which it applies to the workload, and its
/// [messages](Self::next_message), which it sends the peer, until the
/// [terminal outcome](Message::Terminal). It reads each output stream of the workload only as far
/// as the [read allowance](Self::read_allowance) lets it, and writes the
/// [pending input](Self::pending_input) to the workload as the workload takes it.
#[derive(Debug)]
pub struct Runner {
    /// The execution's key.
    key: Key,
    /// The limits that both sides agreed on.
    limits: Limits,
    /// The runner's timers.
    timers: Timers,
    /// The workload's launch result, once it arrived.
    launch: Option<Result<(), LaunchFailure>>,
    /// Whether the message that the workload started waits to be returned.
    started_message: bool,
    /// The workload's status, once its end is known: `Some(None)` if its status is lost.
    exit: Option<Option<Status>>,
    /// Why the runner ended the workload, if it did.
    cause: Option<Cause>,
    /// The request to end the workload, while it waits to be returned.
    terminate_request: Option<Cause>,
    /// Whether the request to settle the execution waits to be returned.
    settle_request: bool,
    /// When the workload's timeout passes, while it runs.
    timeout_at: Option<Instant>,
    /// When the termination began, once one did.
    terminated_at: Option<Instant>,
    /// When the grace of pending output ends, while the runner waits for it.
    grace_until: Option<Instant>,
    /// When the runner gives up on the execution after a termination, while it waits for it,
    /// unless the execution's cleanup is complete by then.
    give_up_at: Option<Instant>,
    /// When the runner gives up on what a process may still hold after the workload's end: the
    /// launch result, if it has not arrived, and, if the execution does not settle, the output
    /// streams that are still open. It is the settlement time and then the grace after the
    /// workload's end.
    abandon_at: Option<Instant>,
    /// Whether the runner gave up, and fenced the execution.
    gave_up: bool,
    /// Whether the runner gave up on the launch result after the workload's end, and fenced the
    /// execution.
    launch_abandoned: bool,
    /// Whether the runner is detached from the peer, to which it sends nothing more.
    detached: bool,
    /// The output streams.
    outputs: [Output; 2],
    /// The credit of the output pools: one per stream, or the first one shared.
    credits: [Credit; 2],
    /// The stream whose output the runner sends first next.
    turn: usize,
    /// Whether the last message that the runner returned was credit of the input, so that any
    /// other message that waits goes next: credit and the other messages take turns.
    credited: bool,
    /// The workload's standard input.
    input: Input,
    /// Where the settlement stands.
    settling: Settling,
    /// Whether the terminal outcome was returned.
    finished: bool,
    /// The most bytes that the runner held at once.
    high_water: usize,
}

impl Runner {
    /// Returns the runner of the execution `key`, with the `limits` that both sides agreed on
    /// and its own `timers`, created at `now`, from which its timeout runs.
    pub fn new(key: Key, limits: Limits, timers: Timers, now: Instant) -> Self {
        Self {
            key,
            limits,
            timers,
            launch: None,
            started_message: false,
            exit: None,
            cause: None,
            terminate_request: None,
            settle_request: false,
            timeout_at: timers.timeout.and_then(|timeout| after(now, timeout)),
            terminated_at: None,
            grace_until: None,
            give_up_at: None,
            abandon_at: None,
            gave_up: false,
            launch_abandoned: false,
            detached: false,
            outputs: Default::default(),
            credits: [Credit::new(limits.output), Credit::new(limits.output)],
            turn: 0,
            credited: false,
            input: Input {
                inbox: Inbox::new(limits.input.unit()),
                window: Granted::new(limits.input),
                peer_ended: false,
                closed: false,
                close_requested: false,
                closed_message: false,
            },
            settling: Settling::Idle,
            finished: false,
            high_water: 0,
        }
    }

    /// Returns the execution's key.
    pub fn key(&self) -> Key {
        self.key
    }

    /// Returns the limits that both sides agreed on.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Hands the runner the peer's credit of `units` for the output pool `pool`, which the
    /// execution's limits must have: [`Pool::Output`] if the streams share one pool, otherwise
    /// [`Pool::Stdout`] or [`Pool::Stderr`]. A grant of no units, or of more than the units in
    /// flight, is a [`Violation::Credit`].
    pub fn credit(&mut self, key: Key, pool: Pool, units: u32) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        let index = self.limits.output_pool(pool)?;
        if self.finished {
            return Ok(Receipt::Late);
        }
        self.credits[index].grant(units)?;
        Ok(Receipt::Applied)
    }

    /// Hands the runner bytes of the workload's input from the peer, which the input's window
    /// must allow, and which the runner holds until the workload takes them. Input after the
    /// peer ended it is a [`Violation::Ended`]. Once the workload's input closed, the runner
    /// drops what arrives, as [`Receipt::Late`].
    pub fn input(&mut self, key: Key, bytes: &[u8]) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        if self.finished {
            return Ok(Receipt::Late);
        }
        if self.input.peer_ended {
            return Err(Violation::Ended);
        }
        self.limits.check_chunk(bytes.len())?;
        self.input.window.charge(bytes.len())?;
        if self.input.closed {
            return Ok(Receipt::Late);
        }
        self.input.inbox.push(bytes);
        self.note_buffered();
        Ok(Receipt::Applied)
    }

    /// Hands the runner the end of the workload's input from the peer. The runner requests to
    /// [close the input](Request::CloseInput) once the workload took every byte before it. A
    /// second end is a [`Violation::Ended`].
    pub fn end_input(&mut self, key: Key) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        if self.finished {
            return Ok(Receipt::Late);
        }
        if self.input.peer_ended {
            return Err(Violation::Ended);
        }
        self.input.peer_ended = true;
        if self.input.closed {
            return Ok(Receipt::Late);
        }
        self.close_input_when_taken();
        Ok(Receipt::Applied)
    }

    /// Hands the runner the peer's cancellation at `now`, with the peer's `cause`, such as
    /// [`Cause::Cancelled`], or [`Cause::TimedOut`] for a timeout of the peer's own. It ends the
    /// workload for that cause if the workload still runs, and begins the grace of its pending
    /// output. A cancellation once a termination began, or once the execution ended, is
    /// [`Receipt::Late`].
    pub fn cancel(&mut self, now: Instant, key: Key, cause: Cause) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        self.tick(now);
        if self.finished || self.terminated_at.is_some() {
            return Ok(Receipt::Late);
        }
        self.terminate(now, cause);
        Ok(Receipt::Applied)
    }

    /// Hands the runner the workload's launch result: `Ok` once its program runs, after which
    /// the runner tells the peer that it [started](Message::Started), or why it did not start.
    /// The workload's end, with [`exited`](Self::exited), may come before or after it: an adapter
    /// learns of the launch late if another process holds what reports it. The runner sends the
    /// workload's start, output, and terminal outcome only once it has the result, or gave up on
    /// it: after a termination, or once the settlement time and the grace have passed since the
    /// workload's end. Only the first result counts, and none once the runner gave up on it, as
    /// it may have sent output, which no start may follow, or the terminal outcome.
    pub fn launched(&mut self, result: Result<(), LaunchFailure>) {
        if !self.awaits_launch() {
            return;
        }
        self.launch = Some(result);
        if result.is_ok() {
            self.started_message = !self.detached;
        } else {
            self.close_input();
        }
    }

    /// Returns how many more bytes of `stream` the adapter may read from the workload and hand
    /// over with [`output`](Self::output): what the runner's queue of the stream has room for, at
    /// most one chunk ahead of the peer's credit, and none once the stream closed or the runner
    /// discards it.
    pub fn read_allowance(&self, stream: Stream) -> usize {
        let output = &self.outputs[stream.index()];
        if output.closed || output.discarding || self.gave_up {
            return 0;
        }
        self.limits.chunk() - output.queue.len()
    }

    /// Hands the runner bytes that the adapter read from `stream`, in order, and returns how
    /// many it took: all of them, unless they exceed the [read allowance](Self::read_allowance),
    /// beyond which it takes none. Once the runner discards the stream, it takes and drops them,
    /// which truncates the stream.
    pub fn output(&mut self, stream: Stream, bytes: &[u8]) -> usize {
        let chunk = self.limits.chunk();
        let output = &mut self.outputs[stream.index()];
        if output.closed || output.gave_up {
            return 0;
        }
        if output.discarding {
            output.truncated |= !bytes.is_empty();
            return bytes.len();
        }
        let length = bytes.len().min(chunk - output.queue.len());
        output.queue.extend(&bytes[..length]);
        self.note_buffered();
        length
    }

    /// Hands the runner the count of bytes of `stream` that the adapter read and dropped, as a
    /// [discard](Request::Discard) asks, which truncates the stream unless it is zero.
    pub fn discarded(&mut self, stream: Stream, bytes: usize) {
        self.outputs[stream.index()].truncated |= bytes > 0;
    }

    /// Hands the runner the end of `stream`: every writer of the workload's stream closed it.
    pub fn output_closed(&mut self, stream: Stream) {
        self.outputs[stream.index()].closed = true;
    }

    /// Hands the runner a failure to read `stream`, which ends the stream as its close does, and
    /// truncates it.
    pub fn output_failed(&mut self, stream: Stream) {
        let output = &mut self.outputs[stream.index()];
        output.closed = true;
        output.truncated = true;
    }

    /// Hands the runner the news that every writer of `stream` closed it, though the adapter has
    /// not read it to its end: the stream ends once the adapter reads what it still holds. No
    /// process can hold the stream open any more, so the runner never gives up on it after the
    /// workload's end, even if the execution does not settle.
    pub fn output_hung_up(&mut self, stream: Stream) {
        self.outputs[stream.index()].hung_up = true;
    }

    /// Returns the bytes of input that the adapter writes to the workload next, as many as are
    /// contiguous: a part of what the runner holds, while it holds any and the input is open.
    pub fn pending_input(&self) -> &[u8] {
        if self.input.closed {
            return &[];
        }
        self.input.inbox.front()
    }

    /// Hands the runner the count of bytes of the [pending input](Self::pending_input) that the
    /// workload's input took, which the runner returns to the peer as credit.
    pub fn input_written(&mut self, bytes: usize) {
        if self.input.closed {
            return;
        }
        let freed = self.input.inbox.pop(bytes);
        self.input.window.free(freed);
        self.close_input_when_taken();
    }

    /// Hands the runner the close of the workload's input other than by its
    /// [request](Request::CloseInput): the workload closed it, or the adapter did as the workload
    /// ended. The runner drops the pending input, and tells the peer unless the peer ended the
    /// input.
    pub fn input_closed(&mut self) {
        self.close_input();
    }

    /// Hands the runner the workload's end at `now`, with its status as its collector reaped it,
    /// or `None` if the status is lost. Only the first end counts. The workload's timeout fires
    /// no more, even if `now` has reached it, as the workload ended before the runner learned it:
    /// the end stands as the workload's own. The runner closes the input, and requests to
    /// [settle](Request::Settle) the execution, whose result it expects within the settlement
    /// time. If the execution does not settle, as its settlement failed or came too late, the
    /// processes that hold its output streams may outlive it, so the runner gives up on the
    /// streams that are still open, but for those that [hung up](Self::output_hung_up), once the
    /// grace after the settlement time has passed too: it reads them no more, sends what it read,
    /// and marks them truncated. A process may also hold back what reports the workload's
    /// [launch](Self::launched), whether or not the execution settles, so a launch result that
    /// has not arrived by then fences the execution as well: the runner gives up on the result,
    /// and on those streams, and sends the workload's output and terminal outcome without it,
    /// with the workload's status and no cause.
    pub fn exited(&mut self, now: Instant, status: Option<Status>) {
        self.timeout_at = None;
        self.tick(now);
        if self.exit.is_some() || self.gave_up {
            return;
        }
        self.exit = Some(status);
        self.close_input();
        self.settle_request = true;
        let settlement = after(now, self.timers.settlement);
        self.settling = Settling::Requested(settlement);
        self.abandon_at = settlement.and_then(|at| after(at, self.timers.grace));
    }

    /// Hands the runner the result of the [settlement](Request::Settle) that it requested: the
    /// execution is reusable once it settled, and fenced if it failed. A result that the runner
    /// did not request, or that comes after the settlement time, changes nothing.
    pub fn settled(&mut self, result: Settlement) {
        if let Settling::Requested(_) = self.settling {
            self.settling = match result {
                Settlement::Settled => Settling::Settled,
                Settlement::Failed => Settling::Fenced,
            };
        }
    }

    /// Ends the workload at `now` for [`Cause::Dropped`], because the adapter abandons the
    /// execution, and detaches the runner from the peer.
    pub fn dropped(&mut self, now: Instant) {
        self.detach(now, Cause::Dropped);
    }

    /// Ends the workload at `now` for [`Cause::Reset`], because the session with the peer was
    /// reset, and detaches the runner from the peer.
    pub fn reset(&mut self, now: Instant) {
        self.detach(now, Cause::Reset);
    }

    /// Ends the workload at `now` for [`Cause::Disconnected`], because the transport to the peer
    /// was lost, and detaches the runner from the peer.
    pub fn disconnect(&mut self, now: Instant) {
        self.detach(now, Cause::Disconnected);
    }

    /// Fires the timers whose instants `now` has reached: the workload's timeout, the end of the
    /// grace of pending output, the settlement time, the time after a termination at which the
    /// runner gives up on the execution unless its cleanup is complete, and the time after the
    /// workload's end at which it gives up on a launch result that has not arrived, and on the
    /// output streams of an execution that did not settle.
    pub fn tick(&mut self, now: Instant) {
        if self.finished {
            return;
        }
        if self.timeout_at.is_some_and(|at| now >= at) {
            self.timeout_at = None;
            self.terminate(now, Cause::TimedOut);
        }
        if self.grace_until.is_some_and(|at| now >= at) {
            self.end_grace();
        }
        if let Settling::Requested(Some(by)) = self.settling
            && now >= by
        {
            self.settling = Settling::Fenced;
        }
        if self.give_up_at.is_some_and(|at| now >= at) {
            if self.is_cleaned_up() {
                self.give_up_at = None;
            } else {
                self.give_up();
            }
        }
        if self.abandon_at.is_some_and(|at| now >= at) {
            if self.awaits_launch() {
                self.abandon_launch();
            }
            if self.settling == Settling::Fenced {
                self.abandon_open_streams();
            }
        }
    }

    /// Returns when the runner's next timer fires, for the adapter to [tick](Self::tick) it
    /// then, or `None` while no timer runs.
    pub fn deadline(&self) -> Option<Instant> {
        if self.finished {
            return None;
        }
        let settlement = match self.settling {
            Settling::Requested(by) => by,
            _ => None,
        };
        let give_up = self.give_up_at.filter(|_| !self.is_cleaned_up());
        let abandon = self.abandon_at.filter(|_| {
            self.awaits_launch()
                || self.settling == Settling::Fenced
                    && self
                        .outputs
                        .iter()
                        .any(|output| !output.closed && !output.hung_up)
        });
        [
            self.timeout_at,
            self.grace_until,
            settlement,
            give_up,
            abandon,
        ]
        .into_iter()
        .fold(None, earliest)
    }

    /// Returns the next request to apply to the workload, or `None` if none waits: ending the
    /// workload first, then discarding streams, closing the input, and settling the execution.
    pub fn next_request(&mut self) -> Option<Request> {
        if let Some(cause) = self.terminate_request.take() {
            return Some(Request::Terminate(cause));
        }
        for stream in Stream::ALL {
            let output = &mut self.outputs[stream.index()];
            if std::mem::take(&mut output.discard_requested) {
                return Some(Request::Discard(stream));
            }
        }
        if std::mem::take(&mut self.input.close_requested) {
            return Some(Request::CloseInput);
        }
        if std::mem::take(&mut self.settle_request) {
            return Some(Request::Settle);
        }
        None
    }

    /// Returns the next message to send the peer, or `None` if none waits: credit of the input,
    /// that the workload started, that its input closed early, the end of each stream once its
    /// bytes are sent, the workload's output, which takes turns between the streams as both have
    /// bytes and credit, and the terminal outcome last, once the workload's status, both streams'
    /// ends, and the settlement are known. A stream's end goes before the other stream's output,
    /// which would otherwise hold it back for as long as it flows. Credit of the input and the
    /// other messages take turns: after credit, any other message that waits goes first, so that
    /// neither starves the other, however much input the workload takes between two calls. An
    /// output message carries the most bytes that the stream's queue, its credit, a chunk, and
    /// `buffer` allow, which it holds at the start of `buffer`; an empty buffer carries none, so
    /// that only other messages return.
    pub fn next_message(&mut self, buffer: &mut [u8]) -> Option<Message> {
        if self.finished {
            return None;
        }
        if !self.detached {
            if !self.credited
                && let Some(message) = self.next_credit()
            {
                self.credited = true;
                return Some(message);
            }
            if let Some(message) = self.next_other(buffer) {
                self.credited = false;
                return Some(message);
            }
            if let Some(message) = self.next_credit() {
                self.credited = true;
                return Some(message);
            }
        }
        let outcome = self.outcome()?;
        self.finished = true;
        Some(Message::Terminal(outcome))
    }

    /// Returns whether the runner has returned its terminal outcome.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Returns whether the runner is detached from the peer.
    pub fn is_detached(&self) -> bool {
        self.detached
    }

    /// Returns whether the runner awaits the result of the [settlement](Request::Settle) that it
    /// requested: from its request until the result, the settlement time, or the runner giving
    /// up, after which the adapter may stop settling the execution.
    pub fn awaits_settlement(&self) -> bool {
        matches!(self.settling, Settling::Requested(_))
    }

    /// Returns the most bytes that the runner holds at once: one chunk of each output stream,
    /// and the input's window.
    pub fn memory_bound(&self) -> usize {
        self.limits
            .chunk()
            .saturating_mul(2)
            .saturating_add(self.limits.input_bytes())
    }

    /// Returns the bytes that the runner holds now.
    pub fn buffered(&self) -> usize {
        self.outputs
            .iter()
            .map(|output| output.queue.len())
            .sum::<usize>()
            + self.input.inbox.len()
    }

    /// Returns the most bytes that the runner has held at once.
    pub fn high_water(&self) -> usize {
        self.high_water
    }

    /// Returns credit of the input to send the peer, if the runner still grants it and the
    /// workload took input since the last grant.
    fn next_credit(&mut self) -> Option<Message> {
        if !self.input_accepts() {
            return None;
        }
        let units = self.input.window.take_grant()?;
        Some(Message::Credit { units })
    }

    /// Returns the next message other than credit of the input: that the workload started, that
    /// its input closed early, the end of each stream once its bytes are sent, and the workload's
    /// output.
    fn next_other(&mut self, buffer: &mut [u8]) -> Option<Message> {
        if std::mem::take(&mut self.started_message) {
            return Some(Message::Started);
        }
        if std::mem::take(&mut self.input.closed_message) {
            return Some(Message::InputClosed);
        }
        if !self.awaits_launch() {
            for stream in Stream::ALL {
                let output = &mut self.outputs[stream.index()];
                if output.is_done() && !output.ended {
                    output.ended = true;
                    return Some(Message::End(stream));
                }
            }
            return self.next_output(buffer);
        }
        None
    }

    /// Returns the next output message, taking turns between the streams.
    fn next_output(&mut self, buffer: &mut [u8]) -> Option<Message> {
        for _ in Stream::ALL {
            let stream = Stream::ALL[self.turn];
            self.turn = 1 - self.turn;
            let credit = &mut self.credits[self.limits.pool_of(stream)];
            let output = &mut self.outputs[stream.index()];
            let wanted = output
                .queue
                .len()
                .min(self.limits.chunk())
                .min(buffer.len());
            let length = credit.sendable(wanted);
            if length > 0 {
                credit::take(&mut output.queue, buffer, length);
                credit.spend(length);
                return Some(Message::Output { stream, length });
            }
        }
        None
    }

    /// Returns whether the runner still grants input credit: the input is open, and the peer
    /// has not ended it.
    fn input_accepts(&self) -> bool {
        !self.input.closed && !self.input.peer_ended
    }

    /// Requests to close the input once the peer ended it and the workload took every byte.
    fn close_input_when_taken(&mut self) {
        if self.input.peer_ended && !self.input.closed && self.input.inbox.is_empty() {
            self.input.closed = true;
            self.input.close_requested = true;
        }
    }

    /// Notes that the workload's input closed, other than by the runner's request: drops the
    /// pending input, and tells the peer unless the peer ended the input.
    fn close_input(&mut self) {
        if self.input.closed {
            return;
        }
        self.input.closed = true;
        self.input.inbox.clear();
        self.input.closed_message = !self.input.peer_ended && !self.detached;
    }

    /// Ends the workload at `now` for `cause` if it still runs, and begins the grace of pending
    /// output, unless a termination began already.
    fn terminate(&mut self, now: Instant, cause: Cause) {
        if self.terminated_at.is_some() {
            return;
        }
        self.terminated_at = Some(now);
        self.timeout_at = None;
        if self.exit.is_none() {
            self.cause = Some(cause);
            self.terminate_request = Some(cause);
        }
        self.close_input();
        self.grace_until = after(now, self.timers.grace);
        self.give_up_at = self
            .grace_until
            .and_then(|grace| after(grace, self.timers.settlement));
        if self.grace_until.is_some_and(|at| now >= at) {
            self.end_grace();
        }
    }

    /// Detaches the runner from the peer at `now` for `cause`: ends the workload if it still
    /// runs, and discards its output at once, as no peer is left to take it.
    fn detach(&mut self, now: Instant, cause: Cause) {
        self.tick(now);
        if self.finished || self.detached {
            return;
        }
        self.terminate(now, cause);
        self.detached = true;
        self.started_message = false;
        self.input.closed_message = false;
        self.end_grace();
        let give_up = after(now, self.timers.settlement);
        self.give_up_at = earliest(self.give_up_at, give_up);
    }

    /// Ends the grace of pending output: drops what the queues hold, and requests to discard
    /// each stream that is still open.
    fn end_grace(&mut self) {
        self.grace_until = None;
        for output in &mut self.outputs {
            output.drop_queue();
            if !output.closed && !output.discarding {
                output.discarding = true;
                output.discard_requested = true;
            }
        }
    }

    /// Gives up on the execution after a termination: whatever is still unknown or open stays
    /// so, each stream that did not close is truncated, and the execution is fenced.
    fn give_up(&mut self) {
        self.give_up_at = None;
        self.grace_until = None;
        self.gave_up = true;
        for output in &mut self.outputs {
            output.drop_queue();
            if !output.closed {
                output.closed = true;
                output.truncated = true;
            }
        }
        if self.exit.is_none() {
            self.exit = Some(None);
        }
        self.settling = Settling::Fenced;
        self.settle_request = false;
    }

    /// Gives up on the launch result, which has not arrived once the settlement time and the
    /// grace have passed since the workload's end: a process that outlived the workload holds
    /// what reports it. The execution's cleanup is incomplete, so the runner fences it, and then
    /// gives up on its open streams too, and it sends the workload's output and terminal outcome
    /// without the result.
    fn abandon_launch(&mut self) {
        self.launch_abandoned = true;
        self.settling = Settling::Fenced;
    }

    /// Gives up on the output streams that are still open after the workload's end, once the
    /// execution did not settle, but for those that hung up: the processes that hold them
    /// outlived the cleanup, so they may never close. The runner reads them no more and marks
    /// them truncated, but still sends what it read, and loses nothing of the streams that closed
    /// or hung up.
    fn abandon_open_streams(&mut self) {
        self.abandon_at = None;
        for output in &mut self.outputs {
            if !output.closed && !output.hung_up {
                output.closed = true;
                output.truncated = true;
            }
        }
    }

    /// Returns whether the runner still waits for the workload's launch result: it has not
    /// arrived, and the runner has not given up on it.
    fn awaits_launch(&self) -> bool {
        self.launch.is_none() && !self.gave_up && !self.launch_abandoned
    }

    /// Returns whether the execution's cleanup is complete: the workload's launch result and
    /// status are known, both of its streams closed, and the execution settled. Nothing is left
    /// to give up on then but the delivery of the last messages, which the transport paces.
    fn is_cleaned_up(&self) -> bool {
        self.launch.is_some()
            && self.exit.is_some()
            && self.outputs.iter().all(|output| output.closed)
            && self.settling == Settling::Settled
    }

    /// Returns the terminal outcome once everything that it reports is known: the launch
    /// result, the workload's status, both streams' ends, and the settlement.
    fn outcome(&self) -> Option<Outcome> {
        let delivered = self
            .outputs
            .iter()
            .all(|output| output.is_done() && (output.ended || self.detached));
        let settled = matches!(self.settling, Settling::Settled | Settling::Fenced);
        if self.awaits_launch() || self.exit.is_none() || !delivered || !settled {
            return None;
        }
        let mut outcome = Outcome::new(self.exit.flatten());
        if let Some(cause) = self.cause {
            outcome = outcome.with_cause(cause);
        }
        if let Some(Err(failure)) = self.launch {
            outcome = outcome.with_launch_failure(failure);
        }
        for stream in Stream::ALL {
            if self.outputs[stream.index()].truncated {
                outcome = outcome.with_truncated(stream);
            }
        }
        if self.settling == Settling::Settled {
            outcome = outcome.with_reusable();
        }
        Some(outcome)
    }

    /// Notes the bytes that the runner holds, for its high-water mark.
    fn note_buffered(&mut self) {
        self.high_water = self.high_water.max(self.buffered());
    }
}
