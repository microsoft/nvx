//! The side of an execution that requested it: see the [module](super) documentation.

use std::collections::VecDeque;
use std::time::Instant;

use super::credit::{self, Credit, Granted, Inbox};
use super::{
    Cause, Key, Limits, Outcome, Pool, Receipt, Stream, Timers, Violation, after, earliest,
};

/// A message from the peer to the runner, which the adapter frames and sends in order, as
/// [`Peer::next_message`] returns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Message {
    /// End the workload for `cause`: [`Cause::Cancelled`], [`Cause::Dropped`], or
    /// [`Cause::TimedOut`], as the peer's caller or timer ended the execution.
    Cancel(Cause),
    /// Credit of the output pool `pool`: `units` more of its window, which the runner may spend
    /// on output. The first one of each pool opens its whole window.
    Credit {
        /// The output pool: [`Pool::Output`] if the streams share one, otherwise
        /// [`Pool::Stdout`] or [`Pool::Stderr`].
        pool: Pool,
        /// The units of the credit.
        units: u32,
    },
    /// Bytes of the workload's input, in order: the first `length` bytes of the buffer that the
    /// adapter lent [`Peer::next_message`].
    Input {
        /// The number of bytes, at least one and at most a chunk.
        length: usize,
    },
    /// The end of the workload's input, after its last byte.
    InputEnd,
}

/// What the peer tells its caller, as [`Peer::next_event`] returns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Event {
    /// The workload's program runs: the runner confirmed its launch.
    Started,
    /// `stream` ended: its sink took its last byte, or the peer discarded the rest.
    End(Stream),
    /// The workload's input closed before its end was sent, or the execution ended: the caller
    /// sends no more input, and the peer dropped what it held.
    InputClosed,
    /// The execution finished, with this outcome: the peer's last event. Its cause is the
    /// runner's, or, if the runner reported none or never reported, the peer's own.
    Finished(Outcome),
}

/// One output stream, as the peer receives and delivers it.
#[derive(Debug)]
struct Incoming {
    /// The bytes that the runner sent and the sink has not taken.
    inbox: Inbox,
    /// Whether the runner ended the stream.
    ended: bool,
    /// Whether the stream's end event was returned.
    end_event: bool,
    /// Whether the peer drops the stream's bytes, as no sink takes them any more.
    discarding: bool,
    /// Whether the peer dropped bytes of the stream.
    truncated: bool,
}

impl Incoming {
    /// Drops what the inbox holds, notes whether that lost bytes, and returns the units that
    /// this frees.
    fn drop_inbox(&mut self) -> u32 {
        let (length, freed) = self.inbox.clear();
        self.truncated |= length > 0;
        freed
    }
}

/// The workload's standard input, as the peer takes it from its caller and sends it.
#[derive(Debug)]
struct Outgoing {
    /// The bytes that the caller handed over and the peer has not sent: at most one chunk.
    queue: VecDeque<u8>,
    /// The credit that the runner granted.
    credit: Credit,
    /// Whether the caller ended the input.
    ended: bool,
    /// Whether the end of the input was sent.
    end_sent: bool,
    /// Whether the input closed: the runner closed it, or the execution ended.
    closed: bool,
    /// Whether the event that the input closed waits to be returned.
    closed_event: bool,
}

/// The side of an execution that requested it: see the [module](super) documentation.
///
/// The adapter creates a peer as it asks the runner's side to run a workload, and hands it the
/// runner's messages and its caller's input. In return, it takes the peer's
/// [messages](Self::next_message), which it sends the runner, and its
/// [events](Self::next_event), which it tells its caller, until the execution
/// [finishes](Event::Finished). Its sink takes each stream's [pending output](Self::pending_output)
/// as it can, and acknowledges what it took with [`consumed`](Self::consumed); its source hands
/// over input as the [write allowance](Self::write_allowance) lets it.
#[derive(Debug)]
pub struct Peer {
    /// The execution's key.
    key: Key,
    /// The limits that both sides agreed on.
    limits: Limits,
    /// The peer's timers.
    timers: Timers,
    /// Whether the runner confirmed the workload's launch.
    started: bool,
    /// Whether the event that the workload started waits to be returned.
    started_event: bool,
    /// The workload's standard input.
    input: Outgoing,
    /// The output streams.
    outputs: [Incoming; 2],
    /// The windows of the output pools: one per stream, or the first one shared.
    windows: [Granted; 2],
    /// The source whose message the peer returns first next: each output pool's credit, in the
    /// order of the pools, then the input. They take turns, so that none starves another.
    turn: usize,
    /// The runner's terminal outcome, once it arrived.
    terminal: Option<Outcome>,
    /// Why the peer ended the execution, if it did.
    cause: Option<Cause>,
    /// Whether the cancellation waits to be sent.
    cancel_message: bool,
    /// When the execution's timeout passes, while it runs.
    timeout_at: Option<Instant>,
    /// When the peer ended the execution, once it did.
    terminated_at: Option<Instant>,
    /// When the grace of pending output ends, while the peer waits for it.
    grace_until: Option<Instant>,
    /// When the peer gives up waiting for the terminal outcome of an execution that it ended.
    give_up_at: Option<Instant>,
    /// Whether the peer gave up waiting for the terminal outcome.
    gave_up: bool,
    /// Whether the peer is detached from the runner, from which nothing more arrives.
    detached: bool,
    /// Whether the outcome was returned.
    finished: bool,
    /// The most bytes that the peer held at once.
    high_water: usize,
}

