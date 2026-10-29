//! The worker's executions, as the worker's module documentation describes them: the table that
//! reaches each one by its ID, the generations that the host started, their admission, the
//! refusal of a start, the loop's turns at every execution, the cgroup of each execution, through
//! which it settles, and which keeps its place until the worker removes it, and, without
//! cgroups, the collection of each program's status, before which the worker ends what is left
//! of the program's process group, and that of the statuses of the programs that the worker
//! forgot.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{self, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{self, WaitPidFlag, WaitStatus};
use nvx::exec::{Key, LaunchFailure, Runner, Settlement, Stream, Violation, runner};
use nvx::guest::cgroup::{ExecutionCgroup, Layout, Scope};
use nvx::guest::entry::{self, AccountError, Capabilities, Capability, Prepared, Publication};
use nvx::guest::exec::binding::{Binding, Event};
use nvx::guest::exec::{Execution, Options, Pipes, Source};
use nvx::guest::process::Child;
use nvx::guest::spawn::Command;

use super::connection::{BACKLOG, Connection};
use super::{Entrance, EntryOperations, MAX_EXECUTION_MEMORY, MAX_EXECUTIONS, MAX_HISTORY};
use crate::exec::{AccountMode, GuestMessage, HostMessage, Identity, MAX_CHUNK, Start, name};
use crate::guest::ServeError;

/// The device that a program without the host's input reads.
const NULL_DEVICE: &str = "/dev/null";

/// The interval at which the worker checks whether the programs that it forgot ended.
const REAP_INTERVAL: Duration = Duration::from_millis(10);

/// The longest that the worker waits for the programs of the executions that it drops at once.
const REAP_WAIT: Duration = Duration::from_secs(1);

/// The most bytes of output that an execution that the host reaches reads in one turn of the
/// loop, give or take a chunk: an equal share of the [`BACKLOG`] among the most executions that
/// the worker runs, so that no execution takes the room on the stream to the host from the
/// others.
const SHARE: usize = BACKLOG / MAX_EXECUTIONS;

/// The worker's executions.
pub(super) struct Executions {
    /// The executions that the host reaches, by ID.
    attached: BTreeMap<u64, Run>,
    /// The executions that the host abandoned, which run until they end.
    detached: Vec<Run>,
    /// The process IDs of the programs that the worker forgot before it collected their
    /// statuses, which it collects once they ended.
    reaping: Vec<u32>,
    /// When the worker last checked whether those programs ended.
    reaped: Option<Instant>,
    /// When the worker stops waiting for the programs once it dropped every execution: a while
    /// after it first did.
    cleared: Option<Instant>,
    /// The cgroups in which the executions run, once the root is ready: the layout below whose
    /// workload cgroup each execution gets a cgroup of its own. Without them, each program runs
    /// in a process group alone.
    cgroups: Option<Layout>,
    /// The sandbox in which the executions run.
    sandbox: Sandbox,
    /// How many cgroups of executions the worker prepared, which numbers the next one.
    prepared: u64,
    /// The cgroups of the executions that did not settle, each of which keeps its place among
    /// [`MAX_EXECUTIONS`] until the worker drops every execution.
    fenced: Vec<ExecutionCgroup>,
    /// The ID from which the next turn serves the executions that the host reaches, before
    /// those of lower IDs: the ID after that of the execution that the last turn served first,
    /// so that the executions take turns at being served first.
    turn: u64,
    /// The instant of the last turn if an execution read its whole share of output in it, and
    /// may have more to read: the executions' deadline, which the worker's next wait finds
    /// passed already.
    owed: Option<Instant>,
    /// The latest generation that the host started of each ID of `history`.
    generations: BTreeMap<u64, u64>,
    /// The IDs whose latest generation the worker remembers, in the order in which it heard of
    /// them first.
    history: VecDeque<u64>,
    /// The buffer into which each read of an output stream goes: a chunk at most.
    buffer: Vec<u8>,
}

/// The sandbox in which the executions run once the root is ready: what every execution's
/// [`Entrance`] shares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Sandbox {
    /// The executions' hostname.
    pub(super) hostname: Vec<u8>,
    /// The sandbox's workload identity, its user ID and group ID, if the root configuration
    /// names one, as which every execution runs.
    pub(super) identity: Option<(u32, u32)>,
    /// Whether the sandbox's listeners are published, as the root configuration says.
    pub(super) publication: Publication,
}

