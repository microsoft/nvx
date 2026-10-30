//! The runner of the [execution engine](crate::exec), bound to an [`Execution`] of the guest's
//! driver: the glue that every guest adapter would otherwise write.
//!
//! A [`Binding`] applies the [`Runner`]'s requests to the execution: its read allowances become
//! [grants](Execution::grant), its discards [discards](Execution::discard), the input that it
//! accepted [writes](Execution::write_input) and, once the peer ended it,
//! [a close](Execution::close_input), its terminations a [cancellation](Execution::cancel), a
//! [disconnection](Execution::disconnect), or a [termination](Execution::terminate), and its
//! settlement a kill and an occupancy check of the execution's [cgroup](super::Options::cgroup). It
//! feeds the execution's events back to the runner: the launch report, the output and its ends,
//! the bytes that the execution discarded, the program's end, once the execution collected its
//! status or the caller's own collector handed it over with [`observe`](Binding::observe), as
//! [`Options::caller_reaps`](super::Options::caller_reaps) has it, and, after the program's end,
//! the streams that [hung up](crate::exec::Runner::output_hung_up), which no process holds any
//! more.
//!
//! A [signal](Binding::signal) that the adapter sends the program itself, such as the first step
//! of a graceful stop, passes through to the execution, which it does not end: the runner sees
//! nothing of it until the program exits.
//!
//! A launch that failed reaches the runner as a [`LaunchFailure`] whose
//! [stage](LaunchFailure::stage) is the [number](crate::guest::spawn::LaunchStage::code) of the
//! [`LaunchStage`](crate::guest::spawn::LaunchStage) that failed, which
//! [`LaunchStage::from_code`](crate::guest::spawn::LaunchStage::from_code) turns back into the
//! stage, or 0, which numbers no stage, if the binding could not read the launch report. The
//! numbers are part of the API: a stage keeps its number, and a new stage takes a number of its
//! own.
//!
//! The adapter hands the binding the peer's messages, frames the runner's
//! [messages](crate::exec::runner::Message) that [`next`](Binding::next) returns, and drives the
//! binding without blocking, as it would the execution alone: it calls `next` until that returns
//! `None`, then waits until one of the binding's [descriptors](Binding::descriptors) polls
//! readable, its [input writer](Binding::input_writer) polls writable, its
//! [deadline](Binding::deadline) passes, or a message arrives. A stalled peer, sink, or transport
//! only withholds credit, so the binding stops reading the stream that waits on it, and nothing
//! else. Each call takes a bounded number of steps, its writes of the input among them, so that
//! no execution holds the adapter's loop: a call that stops with steps left sets the deadline to
//! its own instant, which the adapter's next wait finds passed already. Once the binding returned
//! a message, it steps the execution, if it can, before it returns another, so that input that
//! the adapter hands it between calls never keeps it from reading the program's output.
//!
//! Settlement goes through the cgroup that the execution's options name, if any, in the scope
//! that they give: once the program's status is known, the binding kills the cgroup's processes,
//! and reports the execution settled once the cgroup is empty, which it checks at most every 10
//! ms. From then on, the cgroup is the caller's again: neither the execution nor its drop kills
//! the cgroup's processes, so the caller may reuse the cgroup once the outcome is reusable,
//! whether or not it dropped the binding. A zombie still occupies the cgroup until its parent
//! collects it, so a caller whose executions' orphans it adopts, such as a PID 1, collects their
//! statuses between its calls.
//! Without a cgroup, nothing proves that the program's descendants are gone, so the execution
//! never settles, and is fenced: the runner then gives up on the output streams that a process
//! may still hold the settlement time and the grace after the program's end, while it waits as
//! long as the peer takes for the rest of those that hung up. The runner's timers time the
//! execution: an execution with a [timeout](super::Options::timeout) of its own ends its program
//! without telling the runner why.