impl Peer {
    /// Returns the peer of the execution `key`, with the `limits` that both sides agreed on and
    /// its own `timers`, created at `now`, from which its timeout runs.
    pub fn new(key: Key, limits: Limits, timers: Timers, now: Instant) -> Self {
        let incoming = || Incoming {
            inbox: Inbox::new(limits.output.unit()),
            ended: false,
            end_event: false,
            discarding: false,
            truncated: false,
        };
        Self {
            key,
            limits,
            timers,
            started: false,
            started_event: false,
            input: Outgoing {
                queue: VecDeque::new(),
                credit: Credit::new(limits.input),
                ended: false,
                end_sent: false,
                closed: false,
                closed_event: false,
            },
            outputs: [incoming(), incoming()],
            windows: [Granted::new(limits.output), Granted::new(limits.output)],
            turn: 0,
            terminal: None,
            cause: None,
            cancel_message: false,
            timeout_at: timers.timeout.and_then(|timeout| after(now, timeout)),
            terminated_at: None,
            grace_until: None,
            give_up_at: None,
            gave_up: false,
            detached: false,
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

    /// Hands the peer the runner's credit of `units` for the workload's input. A grant of no
    /// units, or of more than the units in flight, is a [`Violation::Credit`].
    pub fn credit(&mut self, key: Key, units: u32) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        if self.ignores_runner() {
            return Ok(Receipt::Late);
        }
        self.input.credit.grant(units)?;
        if self.input.closed || self.input.end_sent {
            return Ok(Receipt::Late);
        }
        Ok(Receipt::Applied)
    }

    /// Hands the peer the runner's confirmation that the workload started. A second one, or one
    /// after the terminal outcome, is a [`Violation::Order`].
    pub fn started(&mut self, key: Key) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        if self.ignores_runner() {
            return Ok(Receipt::Late);
        }
        if self.started || self.terminal.is_some() {
            return Err(Violation::Order);
        }
        self.started = true;
        self.started_event = true;
        Ok(Receipt::Applied)
    }