impl Sandbox {
    /// Returns the sandbox whose workload identity is `identity`, if it has one, and whose
    /// listeners are published as `publication` says. Its executions take the entry's default
    /// hostname, [`DEFAULT_HOSTNAME`](entry::DEFAULT_HOSTNAME), since a root configuration names
    /// no other, unless `with_hostname` names theirs, as a clone's record does.
    pub(super) fn new(identity: Option<(u32, u32)>, publication: Publication) -> Self {
        Self {
            hostname: entry::DEFAULT_HOSTNAME.as_bytes().to_vec(),
            identity,
            publication,
        }
    }

    /// Gives the sandbox's executions the hostname `hostname`, which a clone's record names.
    #[cfg(target_arch = "x86_64")]
    pub(super) fn with_hostname(mut self, hostname: Vec<u8>) -> Self {
        self.hostname = hostname;
        self
    }
}

/// One of the worker's executions: the binding of its runner to its program, and either the
/// execution's cgroup, through which it settles, or, without cgroups, the worker's own handle of
/// the program, whose status the binding leaves to the worker.
struct Run {
    /// The binding of the execution's runner to its program.
    binding: Binding,
    /// Without cgroups, the program, through a pidfd of the worker's own, which polls readable
    /// once the program ended, until the worker collected its status.
    program: Option<Child>,
    /// The execution's cgroup, if the worker runs it in one.
    cgroup: Option<ExecutionCgroup>,
    /// Whether the runner's terminal outcome tells that the execution settled, so that its
    /// cgroup is empty, and the worker may remove it.
    settled: bool,
}

impl Executions {
    /// Returns no executions, which run in process groups alone, with the default hostname,
    /// without a workload identity, and with published listeners, until
    /// [`run_in`](Self::run_in) names their cgroups and their sandbox.
    pub(super) fn new() -> Self {
        Self {
            attached: BTreeMap::new(),
            detached: Vec::new(),
            reaping: Vec::new(),
            reaped: None,
            cleared: None,
            cgroups: None,
            sandbox: Sandbox::new(None, Publication::Published),
            prepared: 0,
            fenced: Vec::new(),
            turn: 0,
            owed: None,
            generations: BTreeMap::new(),
            history: VecDeque::new(),
            buffer: vec![0; MAX_CHUNK as usize],
        }
    }

    /// Runs the executions that start from now on in `cgroups`, and in `sandbox`: each in a
    /// cgroup of its own, below the workload cgroup, or, without cgroups, in a process group
    /// alone.
    pub(super) fn run_in(&mut self, cgroups: Option<Layout>, sandbox: Sandbox) {
        self.cgroups = cgroups;
        self.sandbox = sandbox;
    }

    /// Hands `message` from the host to its execution at `now`, and queues on `connection` the
    /// terminal record of a start that the worker refuses. A start's program enters the root
    /// through `operations`. Before a cancellation, it gives the execution a turn at `now`, and
    /// queues its messages on `connection`: the end of a program that ended meanwhile, even
    /// before its timeout, while the worker did not run, reaches the runner first, so that it
    /// stays the program's own, without the cause of the cancellation, or of a timeout that
    /// `now` reached.
    pub(super) fn deliver<S: Read + Write + AsFd>(
        &mut self,
        now: Instant,
        message: HostMessage<'_>,
        operations: &mut dyn EntryOperations,
        connection: &mut Connection<S>,
    ) -> Result<(), ServeError> {
        let key = message.key();
        if let HostMessage::Start(start) = message {
            return self.start(now, start, operations, connection);
        }
        // A message of an execution that the worker no longer runs is late.
        let Self {
            attached, buffer, ..
        } = self;
        let Some(run) = attached.get_mut(&key.id()) else {
            return Ok(());
        };
        if matches!(message, HostMessage::Cancel(..)) {
            run.collect();
            run.serve(now, connection, buffer)?;
        }
        let binding = &mut run.binding;
        let applied = match message {
            HostMessage::Input(key, bytes) => binding.input(key, bytes),
            HostMessage::InputEnd(key) => binding.end_input(key),
            HostMessage::Cancel(key, cause) => binding.cancel(now, key, cause),
            HostMessage::Credit(key, pool, units) => binding.credit(key, pool, units),
            HostMessage::Start(_) => unreachable!("a start is handled above"),
        };
        applied
            .map(|_receipt| ())
            .map_err(|violation| refused(key, violation))
    }