use std::collections::VecDeque;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use super::{Event as DriverEvent, Execution, KillTarget, Source, Stream as DriverStream};
use crate::exec::{
    Cause, Key, LaunchFailure, Pool, Receipt, Runner, Settlement, Status, Stream, Violation, runner,
};

/// The interval between two checks of a settling cgroup.
const SETTLE_INTERVAL: Duration = Duration::from_millis(10);

/// The most steps that one call of [`Binding::next`] takes: steps of the execution, each of which
/// reads at most the caller's buffer, and writes of the input, each of at most a chunk. So neither
/// output that the execution discards as fast as a process writes it, nor input that a process
/// reads as fast as the binding writes it, holds the caller's loop.
const STEPS: usize = 16;

/// What one step of a [`Binding`] reports.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    /// A message for the peer, which the adapter frames and sends in order. An output message's
    /// bytes are at the start of the buffer that the adapter lent [`Binding::next`].
    Message(runner::Message),
    /// A kill that ending the program needed failed, as the execution reports it. The binding
    /// goes on: the runner's deadline bounds a program that the kill did not end.
    KillFailed(KillTarget, io::Error),
    /// Killing or checking the execution's cgroup failed, which fenced the execution.
    SettlementFailed(io::Error),
    /// A write of the program's input failed, other than because the program closed its end. The
    /// binding closed the input, so that the program reads its end, and the runner tells the peer
    /// that the input closed.
    InputFailed(io::Error),
}

/// An [`Execution`] that the execution engine's [`Runner`] drives: see the [module](self)
/// documentation.
///
/// Dropping a binding drops its execution, which ends a program that has not ended, and the
/// processes of its cgroup unless the execution settled.
#[derive(Debug)]
pub struct Binding {
    /// The execution.
    execution: Execution,
    /// The runner.
    runner: Runner,
    /// How many bytes of each stream the binding has granted the execution, and the execution
    /// has not read.
    granted: [usize; 2],
    /// Whether the binding fed the execution's launch report to the runner.
    launched: bool,
    /// Whether the binding reported the program's end to the runner.
    exited: bool,
    /// Whether the binding told the runner that each stream hung up.
    hung_up: [bool; 2],
    /// Whether the execution ended: the driver has no step left.
    ended: bool,
    /// When the binding checks the settling cgroup next, while it settles it.
    settling: Option<Instant>,
    /// The instant of the last call of [`next`](Self::next) if it took [`STEPS`] steps without an
    /// event to return, and may have more to take: the binding's deadline, which has passed
    /// already.
    paused: Option<Instant>,
    /// Whether the execution's turn is due: once the binding returned a message, it takes a step
    /// of the execution, if it has one, before it returns another, so that input credit, which
    /// the writes of every call may free, never keeps it from reading the program's output.
    execution_turn: bool,
    /// The events that wait to be returned: failures, which the binding reports before messages.
    events: VecDeque<Event>,
}

impl Binding {
    /// Binds `runner` to `execution`, whose program its adapter spawned, or adopted, for the
    /// runner's execution. An execution without an input pipe gives its program no input that the
    /// peer could send, so the runner tells the peer that the input closed.
    pub fn new(execution: Execution, runner: Runner) -> Self {
        let mut binding = Self {
            execution,
            runner,
            granted: [0, 0],
            launched: false,
            exited: false,
            hung_up: [false, false],
            ended: false,
            settling: None,
            paused: None,
            execution_turn: true,
            events: VecDeque::new(),
        };
        if binding.execution.input().is_none() {
            binding.runner.input_closed();
        }
        binding
    }

    /// Returns the runner, whose state the adapter may inspect.
    pub fn runner(&self) -> &Runner {
        &self.runner
    }

    /// Returns the execution, whose state the adapter may inspect.
    pub fn execution(&self) -> &Execution {
        &self.execution
    }

    /// Returns the program's process ID.
    pub fn id(&self) -> u32 {
        self.execution.id()
    }