    /// Hands the peer bytes of `stream` from the runner, which the stream's pool must allow, and
    /// which the peer holds until its sink takes them. Output after the stream's end is a
    /// [`Violation::Ended`]. Once the peer discards the stream, it drops what arrives, as
    /// [`Receipt::Late`], and grants its units again at once.
    pub fn output(&mut self, key: Key, stream: Stream, bytes: &[u8]) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        if self.ignores_runner() {
            return Ok(Receipt::Late);
        }
        if self.outputs[stream.index()].ended {
            return Err(Violation::Ended);
        }
        self.limits.check_chunk(bytes.len())?;
        let window = &mut self.windows[self.limits.pool_of(stream)];
        window.charge(bytes.len())?;
        let incoming = &mut self.outputs[stream.index()];
        if incoming.discarding {
            incoming.truncated = true;
            window.free(self.limits.output.cost(bytes.len()));
            return Ok(Receipt::Late);
        }
        incoming.inbox.push(bytes);
        self.note_buffered();
        Ok(Receipt::Applied)
    }

    /// Hands the peer the end of `stream` from the runner, after its last byte. A second end is
    /// a [`Violation::Ended`].
    pub fn end_output(&mut self, key: Key, stream: Stream) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        if self.ignores_runner() {
            return Ok(Receipt::Late);
        }
        let incoming = &mut self.outputs[stream.index()];
        if incoming.ended {
            return Err(Violation::Ended);
        }
        incoming.ended = true;
        Ok(Receipt::Applied)
    }

    /// Hands the peer the runner's notice that the workload's input closed early. The peer drops
    /// the input that it holds, takes no more, and tells its caller.
    pub fn input_closed(&mut self, key: Key) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        if self.ignores_runner() || self.input.closed || self.input.end_sent {
            return Ok(Receipt::Late);
        }
        self.close_input();
        Ok(Receipt::Applied)
    }

    /// Hands the peer the runner's terminal outcome at `now`, which follows both streams' ends:
    /// a terminal outcome before them is a [`Violation::Order`], and a second one a
    /// [`Violation::Ended`]. The peer finishes once its sink took the rest of the output, losing
    /// nothing, unless a side ended the execution: then the sink takes the rest only for the
    /// peer's grace, from when the peer ended the execution, or from `now` if the runner did.
    /// Once the peer detached from the runner or gave up waiting for it, the terminal outcome is
    /// [late](Receipt::Late), as every message of the runner's is then, and the peer's own
    /// outcome stands, whenever its caller retrieves it.
    pub fn terminal(
        &mut self,
        now: Instant,
        key: Key,
        outcome: Outcome,
    ) -> Result<Receipt, Violation> {
        if !self.key.admits(key)? {
            return Ok(Receipt::Stale);
        }
        self.tick(now);
        if self.ignores_runner() {
            return Ok(Receipt::Late);
        }
        if self.terminal.is_some() {
            return Err(Violation::Ended);
        }
        if !self.outputs.iter().all(|incoming| incoming.ended) {
            return Err(Violation::Order);
        }
        self.terminal = Some(outcome);
        self.give_up_at = None;
        self.timeout_at = None;
        self.cancel_message = false;
        self.close_input();
        if outcome.cause().is_some() && self.terminated_at.is_none() {
            self.terminated_at = Some(now);
            self.grace_until = after(now, self.timers.grace);
            if self.grace_until.is_some_and(|at| now >= at) {
                self.end_grace();
            }
        }
        Ok(Receipt::Applied)
    }

    /// Returns how many more bytes of input the caller may hand over with
    /// [`input`](Self::input): what the peer's queue has room for, at most one chunk ahead of the
    /// runner's credit, and none once the input ended or closed, or the peer ended the
    /// execution.
    pub fn write_allowance(&self) -> usize {
        if self.input.closed || self.input.ended || self.terminated_at.is_some() || self.finished {
            return 0;
        }
        self.limits.chunk() - self.input.queue.len()
    }

    /// Hands the peer bytes of the workload's input from its caller, in order, and returns how
    /// many it took: as many as the [write allowance](Self::write_allowance) lets it.
    pub fn input(&mut self, bytes: &[u8]) -> usize {
        let length = bytes.len().min(self.write_allowance());
        self.input.queue.extend(&bytes[..length]);
        self.note_buffered();
        length
    }

    /// Ends the workload's input after the bytes that the caller handed over.
    pub fn end_input(&mut self) {
        if !self.input.closed {
            self.input.ended = true;
        }
    }

    /// Returns the bytes of `stream` that the sink takes next, as many as are contiguous: a part
    /// of what the peer holds, while it holds any.
    pub fn pending_output(&self, stream: Stream) -> &[u8] {
        self.outputs[stream.index()].inbox.front()
    }

    /// Hands the peer the count of bytes of `stream`'s [pending output](Self::pending_output)
    /// that the sink took, which the peer grants the runner again.
    pub fn consumed(&mut self, stream: Stream, bytes: usize) {
        let freed = self.outputs[stream.index()].inbox.pop(bytes);
        self.windows[self.limits.pool_of(stream)].free(freed);
    }

    /// Ends the execution at `now` for [`Cause::Cancelled`]: the peer sends the runner its
    /// cancellation, takes no more input, delivers the pending output for its grace, and
    /// waits for the terminal outcome for the settlement time after that.
    pub fn cancel(&mut self, now: Instant) {
        self.tick(now);
        self.terminate(now, Cause::Cancelled);
    }

    /// Ends the execution at `now` for [`Cause::Dropped`], because the caller dropped its
    /// handle: as a [cancellation](Self::cancel), but the peer discards the output at once, as
    /// no sink takes it any more.
    pub fn dropped(&mut self, now: Instant) {
        self.tick(now);
        self.terminate(now, Cause::Dropped);
        if !self.finished {
            self.end_grace();
        }
    }

    /// Detaches the peer from the runner at `now` for [`Cause::Reset`], because the session was
    /// reset. The peer takes none of the runner's messages from then on.
    pub fn reset(&mut self, now: Instant) {
        self.detach(now, Cause::Reset);
    }

    /// Detaches the peer from the runner at `now` for [`Cause::Disconnected`], because the
    /// transport was lost. The peer takes none of the runner's messages from then on.
    pub fn disconnect(&mut self, now: Instant) {
        self.detach(now, Cause::Disconnected);
    }

    /// Fires the timers whose instants `now` has reached: the execution's timeout, the end of
    /// the grace of pending output, and the time at which the peer gives up waiting for the
    /// terminal outcome of an execution that it ended.
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
        if self.give_up_at.is_some_and(|at| now >= at) {
            self.give_up_at = None;
            self.gave_up = true;
            self.end_grace();
        }
    }

    /// Returns when the peer's next timer fires, for the adapter to [tick](Self::tick) it then,
    /// or `None` while no timer runs.
    pub fn deadline(&self) -> Option<Instant> {
        if self.finished {
            return None;
        }
        [self.timeout_at, self.grace_until, self.give_up_at]
            .into_iter()
            .fold(None, earliest)
    }

    /// Returns the next message to send the runner, or `None` if none waits: the cancellation
    /// first, then credit of each output pool, the input, and its end once its bytes are sent.
    /// Each pool's credit and the input take turns: after one of them, the others that wait go
    /// first, so that none starves another, however much output the sink takes between two
    /// calls. An input message carries the most bytes that the queue, the credit, a chunk, and
    /// `buffer` allow, which it holds at the start of `buffer`; an empty buffer carries none.
    pub fn next_message(&mut self, buffer: &mut [u8]) -> Option<Message> {
        if self.finished || self.detached {
            return None;
        }
        if std::mem::take(&mut self.cancel_message) {
            return Some(Message::Cancel(self.cause.unwrap_or(Cause::Cancelled)));
        }
        let pools = self.limits.output_pools().len();
        let sources = pools + 1;
        for offset in 0..sources {
            let source = (self.turn + offset) % sources;
            let message = if source < pools {
                self.next_credit(source)
            } else {
                self.next_input(buffer)
            };
            if message.is_some() {
                self.turn = (source + 1) % sources;
                return message;
            }
        }
        None
    }

    /// Returns credit of the output pool at `index` to send the runner, if a stream of the pool
    /// is still open, no terminal outcome arrived, and the sink took bytes since the last grant.
    fn next_credit(&mut self, index: usize) -> Option<Message> {
        if self.terminal.is_some() {
            return None;
        }
        let open = Stream::ALL.into_iter().any(|stream| {
            self.limits.pool_of(stream) == index && !self.outputs[stream.index()].ended
        });
        if !open {
            return None;
        }
        let units = self.windows[index].take_grant()?;
        let pool = self.limits.output_pools()[index];
        Some(Message::Credit { pool, units })
    }

    /// Returns the next input message to send the runner: the input, then its end once its bytes
    /// are sent, unless the input closed.
    fn next_input(&mut self, buffer: &mut [u8]) -> Option<Message> {
        if self.input.closed {
            return None;
        }
        let wanted = self
            .input
            .queue
            .len()
            .min(self.limits.chunk()
            .min(buffer.len()));
        let length = self.input.credit.sendable(wanted);
        if length > 0 {
            credit::take(&mut self.input.queue, buffer, length);
            self.input.credit.spend(length);
            return Some(Message::Input { length });
        }
        if self.input.ended && self.input.queue.is_empty() && !self.input.end_sent {
            self.input.end_sent = true;
            return Some(Message::InputEnd);
        }
        None
    }

    /// Returns the next event for the caller, or `None` if none waits: that the workload
    /// started, that its input closed, the end of each stream once its sink took its last byte,
    /// and the outcome last, once the runner's terminal outcome arrived and the sink took the
    /// rest of the output, or once the peer detached or gave up and delivered or discarded
    /// what it held.
    pub fn next_event(&mut self) -> Option<Event> {
        if self.finished {
            return None;
        }
        if std::mem::take(&mut self.started_event) {
            return Some(Event::Started);
        }
        if std::mem::take(&mut self.input.closed_event) {
            return Some(Event::InputClosed);
        }
        for stream in Stream::ALL {
            let incoming = &mut self.outputs[stream.index()];
            if incoming.ended && incoming.inbox.is_empty() && !incoming.end_event {
                incoming.end_event = true;
                return Some(Event::End(stream));
            }
        }
        let outcome = self.outcome()?;
        self.finished = true;
        Some(Event::Finished(outcome))
    }

    /// Returns whether the peer has returned the execution's outcome.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Returns whether the runner confirmed the workload's launch.
    pub fn has_started(&self) -> bool {
        self.started
    }

    /// Returns whether the peer is detached from the runner.
    pub fn is_detached(&self) -> bool {
        self.detached
    }

    /// Returns the most bytes that the peer holds at once: one chunk of input, and the output's
    /// windows.
    pub fn memory_bound(&self) -> usize {
        self.limits
            .chunk()
            .saturating_add(self.limits.output_bytes())
    }

    /// Returns the bytes that the peer holds now.
    pub fn buffered(&self) -> usize {
        self.input.queue.len()
            + self
                .outputs
                .iter()
                .map(|incoming| incoming.inbox.len())
                .sum::<usize>()
    }

    /// Returns the most bytes that the peer has held at once.
    pub fn high_water(&self) -> usize {
        self.high_water
    }

    /// Returns whether the peer takes no more of the runner's messages: it finished, detached
    /// from the runner, or gave up waiting for it. So its outcome is fixed from then on, whenever
    /// its caller retrieves it.
    fn ignores_runner(&self) -> bool {
        self.finished || self.detached || self.gave_up
    }

    /// Ends the execution at `now` for `cause`, unless the peer ended it already: sends the
    /// runner the cancellation, unless the runner's terminal outcome arrived, closes the input,
    /// and begins the grace of pending output, after which the peer waits for the terminal
    /// outcome for the settlement time.
    fn terminate(&mut self, now: Instant, cause: Cause) {
        if self.finished || self.terminated_at.is_some() {
            return;
        }
        self.terminated_at = Some(now);
        self.timeout_at = None;
        self.cause = Some(cause);
        self.close_input();
        self.grace_until = after(now, self.timers.grace);
        if self.terminal.is_none() {
            self.cancel_message = !self.detached;
            self.give_up_at = self
                .grace_until
                .and_then(|grace| after(grace, self.timers.settlement));
        }
        if self.grace_until.is_some_and(|at| now >= at) {
            self.end_grace();
        }
    }

    /// Detaches the peer from the runner at `now` for `cause`: ends the execution if the peer
    /// has not, and stops waiting for the runner, from which nothing more arrives.
    fn detach(&mut self, now: Instant, cause: Cause) {
        self.tick(now);
        if self.finished || self.detached {
            return;
        }
        self.terminate(now, cause);
        self.detached = true;
        self.cancel_message = false;
        self.give_up_at = None;
    }

    /// Closes the input: drops what the peer holds of it, and tells the caller unless the caller
    /// ended it.
    fn close_input(&mut self) {
        if self.input.closed {
            return;
        }
        self.input.closed = true;
        self.input.queue = VecDeque::new();
        self.input.closed_event = !self.input.end_sent;
    }

    /// Ends the grace of pending output: drops what the inboxes hold, grants their units again,
    /// and discards what still arrives.
    fn end_grace(&mut self) {
        self.grace_until = None;
        for stream in Stream::ALL {
            let incoming = &mut self.outputs[stream.index()];
            incoming.discarding = true;
            let freed = incoming.drop_inbox();
            self.windows[self.limits.pool_of(stream)].free(freed);
        }
    }

    /// Returns the outcome once the peer can finish: the runner's terminal outcome, with the
    /// streams that the peer truncated, once the sink took or the peer discarded the rest of the
    /// output; or, once the peer detached or gave up, an outcome of its own, with no status, its
    /// cause, each stream truncated that did not end whole, and fenced.
    fn outcome(&self) -> Option<Outcome> {
        if !self
            .outputs
            .iter()
            .all(|incoming| incoming.inbox.is_empty())
        {
            return None;
        }
        let mut outcome = match self.terminal {
            Some(terminal) => terminal,
            None if self.detached || self.gave_up => {
                let mut outcome = Outcome::new(None);
                for stream in Stream::ALL {
                    outcome = outcome.with_truncated(stream);
                }
                outcome
            }
            None => return None,
        };
        if outcome.cause().is_none()
            && let Some(cause) = self.cause
        {
            outcome = outcome.with_cause(cause);
        }
        for stream in Stream::ALL {
            if self.outputs[stream.index()].truncated {
                outcome = outcome.with_truncated(stream);
            }
        }
        Some(outcome)
    }

    /// Notes the bytes that the peer holds, for its high-water mark.
    fn note_buffered(&mut self) {
        self.high_water = self.high_water.max(self.buffered());
    }
}