    /// Takes a turn at `now`: collects the status of each program that ended, takes every step
    /// that the executions can take, queues their messages for the host on `connection`,
    /// forgets each execution once it returned its terminal outcome, which removes the cgroup of
    /// one that settled, and collects the statuses of the programs that it forgot, of those that
    /// ended.
    ///
    /// The executions that the host reaches take turns at being served first: a turn starts with
    /// the execution after the one that the last turn started with, in the order of their IDs,
    /// and goes on with the others in that order, starting over from the lowest ID. Each reads at
    /// most [`SHARE`] bytes of output in a turn, and none while the connection is backlogged,
    /// and then returns only its other messages. So, however fast a program writes, and however
    /// much credit the host grants it, the other executions get to the stream in their turns.
    pub(super) fn run<S: Read + Write + AsFd>(
        &mut self,
        now: Instant,
        connection: &mut Connection<S>,
    ) -> Result<(), ServeError> {
        for run in self.attached.values_mut().chain(&mut self.detached) {
            run.collect();
        }
        let Self {
            attached,
            detached,
            turn,
            owed,
            buffer,
            ..
        } = self;
        let first = attached
            .range(*turn..)
            .chain(attached.range(..*turn))
            .next()
            .map(|(&id, _)| id);
        let mut more = false;
        for (_, run) in attached.range_mut(*turn..) {
            more |= run.serve(now, connection, buffer)?;
        }
        for (_, run) in attached.range_mut(..*turn) {
            more |= run.serve(now, connection, buffer)?;
        }
        if let Some(first) = first {
            *turn = first.wrapping_add(1);
        }
        *owed = more.then_some(now);
        for run in detached.iter_mut() {
            let key = run.binding.runner().key();
            while let Some(event) = run.binding.next(now, buffer) {
                match event {
                    // A detached runner returns only its terminal outcome, which no host takes.
                    Event::Message(runner::Message::Terminal(outcome)) => {
                        run.settled = outcome.is_reusable();
                    }
                    Event::Message(_) => {}
                    event => report(key, &event),
                }
            }
        }
        let finished = attached
            .iter()
            .filter(|(_, run)| run.binding.is_finished())
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        let mut forgotten = finished
            .into_iter()
            .filter_map(|id| attached.remove(&id))
            .collect::<Vec<_>>();
        forgotten.extend(detached.extract_if(.., |run| run.binding.is_finished()));
        for run in forgotten {
            self.forget(run);
        }
        self.reap(now);
        Ok(())
    }

    /// Abandons every execution at `now`, because the session was reset.
    pub(super) fn reset(&mut self, now: Instant) {
        for (_, mut run) in std::mem::take(&mut self.attached) {
            run.binding.reset(now);
            self.detached.push(run);
        }
    }

    /// Returns whether the worker runs no execution: none that the host reaches, and none that it
    /// abandoned. It may still collect the statuses of programs that it forgot, and hold the
    /// cgroups of executions that did not settle.
    pub(super) fn is_empty(&self) -> bool {
        self.attached.is_empty() && self.detached.is_empty()
    }

    /// Drops every execution, which ends the program and its process group, or the processes of
    /// its cgroup, of each one that has not ended, and waits until the programs ended and their
    /// cgroups emptied, so that the worker collects the programs' statuses and removes the
    /// cgroups, those of the executions that did not settle included, whose processes it ends
    /// too, at most [`REAP_WAIT`] after it first dropped every execution: a later call, as once
    /// the executions drop after a stop, waits no longer. It removes the cgroups once it
    /// collected the status of every program that it forgot: until then, such a program may
    /// still join its cgroup, as its launch may not have reported, and a removal could hide it
    /// once it ended. Without cgroups, the worker ended the process groups of the other programs
    /// when it collected their statuses.
    pub(super) fn clear(&mut self) {
        let runs = std::mem::take(&mut self.attached)
            .into_values()
            .chain(self.detached.drain(..))
            .collect::<Vec<_>>();
        for run in runs {
            let Run {
                binding, cgroup, ..
            } = run;
            if binding.execution().status().is_none() {
                self.reaping.push(binding.id());
            }
            self.fenced.extend(cgroup);
        }
        // An execution that settled gave its cgroup back, which its drop then leaves alone.
        for cgroup in &self.fenced {
            if let Err(error) = cgroup.kill(Scope::Execution) {
                eprintln!(
                    "runtime-adapter-guest: cannot end the processes of {}: {error}",
                    cgroup.path().display()
                );
            }
        }
        let deadline = *self
            .cleared
            .get_or_insert_with(|| Instant::now() + REAP_WAIT);
        loop {
            let now = Instant::now();
            self.reap(now);
            if self.reaping.is_empty() {
                self.fenced.retain(|cgroup| !removed(cgroup));
            }
            if (self.reaping.is_empty() && self.fenced.is_empty()) || now >= deadline {
                return;
            }
            thread::sleep(REAP_INTERVAL);
        }
    }