    /// Hands the runner the peer's credit: see [`Runner::credit`].
    pub fn credit(&mut self, key: Key, pool: Pool, units: u32) -> Result<Receipt, Violation> {
        self.runner.credit(key, pool, units)
    }

    /// Hands the runner the peer's input: see [`Runner::input`].
    pub fn input(&mut self, key: Key, bytes: &[u8]) -> Result<Receipt, Violation> {
        self.runner.input(key, bytes)
    }

    /// Hands the runner the end of the peer's input: see [`Runner::end_input`].
    pub fn end_input(&mut self, key: Key) -> Result<Receipt, Violation> {
        self.runner.end_input(key)
    }

    /// Hands the runner the peer's cancellation: see [`Runner::cancel`].
    pub fn cancel(&mut self, now: Instant, key: Key, cause: Cause) -> Result<Receipt, Violation> {
        self.runner.cancel(now, key, cause)
    }

    /// Ends the execution because the adapter abandons it: see [`Runner::dropped`].
    pub fn dropped(&mut self, now: Instant) {
        self.runner.dropped(now);
    }

    /// Ends the execution because the session was reset: see [`Runner::reset`].
    pub fn reset(&mut self, now: Instant) {
        self.runner.reset(now);
    }

    /// Ends the execution because the transport was lost: see [`Runner::disconnect`].
    pub fn disconnect(&mut self, now: Instant) {
        self.runner.disconnect(now);
    }

    /// Sends the program the signal numbered `signal`, without ending the execution, and returns
    /// whether the program was unreaped, as [`Execution::signal`] does. The runner learns nothing
    /// of the signal: it sees the program's end, with the program's own status, once the program
    /// exits.
    pub fn signal(&mut self, signal: i32) -> io::Result<bool> {
        self.execution.signal(signal)
    }

    /// Hands the execution the status that the caller's own collector collected for the process
    /// `id`, and returns whether it was the program's, as [`Execution::observe`] does. The next
    /// call of [`next`](Self::next) reports the program's end to the runner before anything else.
    pub fn observe(&mut self, id: u32, status: ExitStatus) -> bool {
        self.execution.observe(id, status)
    }

    /// Takes the next step without blocking, at `now`, and returns what it reports, or `None` if
    /// nothing can happen until a descriptor polls ready, the deadline passes, or a message
    /// arrives. Each read of an output stream goes into `buffer`, which bounds it, and each
    /// output message's bytes are at the start of `buffer`.
    ///
    /// Each call first collects the program's status without blocking, unless the caller's own
    /// collector takes it, and takes the program's launch report once it has arrived. Within a
    /// step, the binding then reports the program's end to the runner once the execution has its
    /// status, whether or not the launch report has arrived, which a process that another thread
    /// forked meanwhile may hold back, tells the runner which streams hung up once a
    /// timer is due after the program's end, fires the runner's timers, checks a settling
    /// cgroup if a check is due, writes the input that the program's pipe takes, at most a chunk
    /// at a time, applies the runner's requests, those that the last write raised included, such
    /// as closing the input after its last byte, grants the reads that the runner allows, and
    /// returns the runner's next message; failing that, it takes the execution's next step, feeds
    /// its event to the runner, and starts over. So the program's end reaches the runner before
    /// its timers fire or a queued message returns, even in a first call that comes after the
    /// runner's timeout. Once the binding returned a message, the execution's turn comes before
    /// the next: it takes the execution's next step, if it has one, and starts over, so that
    /// input that the caller hands it between calls, which frees credit for each of them, never
    /// keeps it from reading the program's output. The binding also starts over while anything
    /// else changed, so that every request that a step raised is applied before it returns
    /// `None`, and a due check runs however much output flows. It bounds the work of one call:
    /// the steps of the execution and the writes of the input count against one bound, of which
    /// the writes leave a step for the execution's turn while it is due, and a call that reaches
    /// it without an event to return sets the [deadline](Self::deadline) to `now`, so that the
    /// caller calls it again at once, after its other work. After the terminal outcome, it
    /// returns `None`.
    pub fn next(&mut self, now: Instant, buffer: &mut [u8]) -> Option<Event> {
        self.paused = None;
        self.collect_status(now);
        self.take_launch_report();
        let mut steps = 0;
        loop {
            let mut progressed = self.report_status(now);
            self.report_hang_ups(now);
            self.runner.tick(now);
            progressed |= self.settle(now);
            progressed |= self.write_input(&mut steps);
            progressed |= self.apply_requests(now);
            self.grant();
            if let Some(event) = self.events.pop_front() {
                return Some(event);
            }
            if std::mem::take(&mut self.execution_turn) && self.step_execution(now, buffer) {
                steps += 1;
                continue;
            }
            if let Some(message) = self.runner.next_message(buffer) {
                self.execution_turn = true;
                return Some(Event::Message(message));
            }
            if self.runner.is_finished() {
                return None;
            }
            if steps >= STEPS {
                self.paused = Some(now);
                return None;
            }
            if self.step_execution(now, buffer) {
                steps += 1;
                continue;
            }
            if !progressed {
                return None;
            }
        }
    }