    /// Returns when an execution must take a step even if none of its descriptors polls ready,
    /// when the worker checks next whether the programs that it forgot ended, or, if an
    /// execution read its whole share of output in the last turn, that turn's instant, so that
    /// the next turn comes at once.
    pub(super) fn deadline(&self) -> Option<Instant> {
        let reaping = self
            .reaped
            .filter(|_| !self.reaping.is_empty())
            .map(|reaped| reaped + REAP_INTERVAL);
        self.runs()
            .filter_map(|run| run.binding.deadline())
            .chain(reaping)
            .chain(self.owed)
            .min()
    }

    /// Appends the executions' descriptors to `descriptors`, each with the readiness that it
    /// waits for, but those of the output of the executions that the host reaches while the
    /// connection is `backlogged`, as they take no output then, and the pidfd of each program
    /// whose status the worker has not collected, which polls readable once the program ended.
    pub(super) fn poll_descriptors<'a>(
        &'a self,
        descriptors: &mut Vec<PollFd<'a>>,
        backlogged: bool,
    ) {
        let attached = self.attached.values().map(|run| (run, backlogged));
        let detached = self.detached.iter().map(|run| (run, false));
        for (run, backlogged) in attached.chain(detached) {
            for (source, descriptor) in run.binding.descriptors() {
                if !(backlogged && matches!(source, Source::Output(_))) {
                    descriptors.push(PollFd::new(descriptor, PollFlags::POLLIN));
                }
            }
            if let Some(descriptor) = run.binding.input_writer() {
                descriptors.push(PollFd::new(descriptor, PollFlags::POLLOUT));
            }
            if let Some(pidfd) = run.program.as_ref().and_then(Child::pidfd) {
                descriptors.push(PollFd::new(pidfd, PollFlags::POLLIN));
            }
        }
    }

    /// Returns every execution.
    fn runs(&self) -> impl Iterator<Item = &Run> {
        self.attached.values().chain(&self.detached)
    }

    /// Returns how many of the places of [`MAX_EXECUTIONS`] the executions take: one for each
    /// execution that the worker runs, and, with cgroups, one for each cgroup of an execution that
    /// did not settle, or, without them, one for each program that the worker forgot before it
    /// collected its status.
    fn places(&self) -> usize {
        let held = if self.cgroups.is_some() {
            self.fenced.len()
        } else {
            self.reaping.len()
        };
        self.attached.len() + self.detached.len() + held
    }

    /// Forgets `run`, whose runner returned its terminal outcome. Unless the worker collected
    /// the program's status, the program's ID goes to `reaping`, so that the worker collects the
    /// status once the program ended. The worker removes the cgroup of an execution that settled,
    /// which frees its place, and keeps that of one that did not, or that it cannot remove, which
    /// keeps its place until the worker drops every execution. The binding drops last, which ends
    /// a program that has not ended, and its process group, or the processes of its cgroup unless
    /// the execution settled: one that settled gave its cgroup back, which its drop leaves alone.
    fn forget(&mut self, run: Run) {
        let Run {
            binding,
            cgroup,
            settled,
            ..
        } = run;
        if binding.execution().status().is_none() {
            self.reaping.push(binding.id());
        }
        let Some(cgroup) = cgroup else {
            return;
        };
        if settled {
            match cgroup.remove() {
                Ok(()) => return,
                Err(error) => eprintln!(
                    "runtime-adapter-guest: execution {}: cannot remove its cgroup: {error}",
                    name(binding.runner().key())
                ),
            }
        }
        self.fenced.push(cgroup);
    }

    /// Collects at `now` the statuses of the programs that the worker forgot, of those that
    /// ended.
    fn reap(&mut self, now: Instant) {
        self.reaping.retain(|&id| {
            // Any other result tells that no status is left to collect: `ECHILD`, or a status
            // that `nix` collected, and then could not decode, as that of a real-time signal.
            matches!(
                wait::waitpid(super::pid(id), Some(WaitPidFlag::WNOHANG)),
                Ok(WaitStatus::StillAlive) | Err(Errno::EINTR)
            )
        });
        self.reaped = Some(now);
    }

    /// Starts the execution of `start` at `now`, whose program enters the root through
    /// `operations`, or refuses it with a terminal record that it queues on `connection`. A start
    /// of an earlier generation than the latest that the host started of its ID is stale, and one
    /// of the same generation fails.
    fn start<S: Read + Write + AsFd>(
        &mut self,
        now: Instant,
        start: Start,
        operations: &mut dyn EntryOperations,
        connection: &mut Connection<S>,
    ) -> Result<(), ServeError> {
        let key = start.key;
        // The running execution of the ID, if any, holds the latest generation too, in case the
        // worker forgot the ID's generations meanwhile.
        let running = self
            .attached
            .get(&key.id())
            .map(|run| run.binding.runner().key().generation());
        if let Some(latest) = self.generations.get(&key.id()).copied().max(running) {
            if key.generation() < latest {
                return Ok(());
            }
            if key.generation() == latest {
                return Err(ServeError::Execution(format!(
                    "execution {} starts twice",
                    name(key)
                )));
            }
        }
        self.remember(key);
        if let Some(mut abandoned) = self.attached.remove(&key.id()) {
            abandoned.binding.dropped(now);
            self.detached.push(abandoned);
        }
        let runner = Runner::new(key, start.limits, start.timers, now);
        let admitted = if self.places() >= MAX_EXECUTIONS {
            Err(Errno::EAGAIN as i32)
        } else if runner.memory_bound() > MAX_EXECUTION_MEMORY {
            Err(Errno::ENOMEM as i32)
        } else {
            self.launch(&start, operations)
                .map_err(|error| error.raw_os_error().unwrap_or(0))
        };
        match admitted {
            Ok((execution, program, cgroup)) => {
                let binding = Binding::new(execution, runner);
                let run = Run {
                    binding,
                    program,
                    cgroup,
                    settled: false,
                };
                self.attached.insert(key.id(), run);
                Ok(())
            }
            Err(error) => refuse(now, runner, error, connection),
        }
    }

    /// Starts the program of `start`, which enters the root through `operations` as its
    /// [entrance](Self::entrance) describes, once the entry prepared its execution: with cgroups,
    /// in a cgroup of its own that the worker prepares below the workload cgroup, and otherwise
    /// with the worker's own handle of the program. A start whose entry the root's entry refuses
    /// fails with the `errno` that [`refusal`] returns, before the worker prepares a cgroup. The
    /// worker removes the cgroup of a program that it could not spawn. One that it spawned, but
    /// whose execution could not be had, may still join the cgroup until the worker collected its
    /// status, so that a removal could hide it once it ended: its cgroup keeps its place, as that
    /// of an execution that did not settle.
    fn launch(
        &mut self,
        start: &Start,
        operations: &mut dyn EntryOperations,
    ) -> io::Result<(Execution, Option<Child>, Option<ExecutionCgroup>)> {
        let entrance = self.entrance(start)?;
        let prepared = operations
            .prepare(&entrance)
            .map_err(|error| io::Error::from(refusal(&error)))?;
        let Some(cgroups) = &self.cgroups else {
            let (execution, program) = spawn(start, prepared, None, operations, &mut self.reaping)
                .map_err(|unstarted| unstarted.error)?;
            return Ok((execution, program, None));
        };
        self.prepared += 1;
        let name = format!("execution-{}", self.prepared);
        let cgroup = ExecutionCgroup::prepare(cgroups, Some(&name), &[])?;
        match spawn(
            start,
            prepared,
            Some(&cgroup),
            operations,
            &mut self.reaping,
        ) {
            Ok((execution, program)) => Ok((execution, program, Some(cgroup))),
            Err(Unstarted { error, spawned }) => {
                if spawned || cgroup.remove().is_err() {
                    self.fenced.push(cgroup);
                }
                Err(error)
            }
        }
    }

    /// Returns how the program of `start` enters the root: with the sandbox's hostname and its
    /// publication of listeners, as the identity that exactly one source names, the sandbox's
    /// workload identity, if it has one, or otherwise the identity that the start names. A start
    /// that names an identity in a sandbox with a workload identity, or none in a sandbox without
    /// one, is `EINVAL`.
    fn entrance(&self, start: &Start) -> io::Result<Entrance> {
        let identity = match (self.sandbox.identity, &start.identity) {
            (Some((uid, gid)), None) => entry::Identity::new(uid, gid),
            (None, Some(identity)) => entry_identity(identity),
            _ => return Err(Errno::EINVAL.into()),
        };
        Ok(Entrance {
            identity,
            hostname: self.sandbox.hostname.clone(),
            publication: self.sandbox.publication,
        })
    }

    /// Remembers that the host started `key`, the latest generation of its ID, and forgets the
    /// ID that it heard of first once it remembers more than [`MAX_HISTORY`].
    fn remember(&mut self, key: Key) {
        if self
            .generations
            .insert(key.id(), key.generation())
            .is_none()
        {
            self.history.push_back(key.id());
        }
        if self.history.len() > MAX_HISTORY
            && let Some(first) = self.history.pop_front()
        {
            self.generations.remove(&first);
        }
    }
}

impl Drop for Executions {
    /// Drops every execution, and waits a little for their programs, as
    /// [`clear`](Executions::clear) does.
    fn drop(&mut self) {
        self.clear();
    }
}

impl Run {
    /// Collects the program's status without blocking once the program ended, and hands it to
    /// the binding, if the worker collects it: without cgroups. First, it kills what is left of
    /// the process group that the program leads, as the binding's settlement through a cgroup
    /// kills what is left in the cgroup, so that no descendant that stayed in the group outlives
    /// the program, or holds its output open: until its status is collected, the program, a
    /// zombie, keeps its process ID, which is the group's, from any other process, so that the
    /// kill reaches that group alone.
    fn collect(&mut self) {
        let Self {
            binding, program, ..
        } = self;
        let Some(child) = program else {
            return;
        };
        let Some(pidfd) = child.pidfd() else {
            return;
        };
        // The pidfd polls readable once the program ended, while its status waits to be
        // collected. A wait with `WNOWAIT` would tell that too, but `nix` decodes the status that
        // it reports, which fails for a real-time signal, while the handle's own collection
        // keeps every signal.
        let ended = {
            let mut descriptors = [PollFd::new(pidfd, PollFlags::POLLIN)];
            poll::poll(&mut descriptors, PollTimeout::ZERO).map(|_| {
                descriptors[0]
                    .revents()
                    .is_some_and(|events| events.contains(PollFlags::POLLIN))
            })
        };
        match ended {
            Ok(true) => {}
            Ok(false) | Err(Errno::EINTR) => return,
            Err(error) => {
                // Only the worker collects its programs' statuses, so no wait fails, but the
                // execution's deadlines bound one that does: the worker no longer waits for its
                // program then.
                lost(binding, &error.into());
                *program = None;
                return;
            }
        }
        let id = child.id();
        if let Err(error) = signal::killpg(super::pid(id), Signal::SIGKILL)
            && error != Errno::ESRCH
        {
            eprintln!(
                "runtime-adapter-guest: execution {}: cannot kill its program's process group: \
                 {error}",
                name(binding.runner().key())
            );
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                binding.observe(id, status);
                *program = None;
            }
            Ok(None) => {}
            Err(error) => {
                lost(binding, &error);
                *program = None;
            }
        }
    }

    /// Serves the execution in a turn at `now`: takes every step that it can take, and queues its
    /// messages for the host on `connection`, and notes whether its terminal outcome tells that
    /// it settled. Each read of its program's output goes into `buffer`, until it read [`SHARE`]
    /// bytes in the turn, or while the connection is backlogged: an empty buffer then lets it
    /// read no output, and return none. Returns whether it read its whole share, and so may have
    /// more output to read in the next turn.
    fn serve<S: Read + Write + AsFd>(
        &mut self,
        now: Instant,
        connection: &mut Connection<S>,
        buffer: &mut [u8],
    ) -> Result<bool, ServeError> {
        let key = self.binding.runner().key();
        let mut read = 0;
        loop {
            let length = if read >= SHARE || connection.is_backlogged() {
                0
            } else {
                buffer.len()
            };
            let lent = &mut buffer[..length];
            let Some(event) = self.binding.next(now, lent) else {
                break;
            };
            match event {
                Event::Message(message) => {
                    match message {
                        runner::Message::Output { length: bytes, .. } => read += bytes,
                        runner::Message::Terminal(outcome) => self.settled = outcome.is_reusable(),
                        _ => {}
                    }
                    if let Some(message) = GuestMessage::from_runner(key, message, lent) {
                        connection.send(&message.encode())?;
                    }
                }
                event => report(key, &event),
            }
        }
        Ok(read >= SHARE)
    }
}