    /// Returns the descriptors that move the execution on, each with what it waits on, which
    /// the caller polls for reading, as [`Execution::descriptors`] describes. An output stream is
    /// among them only while the runner lets the binding read it.
    pub fn descriptors(&self) -> impl Iterator<Item = (Source, BorrowedFd<'_>)> {
        let active = !self.runner.is_finished();
        self.execution.descriptors().filter(move |_| active)
    }

    /// Lends the writer of the program's input while the runner holds input for it, which the
    /// caller polls for writing.
    pub fn input_writer(&self) -> Option<BorrowedFd<'_>> {
        if self.runner.pending_input().is_empty() {
            return None;
        }
        self.execution.input()
    }

    /// Returns when the binding must take a step even if no descriptor polls ready: the earliest
    /// of the runner's deadline, the execution's, the next check of a settling cgroup, and the
    /// instant of the last call of [`next`](Self::next) if it stopped with steps left.
    pub fn deadline(&self) -> Option<Instant> {
        if self.runner.is_finished() {
            return None;
        }
        [
            self.runner.deadline(),
            self.execution.deadline(),
            self.settling.filter(|_| self.runner.awaits_settlement()),
            self.paused,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Returns whether the runner returned its terminal outcome.
    pub fn is_finished(&self) -> bool {
        self.runner.is_finished()
    }

    /// Applies the runner's requests to the execution, and returns whether there were any.
    fn apply_requests(&mut self, now: Instant) -> bool {
        let mut applied = false;
        while let Some(request) = self.runner.next_request() {
            applied = true;
            match request {
                runner::Request::Terminate(Cause::Cancelled) => {
                    self.execution.cancel();
                }
                runner::Request::Terminate(Cause::Disconnected) => self.execution.disconnect(),
                runner::Request::Terminate(_) => {
                    self.execution.terminate();
                }
                runner::Request::Discard(stream) => self.execution.discard(driver_stream(stream)),
                runner::Request::CloseInput => self.execution.close_input(),
                runner::Request::Settle => self.begin_settlement(now),
            }
        }
        applied
    }

    /// Writes the input that the runner holds while the program's pipe takes it, at most a chunk
    /// in each write, each of which counts as one of the call's `steps`, until they reach
    /// [`STEPS`], or one less while the execution's turn is due, so that a step is left for it;
    /// and tells the runner once the input closed: because the program closed its end, because
    /// the execution closed it as the program ended, or because a write failed, after which the
    /// binding closes it too, so that the program reads its end, and reports the failure. Returns
    /// whether it wrote any byte.
    fn write_input(&mut self, steps: &mut usize) -> bool {
        let chunk = self.runner.limits().chunk();
        let limit = STEPS - usize::from(self.execution_turn);
        let mut wrote = false;
        while *steps < limit {
            let pending = self.runner.pending_input();
            if pending.is_empty() {
                break;
            }
            let pending = &pending[..pending.len().min(chunk)];
            match self.execution.write_input(pending) {
                Ok(0) => break,
                Ok(length) => {
                    wrote = true;
                    *steps += 1;
                    self.runner.input_written(length);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    // The execution closes the input itself only once the program closed its
                    // end, which is no failure: the binding closes it after any other failure,
                    // and reports that.
                    self.execution.close_input();
                    self.runner.input_closed();
                    if error.kind() != io::ErrorKind::BrokenPipe {
                        self.events.push_back(Event::InputFailed(error));
                    }
                    break;
                }
            }
        }
        if self.execution.input().is_none() {
            self.runner.input_closed();
        }
        wrote
    }

    /// Grants the execution the reads that the runner allows beyond what it granted already.
    fn grant(&mut self) {
        for stream in Stream::ALL {
            let allowance = self.runner.read_allowance(stream);
            let granted = &mut self.granted[index(stream)];
            if allowance > *granted {
                self.execution
                    .grant(driver_stream(stream), allowance - *granted);
                *granted = allowance;
            }
        }
    }

    /// Takes the execution's next step at `now`, if it has one, feeds its event to the runner,
    /// and reports the program's end once its status is known. Returns whether it took a step or
    /// reported the end.
    fn step_execution(&mut self, now: Instant, buffer: &mut [u8]) -> bool {
        if self.ended {
            return false;
        }
        let stepped = match self.execution.next_event(buffer) {
            Ok(Some(event)) => {
                self.feed(event);
                true
            }
            Ok(None) => false,
            // Another collector took the program's status, or collecting it failed: the
            // program has ended, and its status is lost.
            Err(_) => self.report_exit(now, None),
        };
        let reported = self.report_status(now);
        stepped || reported
    }

    /// Feeds one event of the execution to the runner.
    fn feed(&mut self, event: DriverEvent<'_>) {
        match event {
            DriverEvent::Launch(report) => {
                self.launched = true;
                self.runner.launched(match report {
                    Ok(None) => Ok(()),
                    Ok(Some(failure)) => Err(LaunchFailure::new(
                        failure.stage().code(),
                        failure.raw_os_error(),
                    )),
                    Err(error) => Err(LaunchFailure::new(0, error.raw_os_error().unwrap_or(0))),
                });
            }
            DriverEvent::Output(stream, bytes) => {
                let stream = engine_stream(stream);
                let granted = &mut self.granted[index(stream)];
                *granted = granted.saturating_sub(bytes.len());
                self.runner.output(stream, bytes);
            }
            DriverEvent::Discarded(stream, length) => {
                self.runner.discarded(engine_stream(stream), length);
            }
            DriverEvent::Closed(stream) => self.runner.output_closed(engine_stream(stream)),
            DriverEvent::ReadFailed(stream, _) => self.runner.output_failed(engine_stream(stream)),
            DriverEvent::KillFailed(target, error) => {
                self.events.push_back(Event::KillFailed(target, error));
            }
            DriverEvent::Ended(_) => self.ended = true,
        }
    }

    /// Collects the program's status at `now` without blocking, if the execution collects it
    /// itself. A failed collection reports the program's end with its status lost, as a failed
    /// step does.
    fn collect_status(&mut self, now: Instant) {
        if self.execution.collect().is_err() {
            self.report_exit(now, None);
        }
    }

    /// Feeds the program's launch report to the runner once it has arrived, unless the binding
    /// did already.
    fn take_launch_report(&mut self) {
        if !self.launched
            && let Some(report) = self.execution.launch()
        {
            self.feed(DriverEvent::Launch(report));
        }
    }

    /// Reports the program's end at `now` to the runner once the execution has its status, unless
    /// the binding did already, and returns whether it reported the end now. The end does not
    /// wait for the launch report: a process that another thread forked while the program was
    /// spawning holds the report's writer until it replaces itself with its program or ends, and
    /// the runner's timeout would fire meanwhile. The runner holds the start, the output, and the
    /// terminal outcome until the report arrives, or until the settlement time and the grace have
    /// passed since the program's end, when it gives up on the report and fences the execution.
    fn report_status(&mut self, now: Instant) -> bool {
        match self.execution.status() {
            Some(status) => self.report_exit(now, Some(status)),
            None => false,
        }
    }

    /// Reports the program's end at `now` to the runner, with its status if known, unless it did
    /// already, and returns whether it reported it now.
    fn report_exit(&mut self, now: Instant, status: Option<ExitStatus>) -> bool {
        if self.exited {
            return false;
        }
        self.exited = true;
        let status = status.and_then(|status| {
            status
                .code()
                .map(Status::Exited)
                .or_else(|| status.signal().map(Status::Signaled))
        });
        self.runner.exited(now, status);
        true
    }

    /// Tells the runner which output streams hung up, once it knows of the program's end and one
    /// of its timers is due at `now`, before the timer fires: no process holds such a stream any
    /// more, so the runner never gives up on it, even if the execution does not settle.
    fn report_hang_ups(&mut self, now: Instant) {
        if !self.exited || self.runner.deadline().is_none_or(|at| at > now) {
            return;
        }
        for stream in Stream::ALL {
            let hung_up = &mut self.hung_up[index(stream)];
            if !*hung_up && self.execution.output_hung_up(driver_stream(stream)) {
                *hung_up = true;
                self.runner.output_hung_up(stream);
            }
        }
    }

    /// Begins to settle the execution at `now`: kills the processes of its cgroup, and checks
    /// the cgroup from then on. Without a cgroup, or if the kill fails, the settlement fails.
    fn begin_settlement(&mut self, now: Instant) {
        let Some((cgroup, scope)) = &self.execution.cgroup else {
            self.runner.settled(Settlement::Failed);
            return;
        };
        match cgroup.kill(*scope) {
            Ok(()) => self.settling = Some(now),
            Err(error) => {
                self.runner.settled(Settlement::Failed);
                self.events.push_back(Event::SettlementFailed(error));
            }
        }
    }

    /// Checks the settling cgroup at `now`, if a check is due: the execution settled once no
    /// process occupies the cgroup, and its settlement failed if the check does. Returns whether
    /// the settlement ended.
    fn settle(&mut self, now: Instant) -> bool {
        if !self.runner.awaits_settlement() {
            self.settling = None;
        }
        if self.settling.is_none_or(|due| now < due) {
            return false;
        }
        let Some((cgroup, scope)) = &self.execution.cgroup else {
            return false;
        };
        match cgroup.is_occupied(*scope) {
            Ok(false) => {
                self.settling = None;
                self.execution.release_cgroup();
                self.runner.settled(Settlement::Settled);
                true
            }
            Ok(true) => {
                self.settling = now.checked_add(SETTLE_INTERVAL);
                false
            }
            Err(error) => {
                self.settling = None;
                self.runner.settled(Settlement::Failed);
                self.events.push_back(Event::SettlementFailed(error));
                true
            }
        }
    }
}

/// Returns `stream`'s position in [`Stream::ALL`].
fn index(stream: Stream) -> usize {
    match stream {
        Stream::Stdout => 0,
        Stream::Stderr => 1,
    }
}

/// Returns the driver's stream of the engine's `stream`.
fn driver_stream(stream: Stream) -> DriverStream {
    match stream {
        Stream::Stdout => DriverStream::Stdout,
        Stream::Stderr => DriverStream::Stderr,
    }
}

/// Returns the engine's stream of the driver's `stream`.
fn engine_stream(stream: DriverStream) -> Stream {
    match stream {
        DriverStream::Stdout => Stream::Stdout,
        DriverStream::Stderr => Stream::Stderr,
    }
}

#[cfg(test)]
mod tests;