/// Why the worker could not start a program.
struct Unstarted {
    /// The error.
    error: io::Error,
    /// Whether the worker spawned the program before the error.
    spawned: bool,
}

impl From<io::Error> for Unstarted {
    /// Returns the failure of a program that the worker did not spawn.
    fn from(error: io::Error) -> Self {
        Self {
            error,
            spawned: false,
        }
    }
}

/// Spawns the program of `start` with pipes, in a process group of its own, and in `cgroup`, if
/// any, through `operations`, whose spawn enters the root as `prepared` prepared it, which makes
/// the program lead a session of its own, and returns its execution. The program starts in the
/// start's working directory, or the root's `/`, with exactly the start's environment. In a
/// cgroup, the execution settles through the cgroup, and collects the program's status itself.
/// Otherwise, it leaves the status to the worker, and comes with the worker's own handle of the
/// program. If the program started, but the execution or the handle could not be had, the
/// program ends, with its process group or the processes of its cgroup, and the program's ID goes
/// to `reaping`, so that the worker collects its status once it ended, without waiting for it;
/// the failure then tells that the worker spawned the program.
fn spawn(
    start: &Start,
    prepared: Prepared,
    cgroup: Option<&ExecutionCgroup>,
    operations: &mut dyn EntryOperations,
    reaping: &mut Vec<u32>,
) -> Result<(Execution, Option<Child>), Unstarted> {
    let strings = |items: &[Vec<u8>]| {
        items
            .iter()
            .map(|item| CString::new(item.as_slice()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::from)
    };
    let arguments = strings(&start.arguments)?;
    let environment = strings(&start.environment)?;
    let program = arguments.first().cloned().unwrap_or_default();
    let (pipes, ends) = Pipes::new(start.input)?;
    let stdin = match ends.stdin {
        Some(stdin) => stdin,
        None => OwnedFd::from(File::open(NULL_DEVICE)?),
    };
    let mut command = Command::new(program, arguments)
        .stdin(stdin)
        .stdout(ends.stdout)
        .stderr(ends.stderr)
        .new_process_group()
        .environment(environment);
    if let Some(directory) = &start.working_directory {
        let directory = CString::new(directory.as_slice()).map_err(io::Error::from)?;
        command = command.working_directory(directory);
    }
    if let Some(cgroup) = cgroup {
        command = command.cgroup(cgroup.placement()?);
    }
    let spawned = operations.spawn(command, prepared)?;
    let id = spawned.id();
    let options = match cgroup {
        Some(cgroup) => Options::new().cgroup(cgroup, Scope::Execution),
        None => Options::new().process_group().caller_reaps(),
    };
    let unstarted = |error| Unstarted {
        error,
        spawned: true,
    };
    let execution = Execution::new(spawned, pipes, options)
        .inspect_err(|error| {
            // The execution ended the program, and the program's status is still the worker's,
            // unless `ESRCH` tells that it is gone already.
            if error.raw_os_error() != Some(Errno::ESRCH as i32) {
                reaping.push(id);
            }
        })
        .map_err(unstarted)?;
    if cgroup.is_some() {
        return Ok((execution, None));
    }
    // Nothing collected the program's status, so its ID still names it. Dropping the execution
    // after a failure ends the program and its process group.
    let program = Child::with_pidfd(id)
        .inspect_err(|_| reaping.push(id))
        .map_err(unstarted)?;
    Ok((execution, Some(program)))
}

/// Removes the cgroup `cgroup` of an execution that the worker dropped, and returns whether it is
/// gone, or false while a process, a zombie included, is left in it. A removal that fails
/// otherwise is reported, and given up.
fn removed(cgroup: &ExecutionCgroup) -> bool {
    match cgroup.remove() {
        Ok(()) => true,
        Err(error) if error.raw_os_error() == Some(Errno::EBUSY as i32) => false,
        Err(error) => {
            eprintln!(
                "runtime-adapter-guest: cannot remove {}: {error}",
                cgroup.path().display()
            );
            true
        }
    }
}

/// Refuses the execution of `runner` at `now`, whose launch failed at stage 0 with `error`: queues
/// its messages on `connection`, its terminal record last.
fn refuse<S: Read + Write + AsFd>(
    now: Instant,
    mut runner: Runner,
    error: i32,
    connection: &mut Connection<S>,
) -> Result<(), ServeError> {
    runner.launched(Err(LaunchFailure::new(0, error)));
    for stream in Stream::ALL {
        runner.output_closed(stream);
    }
    runner.exited(now, None);
    while let Some(request) = runner.next_request() {
        if let runner::Request::Settle = request {
            runner.settled(Settlement::Failed);
        }
    }
    let key = runner.key();
    while let Some(message) = runner.next_message(&mut []) {
        if let Some(message) = GuestMessage::from_runner(key, message, &[]) {
            connection.send(&message.encode())?;
        }
    }
    Ok(())
}

/// Returns the failure of the host's message about the execution `key` that its runner refused.
fn refused(key: Key, violation: Violation) -> ServeError {
    ServeError::Execution(format!("execution {}: {violation}", name(key)))
}

/// Returns the `errno` with which the worker refuses a start whose entry `error` refused, at
/// stage 0: `EPERM` for an identity that the entry does not permit, with a capability outside
/// its allowlist, with any capability but user ID 0's, or with `CAP_NET_BIND_SERVICE` or
/// `CAP_NET_RAW` while listeners are published; `ENOENT` for an identity whose account the root
/// lacks in existing-account mode, without the user ID's passwd entry or the group ID's group
/// entry; the error's own for an account database or a root that cannot be read; and `EINVAL`
/// otherwise.
fn refusal(error: &entry::Error) -> Errno {
    match error {
        entry::Error::Capability(_)
        | entry::Error::NonRootCapabilities
        | entry::Error::Published(_) => Errno::EPERM,
        entry::Error::Account(AccountError::Missing | AccountError::Group) => Errno::ENOENT,
        entry::Error::Account(AccountError::Read(_, error)) | entry::Error::Root(error) => {
            error.raw_os_error().map_or(Errno::EIO, Errno::from_raw)
        }
        _ => Errno::EINVAL,
    }
}

/// Returns the identity of `nvx::guest::entry` that a start's `identity` names.
fn entry_identity(identity: &Identity) -> entry::Identity {
    let capabilities = (0..64)
        .filter(|number| identity.capabilities & 1 << number != 0)
        .filter_map(Capability::new)
        .collect::<Capabilities>();
    let account = match identity.account {
        AccountMode::Existing => entry::AccountMode::Existing,
        AccountMode::Numeric => entry::AccountMode::Numeric,
    };
    entry::Identity::new(identity.uid, identity.gid)
        .groups(identity.groups.clone())
        .capabilities(capabilities)
        .account(account)
}

/// Reports that the worker cannot collect the status of the program of `binding`'s execution,
/// with `error`: the worker no longer waits for the program, and the runner's deadlines bound
/// the execution.
fn lost(binding: &Binding, error: &io::Error) {
    eprintln!(
        "runtime-adapter-guest: execution {}: cannot collect its program's status: {error}",
        name(binding.runner().key())
    );
}

/// Reports a failure of the execution `key` that its binding reported, which ends nothing: the
/// runner's deadlines bound what the failure leaves.
fn report(key: Key, event: &Event) {
    match event {
        Event::KillFailed(target, error) => {
            eprintln!(
                "runtime-adapter-guest: execution {}: cannot kill its {target:?}: {error}",
                name(key)
            );
        }
        Event::SettlementFailed(error) => {
            eprintln!(
                "runtime-adapter-guest: execution {}: cannot settle it: {error}",
                name(key)
            );
        }
        Event::InputFailed(error) => {
            eprintln!(
                "runtime-adapter-guest: execution {}: cannot write its input: {error}",
                name(key)
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
