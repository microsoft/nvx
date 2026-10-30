//! Checks of bound executions of real programs, each with a peer in the same process: streams
//! beyond their windows, input beyond its window, an input that the program closes early, one
//! that closes with its last byte while output waits, and one whose write fails, a program that
//! the caller started itself, a launch stage that fails, which the peer learns by its number,
//! settlement before reuse and the fences of failed cleanups, the bounded wait for output that a
//! descendant holds once an execution does not settle, due settlement checks while output drains
//! slowly, statuses that the execution or the caller collects, which reach the runner ahead of
//! queued output and of its timeout, even in a first call after it, and while the launch report
//! is late, a signal that the program handles, which reaches the runner only as the program's
//! end, the bounded wait for a launch report that a process holds for good, the bounds on the
//! steps and on the writes of one call, output that flows while the caller hands over input
//! before every call, and two executions in one loop, one of them with a stalled sink or a pipe
//! writer that escaped its kill. The loop waits only for what the bindings and peers say that
//! they wait on, so a step that nothing leads to stalls a check rather than passing it.

use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout};
use nix::sys::socket::{self, AddressFamily, SockFlag, SockType, sockopt};

use super::*;
use crate::exec::{Limits, Outcome, Peer, Timers, Window, peer};
use crate::guest::cgroup::{ExecutionCgroup, Layout, Scope};
use crate::guest::exec::{Options, Pipes};
use crate::guest::process::Child;
use crate::guest::spawn::{Command, LaunchStage};
use crate::guest::sys::{self, ProcessId};

/// How long a test waits for its executions before it fails.
const PATIENCE: Duration = Duration::from_secs(30);
/// The grace of pending output in these tests.
const GRACE: Duration = Duration::from_millis(300);
/// The settlement time in these tests.
const SETTLEMENT: Duration = Duration::from_secs(10);
/// The key of each execution in these tests.
const KEY: Key = Key::new(1, 1);
/// The longest that a deadline may be overrun while the test's loop serves other executions.
const SLACK: Duration = Duration::from_secs(2);

/// Returns limits with chunks of 16 KiB and byte windows of `window` bytes.
fn limits(window: u32) -> Limits {
    Limits::new(16 * 1024, Window::bytes(window), Window::bytes(window)).unwrap()
}

/// Returns the timers of these tests.
fn timers() -> Timers {
    Timers::new(GRACE, SETTLEMENT)
}

/// Returns a command that runs `/bin/sh -c script`, with `$0` set to `sh`.
fn shell(script: &str) -> Command {
    Command::new(
        c"/bin/sh".to_owned(),
        vec![
            c"sh".to_owned(),
            c"-c".to_owned(),
            CString::new(script).unwrap(),
        ],
    )
}

/// Returns `seq 1 count`'s output.
fn sequence(count: usize) -> Vec<u8> {
    (1..=count)
        .map(|number| format!("{number}\n"))
        .collect::<String>()
        .into_bytes()
}

/// Returns a fake cgroup in `root`: a workload cgroup whose `cgroup.kill` records the kills,
/// and whose `pids.current` says `pids`. A test plays the kernel's part.
fn fake_cgroup(root: &Path, pids: &str) -> ExecutionCgroup {
    let layout = Layout::new(root.join("agent"), root.join("workload")).unwrap();
    std::fs::create_dir_all(layout.workload()).unwrap();
    std::fs::write(layout.workload().join("cgroup.kill"), b"").unwrap();
    std::fs::write(layout.workload().join("pids.current"), pids).unwrap();
    ExecutionCgroup::open(&layout, None).unwrap()
}

/// One bound execution and its peer, connected directly, with the peer's sink and source.
struct Side {
    /// The runner's side: the binding of the execution.
    binding: Binding,
    /// The peer.
    peer: Peer,
    /// The bytes of each stream that the sink took.
    output: [Vec<u8>; 2],
    /// Whether the sink takes nothing.
    stalled: bool,
    /// The input that the source hands over.
    input: Vec<u8>,
    /// The bytes of input that the source handed over.
    sent: usize,
    /// The peer's events.
    events: Vec<peer::Event>,
    /// The runner's terminal outcome, once it returned it.
    terminal: Option<Outcome>,
    /// The failures that the binding reported.
    failures: Vec<String>,
}

impl Side {
    /// Spawns `command` with pipes, its standard input piped if `input` and the null device
    /// otherwise, in a process group of its own, and binds a runner with `limits` and `timers`
    /// to its execution with `options`, with a peer of its own.
    fn start(
        command: Command,
        input: bool,
        options: Options,
        limits: Limits,
        timers: Timers,
    ) -> Self {
        let (pipes, ends) = Pipes::new(input).unwrap();
        let stdin = match ends.stdin {
            Some(stdin) => stdin,
            None => OwnedFd::from(std::fs::File::open("/dev/null").unwrap()),
        };
        let spawned = command
            .stdin(stdin)
            .stdout(ends.stdout)
            .stderr(ends.stderr)
            .new_process_group()
            .spawn()
            .unwrap();
        let execution = Execution::new(spawned, pipes, options.process_group()).unwrap();
        Self::bound(execution, limits, timers)
    }

    /// Binds a runner with `limits` and `timers` to `execution`, with a peer of its own.
    fn bound(execution: Execution, limits: Limits, timers: Timers) -> Self {
        let now = Instant::now();
        Self {
            binding: Binding::new(execution, Runner::new(KEY, limits, timers, now)),
            peer: Peer::new(KEY, limits, timers, now),
            output: [Vec::new(), Vec::new()],
            stalled: false,
            input: Vec::new(),
            sent: 0,
            events: Vec::new(),
            terminal: None,
            failures: Vec::new(),
        }
    }

    /// Takes every step that the binding and the peer can take at `now` without waiting, and
    /// returns whether anything happened.
    fn pump(&mut self, now: Instant, buffer: &mut [u8]) -> bool {
        let mut progressed = false;
        while let Some(event) = self.binding.next(now, buffer) {
            progressed = true;
            self.handle(now, event, buffer);
        }
        self.serve(now, buffer) || progressed
    }

    /// Takes what one step of the binding at `now` reported: hands the peer a message, whose
    /// output is in `buffer`, and records a failure.
    fn handle(&mut self, now: Instant, event: Event, buffer: &[u8]) {
        match event {
            Event::Message(message) => self.deliver(now, message, buffer),
            Event::KillFailed(target, error) => {
                self.failures.push(format!("kill {target:?}: {error}"));
            }
            Event::SettlementFailed(error) => {
                self.failures.push(format!("settlement: {error}"));
            }
            Event::InputFailed(error) => {
                self.failures.push(format!("input: {error}"));
            }
        }
    }

    /// Takes every step that the peer, its source, and its sink can take at `now` without
    /// waiting, hands the peer's messages to the binding at once, and returns whether anything
    /// happened.
    fn serve(&mut self, now: Instant, buffer: &mut [u8]) -> bool {
        let mut progressed = false;
        if self.sent < self.input.len() {
            let length = self.peer.input(&self.input[self.sent..]);
            self.sent += length;
            progressed |= length > 0;
        }
        if self.sent == self.input.len() {
            self.peer.end_input();
        }
        if !self.stalled {
            for stream in Stream::ALL {
                loop {
                    let pending = self.peer.pending_output(stream);
                    if pending.is_empty() {
                        break;
                    }
                    let length = pending.len();
                    self.output[index(stream)].extend_from_slice(pending);
                    self.peer.consumed(stream, length);
                    progressed = true;
                }
            }
        }
        self.peer.tick(now);
        while let Some(event) = self.peer.next_event() {
            progressed = true;
            self.events.push(event);
        }
        while let Some(message) = self.peer.next_message(buffer) {
            progressed = true;
            match message {
                peer::Message::Cancel(cause) => self.binding.cancel(now, KEY, cause),
                peer::Message::Credit { pool, units } => self.binding.credit(KEY, pool, units),
                peer::Message::Input { length } => self.binding.input(KEY, &buffer[..length]),
                peer::Message::InputEnd => self.binding.end_input(KEY),
            }
            .expect("the runner accepts the peer's message");
        }
        progressed
    }

    /// Hands the peer the runner's `message` at `now`, whose output is in `buffer`.
    fn deliver(&mut self, now: Instant, message: runner::Message, buffer: &[u8]) {
        let receipt = match message {
            runner::Message::Credit { units } => self.peer.credit(KEY, units),
            runner::Message::Started => self.peer.started(KEY),
            runner::Message::Output { stream, length } => {
                self.peer.output(KEY, stream, &buffer[..length])
            }
            runner::Message::End(stream) => self.peer.end_output(KEY, stream),
            runner::Message::InputClosed => self.peer.input_closed(KEY),
            runner::Message::Terminal(outcome) => {
                assert!(self.terminal.is_none(), "one terminal outcome");
                self.terminal = Some(outcome);
                if self.binding.runner().is_detached() {
                    return;
                }
                self.peer.terminal(now, KEY, outcome)
            }
        };
        receipt.expect("the peer accepts the runner's message");
    }

    /// Returns whether both sides finished.
    fn is_finished(&self) -> bool {
        self.binding.is_finished() && self.peer.is_finished()
    }

    /// Returns the peer's outcome, once it finished.
    fn outcome(&self) -> Option<Outcome> {
        self.events.iter().find_map(|event| match event {
            peer::Event::Finished(outcome) => Some(*outcome),
            _ => None,
        })
    }
}

/// Waits until a descriptor of `sides` polls ready, or the earliest of their deadlines and
/// `until` passes, as an adapter's loop does.
fn wait(sides: &[&mut Side], until: Instant) {
    let mut descriptors = Vec::new();
    for side in sides {
        for (_, descriptor) in side.binding.descriptors() {
            descriptors.push(PollFd::new(descriptor, PollFlags::POLLIN));
        }
        if let Some(descriptor) = side.binding.input_writer() {
            descriptors.push(PollFd::new(descriptor, PollFlags::POLLOUT));
        }
    }
    let deadline = sides
        .iter()
        .flat_map(|side| [side.binding.deadline(), side.peer.deadline()])
        .flatten()
        .fold(until, Instant::min);
    let timeout = deadline.saturating_duration_since(Instant::now());
    // Poll counts whole milliseconds, so a shorter wait would return before its deadline.
    let timeout = if timeout.is_zero() {
        timeout
    } else {
        timeout.max(Duration::from_millis(1))
    };
    match nix::poll::poll(&mut descriptors, PollTimeout::try_from(timeout).unwrap()) {
        Ok(_) | Err(nix::errno::Errno::EINTR) => {}
        Err(error) => panic!("poll failed: {error}"),
    }
}

/// Drives `sides` in one loop, as an adapter does, until `done` holds: it takes every step that
/// they can take, then waits for what their bindings and peers wait on, and for `wake` if any.
/// It fails unless `done` holds within the test's patience, and fails as soon as a wait runs
/// into it, so a step that nothing they wait on leads to stalls the test rather than passing it.
fn run(sides: &mut [&mut Side], wake: Option<Instant>, mut done: impl FnMut(&[&mut Side]) -> bool) {
    let patience = Instant::now() + PATIENCE;
    let until = wake.map_or(patience, |wake| wake.min(patience));
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let mut progressed = false;
        for side in sides.iter_mut() {
            progressed |= side.pump(Instant::now(), &mut buffer);
        }
        if done(sides) {
            return;
        }
        if !progressed {
            wait(sides, until);
        }
        assert!(Instant::now() < patience, "the executions moved on in time");
    }
}

/// Drives `sides` in one loop until `done` holds: see [`run`].
fn drive(sides: &mut [&mut Side], done: impl FnMut(&[&mut Side]) -> bool) {
    run(sides, None, done);
}

/// Drives `sides` in one loop for `duration`: see [`run`].
fn drive_for(sides: &mut [&mut Side], duration: Duration) {
    let until = Instant::now() + duration;
    run(sides, Some(until), |_| Instant::now() >= until);
}

/// Drives `side` alone until both of its sides finished, and returns the peer's outcome.
fn finish(side: &mut Side) -> Outcome {
    drive(&mut [side], |sides| sides[0].is_finished());
    side.outcome().unwrap()
}

#[test]
fn output_beyond_its_windows_arrives_whole_and_the_outcome_is_lossless() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let mut side = Side::start(
        shell("seq 1 400000; seq 1 20000 >&2"),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(64 * 1024),
        timers(),
    );
    let outcome = finish(&mut side);
    assert_eq!(side.output[0], sequence(400_000));
    assert_eq!(side.output[1], sequence(20_000));
    assert_eq!(Some(outcome), side.terminal);
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert_eq!(outcome.cause(), None);
    assert!(!outcome.is_truncated(Stream::Stdout) && !outcome.is_truncated(Stream::Stderr));
    assert!(outcome.is_reusable());
    assert!(side.failures.is_empty(), "{:?}", side.failures);
    let runner = side.binding.runner();
    assert!(runner.high_water() <= runner.memory_bound());
    assert!(side.peer.high_water() <= side.peer.memory_bound());
    // Settlement killed the execution's cgroup, whose emptiness the binding checked.
    assert_eq!(
        std::fs::read(cgroup.path().join("cgroup.kill")).unwrap(),
        b"1"
    );
    assert_eq!(
        side.binding.next(Instant::now(), &mut [0; 16]).map(drop),
        None
    );
    assert!(side.binding.descriptors().next().is_none());
    assert_eq!(side.binding.deadline(), None);
}

#[test]
fn input_longer_than_its_window_reaches_the_program_intact() {
    // With byte windows, and with frames of up to 64 KiB less 256 bytes in a shared pool.
    let frames = Limits::new(64 * 1024 - 256, Window::frames(4), Window::frames(64))
        .unwrap()
        .shared_output();
    for limits in [limits(16 * 1024), frames] {
        let temporary = tempfile::tempdir().unwrap();
        let cgroup = fake_cgroup(temporary.path(), "0\n");
        let mut side = Side::start(
            shell("exec cat"),
            true,
            Options::new().cgroup(&cgroup, Scope::Execution),
            limits,
            timers(),
        );
        side.input = (0..3usize << 20)
            .map(|index| (index * 7 + index / 251) as u8)
            .collect();
        let outcome = finish(&mut side);
        assert_eq!(side.output[0], side.input, "{limits:?}");
        assert_eq!(outcome.status(), Some(Status::Exited(0)));
        assert!(!outcome.is_truncated(Stream::Stdout));
        assert!(outcome.is_reusable());
        let runner = side.binding.runner();
        assert!(runner.high_water() <= runner.memory_bound());
        assert!(side.peer.high_water() <= side.peer.memory_bound());
    }
}

#[test]
fn a_program_that_closes_its_input_early_stops_the_peer_s_source() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let mut side = Side::start(
        shell("head -c 5; printf done >&2"),
        true,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(16 * 1024),
        timers(),
    );
    side.input = vec![b'x'; 16 << 20];
    let outcome = finish(&mut side);
    assert_eq!(side.output, [b"xxxxx".to_vec(), b"done".to_vec()]);
    assert!(side.events.contains(&peer::Event::InputClosed));
    // The source stopped long before its 16 MiB.
    assert!(side.sent < 1 << 20, "{}", side.sent);
    // A program that closes its input is no failure.
    assert!(side.failures.is_empty(), "{:?}", side.failures);
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert!(!outcome.is_truncated(Stream::Stdout));
    assert!(outcome.is_reusable());
}

#[test]
fn an_adopted_program_runs_bound_as_a_spawned_one_does() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    // The test starts the program by a launcher other than `spawn`, the standard library's.
    let (stdin, input) = std::io::pipe().unwrap();
    let (stdout, stdout_writer) = std::io::pipe().unwrap();
    let (stderr, stderr_writer) = std::io::pipe().unwrap();
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", "cat; printf done >&2; exit 3"])
        .stdin(stdin)
        .stdout(stdout_writer)
        .stderr(stderr_writer)
        .process_group(0);
    // The execution collects the program's status, rather than the standard library's handle.
    #[allow(clippy::zombie_processes)]
    let program = command.spawn().unwrap();
    // The command holds the program's ends until it drops.
    drop(command);
    let pipes = Pipes::from_ends(Some(input.into()), stdout.into(), stderr.into()).unwrap();
    let execution = Execution::adopt(
        Child::with_pidfd(program.id()).unwrap(),
        pipes,
        Options::new()
            .process_group()
            .cgroup(&cgroup, Scope::Execution),
    )
    .unwrap();
    let mut side = Side::bound(execution, limits(16 * 1024), timers());
    side.input = b"input".to_vec();
    let outcome = finish(&mut side);
    assert_eq!(side.events.first(), Some(&peer::Event::Started));
    assert_eq!(side.output, [b"input".to_vec(), b"done".to_vec()]);
    assert_eq!(Some(outcome), side.terminal);
    assert_eq!(outcome.status(), Some(Status::Exited(3)));
    assert_eq!(outcome.cause(), None);
    assert!(outcome.is_reusable());
    assert!(side.failures.is_empty(), "{:?}", side.failures);
}

#[test]
fn a_failed_launch_stage_reaches_the_peer_by_its_number() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let mut side = Side::start(
        shell("exit 0").working_directory(c"/nonexistent/directory".to_owned()),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(16 * 1024),
        timers(),
    );
    let outcome = finish(&mut side);
    assert_eq!(Some(outcome), side.terminal);
    let failure = outcome.launch_failure().expect("the launch failed");
    // The caller tells the stage through NVX's numbering, rather than a number of its own.
    assert_eq!(
        LaunchStage::from_code(failure.stage()),
        Some(LaunchStage::WorkingDirectory)
    );
    assert_eq!(failure.error(), nix::errno::Errno::ENOENT as i32);
    assert_eq!(outcome.status(), Some(Status::Exited(125)));
    assert!(side.failures.is_empty(), "{:?}", side.failures);
}

#[test]
fn the_input_closes_with_its_last_byte_while_output_waits_to_be_returned() {
    // The program writes a line, then copies its input to its output. The runner holds the line
    // until the peer's credit reaches the binding, together with the input and its end: the
    // call that writes the input's last byte returns the line, and closes the program's input
    // before it does.
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let mut side = Side::start(
        shell("echo ready; exec cat"),
        true,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(4096),
        timers(),
    );
    // The runner's messages reach the peer, and the peer's wait, until the runner holds the
    // line, and the peer knows that the program started: a call of the binding may read the line
    // after it looked for the launch report.
    let mut buffer = vec![0u8; 64 * 1024];
    let patience = Instant::now() + PATIENCE;
    loop {
        let now = Instant::now();
        let mut progressed = false;
        while let Some(event) = side.binding.next(now, &mut buffer) {
            progressed = true;
            side.handle(now, event, &buffer);
        }
        if side.binding.runner().buffered() == b"ready\n".len() && side.peer.has_started() {
            break;
        }
        if !progressed {
            wait(&[&mut side], patience);
        }
        assert!(Instant::now() < patience, "the program wrote in time");
    }
    side.input = b"data".to_vec();
    let now = Instant::now();
    side.serve(now, &mut buffer);
    let event = side.binding.next(now, &mut buffer);
    assert!(
        matches!(
            event,
            Some(Event::Message(runner::Message::Output {
                stream: Stream::Stdout,
                length: 6,
            }))
        ),
        "{event:?}"
    );
    assert!(side.binding.execution().input().is_none());
    side.handle(now, event.unwrap(), &buffer);
    let outcome = finish(&mut side);
    assert_eq!(side.output[0], b"ready\ndata");
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert!(!outcome.is_truncated(Stream::Stdout));
    assert!(outcome.is_reusable());
}

#[test]
fn a_write_that_fails_closes_the_program_s_input() {
    // No pipe refuses a write but for `EPIPE`, so the program reads a socket of records instead,
    // whose writer refuses a record longer than its buffer with `EMSGSIZE`, as it refuses the
    // binding's first chunk. The binding closes the writer then, and reports the failure, as the
    // runner drops the rest of the input, so the program reads the input's end, and ends on its
    // own.
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let (reader, writer) = socket::socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::SOCK_CLOEXEC,
    )
    .unwrap();
    // Linux doubles the size, and refuses records longer than the result, less 32 bytes.
    socket::setsockopt(&writer, sockopt::SndBuf, &4096).unwrap();
    let writer = sys::FileDescriptor::from_owned(writer);
    writer.set_nonblocking().unwrap();
    let (mut pipes, ends) = Pipes::new(true).unwrap();
    pipes.input = Some(writer);
    let spawned = shell("exec cat")
        .stdin(reader)
        .stdout(ends.stdout)
        .stderr(ends.stderr)
        .new_process_group()
        .spawn()
        .unwrap();
    let options = Options::new()
        .cgroup(&cgroup, Scope::Execution)
        .process_group();
    let execution = Execution::new(spawned, pipes, options).unwrap();
    let mut side = Side::bound(execution, limits(64 * 1024), timers());
    side.input = vec![b'i'; 16 * 1024];
    let outcome = finish(&mut side);
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert_eq!(outcome.cause(), None);
    assert!(side.output.iter().all(Vec::is_empty));
    assert!(outcome.is_reusable());
    let refused = io::Error::from(nix::errno::Errno::EMSGSIZE);
    assert_eq!(side.failures, [format!("input: {refused}")]);
}

/// Returns the process that `output` names by its ID on one line, opened by a pidfd that polls
/// readable once it ends.
fn descendant(output: &[u8]) -> (ProcessId, crate::guest::sys::FileDescriptor) {
    let id = std::str::from_utf8(output).unwrap().trim().parse().unwrap();
    let process = ProcessId::from_positive(id).unwrap();
    (process, sys::open_pidfd(process).unwrap())
}

#[test]
fn descendants_and_pipe_writers_settle_before_reuse_in_a_cgroup_that_the_test_plays() {
    // The fake cgroup counts the program and two descendants that outlive it: one in a session
    // of its own that holds the program's standard output, and one that holds nothing. The test
    // plays the kernel: it ends the descendants once the cgroup is killed, and only then counts
    // no process.
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "3\n");
    let kill = cgroup.path().join("cgroup.kill");
    let pids = cgroup.path().join("pids.current");
    let mut side = Side::start(
        shell(
            "setsid sleep 60 2>/dev/null & echo $!; \
             sleep 60 </dev/null >/dev/null 2>&1 & echo $! >&2",
        ),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(16 * 1024),
        timers(),
    );
    // The program ends at once, and its standard error with it, as no descendant holds it. Its
    // standard output stays open, and the cgroup is killed.
    drive(&mut [&mut side], |sides| {
        sides[0].output.iter().all(|output| output.ends_with(b"\n"))
            && sides[0].events.contains(&peer::Event::End(Stream::Stderr))
            && sides[0].binding.execution().status().is_some()
    });
    assert_eq!(std::fs::read(&kill).unwrap(), b"1");
    assert!(!side.events.contains(&peer::Event::End(Stream::Stdout)));
    let holder = descendant(&side.output[0]);
    let detached = descendant(&side.output[1]);
    // The kernel's kill: the descendants end, and the pipe's last writer with them.
    for (process, _) in [&holder, &detached] {
        sys::kill_process(*process).unwrap();
    }
    for (_, pidfd) in [&holder, &detached] {
        let patience = u16::try_from(PATIENCE.as_millis()).unwrap();
        assert_eq!(
            sys::readable_or_hung_up(&[pidfd.borrowed()], patience).unwrap(),
            [true]
        );
    }
    // The program's end, the streams' ends, and the descendants' end are not settlement: the
    // cgroup still counts processes, so the execution stays unsettled.
    drive(&mut [&mut side], |sides| {
        sides[0].events.contains(&peer::Event::End(Stream::Stdout))
    });
    drive_for(&mut [&mut side], Duration::from_millis(300));
    assert!(!side.binding.is_finished());
    assert!(side.terminal.is_none());
    assert!(side.binding.runner().awaits_settlement());
    // Once the cgroup is empty, the execution settles, and is reusable.
    std::fs::write(&pids, b"0\n").unwrap();
    let outcome = finish(&mut side);
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert!(outcome.is_reusable());
    assert!(!outcome.is_truncated(Stream::Stdout));
    assert!(side.failures.is_empty(), "{:?}", side.failures);
}

#[test]
fn the_settlement_kills_what_the_program_leaves_behind_in_a_cgroup_that_the_test_plays() {
    // The program's descendant holds neither output stream, and both streams end while the
    // program runs, so that once it ends, the settlement's kill is the cgroup's only one.
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "2\n");
    let kill = cgroup.path().join("cgroup.kill");
    let go = temporary.path().join("go");
    let mut side = Side::start(
        shell(&format!(
            "sleep 60 </dev/null >/dev/null 2>&1 & echo $!; exec >&- 2>&-; \
             until [ -e '{}' ]; do sleep 0.01; done",
            go.display()
        )),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(16 * 1024),
        timers(),
    );
    drive(&mut [&mut side], |sides| {
        Stream::ALL
            .into_iter()
            .all(|stream| sides[0].events.contains(&peer::Event::End(stream)))
    });
    let (process, pidfd) = descendant(&side.output[0]);
    assert!(side.binding.execution().status().is_none());
    assert_eq!(std::fs::read(&kill).unwrap(), b"");
    // The program ends; the settlement kills the cgroup, and checks it from then on.
    std::fs::write(&go, b"").unwrap();
    drive(&mut [&mut side], |sides| {
        sides[0].binding.execution().status().is_some()
    });
    assert_eq!(std::fs::read(&kill).unwrap(), b"1");
    assert!(side.binding.runner().awaits_settlement());
    assert!(!side.binding.is_finished());
    // The kernel's kill ends the descendant, after which the cgroup counts no process.
    sys::kill_process(process).unwrap();
    let patience = u16::try_from(PATIENCE.as_millis()).unwrap();
    assert_eq!(
        sys::readable_or_hung_up(&[pidfd.borrowed()], patience).unwrap(),
        [true]
    );
    std::fs::write(cgroup.path().join("pids.current"), b"0\n").unwrap();
    let outcome = finish(&mut side);
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert!(outcome.is_reusable());
    assert!(side.failures.is_empty(), "{:?}", side.failures);
}

#[test]
fn the_settlement_begins_as_the_program_ends_while_a_descendant_holds_its_output() {
    // A descendant in a session of its own holds both of the program's output streams, and the
    // fake cgroup counts no process. The program ends once the test lets it, when nothing is
    // left to read, so that nothing but its end leads to the settlement, which is over before
    // the output ends. Once the outcome is reusable, the caller reuses the cgroup before it drops
    // the binding, which then kills nothing in it.
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let go = temporary.path().join("go");
    let mut side = Side::start(
        shell(&format!(
            "setsid sleep 60 & echo $!; until [ -e '{}' ]; do sleep 0.01; done",
            go.display()
        )),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(16 * 1024),
        timers(),
    );
    drive(&mut [&mut side], |sides| {
        sides[0].output[0].ends_with(b"\n")
    });
    let holder = Stray(descendant(&side.output[0]));
    std::fs::write(&go, b"").unwrap();
    drive(&mut [&mut side], |sides| {
        sides[0].binding.execution().status().is_some()
            && !sides[0].binding.runner().awaits_settlement()
    });
    assert!(
        side.events
            .iter()
            .all(|event| !matches!(event, peer::Event::End(_)))
    );
    assert!(!side.binding.is_finished());
    // The output's end is the last that the terminal outcome waits for.
    drop(holder);
    let outcome = finish(&mut side);
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert!(outcome.is_reusable());
    assert!(!outcome.is_truncated(Stream::Stdout));
    // The settlement killed the cgroup, which is the caller's from then on.
    let kill = cgroup.path().join("cgroup.kill");
    assert_eq!(std::fs::read(&kill).unwrap(), b"1");
    std::fs::write(&kill, b"").unwrap();
    drop(side);
    assert_eq!(std::fs::read(&kill).unwrap(), b"");
}

/// A workload cgroup in the real hierarchy, which the test removes, with everything in it, when
/// it ends.
struct RealWorkload(Layout);

impl RealWorkload {
    /// Creates a new workload cgroup below the root of the hierarchy at `/sys/fs/cgroup`, with
    /// the test's own cgroup as the agent's. It returns `None` unless the test runs as root in a
    /// cgroup v2 hierarchy with the `pids` controller, in which it can create a cgroup.
    fn new(name: &str) -> Option<Self> {
        let root = Path::new("/sys/fs/cgroup");
        let controllers = std::fs::read_to_string(root.join("cgroup.controllers")).ok()?;
        if !nix::unistd::geteuid().is_root()
            || !controllers
                .split_whitespace()
                .any(|controller| controller == "pids")
        {
            return None;
        }
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let own = own.lines().find_map(|line| line.strip_prefix("0::"))?;
        let agent = root.join(own.trim_start_matches('/'));
        let workload = root.join(format!("nvx-binding-{}-{name}", std::process::id()));
        let layout = Layout::new(agent, workload).ok()?;
        std::fs::create_dir(layout.workload()).ok()?;
        Some(Self(layout))
    }
}

impl Drop for RealWorkload {
    /// Kills whatever is left in the workload cgroup, then removes its cgroups.
    fn drop(&mut self) {
        let workload = self.0.workload();
        let _ = std::fs::write(workload.join("cgroup.kill"), b"1");
        for _ in 0..1000 {
            if std::fs::read(workload.join("pids.current")).is_ok_and(|count| count == b"0\n") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Ok(entries) = std::fs::read_dir(workload) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    let _ = std::fs::remove_dir(entry.path());
                }
            }
        }
        let _ = std::fs::remove_dir(workload);
    }
}

#[test]
fn descendants_setsid_and_pipe_writers_settle_before_reuse_in_a_real_cgroup() {
    // The test changes the real hierarchy, which needs root; elsewhere it does nothing.
    let Some(workload) = RealWorkload::new("settle") else {
        return;
    };
    let cgroup = ExecutionCgroup::prepare(&workload.0, Some("exec"), &[]).unwrap();
    // The program ends only once the test has opened its descendants, which the settlement
    // kills as soon as the binding collects the program's status.
    let temporary = tempfile::tempdir().unwrap();
    let opened = temporary.path().join("opened");
    let mut side = Side::start(
        shell(&format!(
            "setsid sleep 60 & echo $!; \
             sleep 60 </dev/null >/dev/null 2>&1 & echo $! >&2; \
             while [ ! -e '{}' ]; do sleep 0.01; done",
            opened.display()
        ))
        .cgroup(cgroup.placement().unwrap()),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(16 * 1024),
        timers(),
    );
    drive(&mut [&mut side], |sides| {
        sides[0].output.iter().all(|output| output.ends_with(b"\n"))
    });
    let holder = descendant(&side.output[0]);
    let detached = descendant(&side.output[1]);
    std::fs::write(&opened, b"").unwrap();
    let outcome = finish(&mut side);
    // The terminal outcome came once the cgroup was empty: both descendants, the one in a
    // session of its own that held the output included, are gone.
    assert!(!cgroup.is_occupied(Scope::Execution).unwrap());
    for (_, pidfd) in [&holder, &detached] {
        assert_eq!(
            sys::readable_or_hung_up(&[pidfd.borrowed()], 0).unwrap(),
            [true]
        );
    }
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert!(outcome.is_reusable(), "{:?}", side.failures);
    assert!(!outcome.is_truncated(Stream::Stdout));
    // The caller reuses the cgroup at once, for a new program, before it drops the binding: the
    // new program outlives the drop.
    let spawned = shell("exec sleep 60")
        .cgroup(cgroup.placement().unwrap())
        .spawn()
        .unwrap();
    let id = spawned.id();
    let pidfd = sys::open_pidfd(ProcessId::from_positive(i64::from(id)).unwrap()).unwrap();
    assert_eq!(spawned.into_report().wait().unwrap(), None);
    drop(side);
    assert!(cgroup.is_occupied(Scope::Execution).unwrap());
    assert_eq!(
        sys::readable_or_hung_up(&[pidfd.borrowed()], 0).unwrap(),
        [false]
    );
    cgroup.kill(Scope::Execution).unwrap();
    let status = Child::new(id).unwrap().wait().unwrap();
    assert_eq!(status.signal(), Some(9));
    cgroup.remove().unwrap();
}

#[test]
fn a_cleanup_failure_fences_the_execution() {
    // A cgroup whose kill fails, one that never empties, and none at all.
    let temporary = tempfile::tempdir().unwrap();
    let unkillable = fake_cgroup(&temporary.path().join("unkillable"), "0\n");
    std::fs::remove_file(unkillable.path().join("cgroup.kill")).unwrap();
    let occupied = fake_cgroup(&temporary.path().join("occupied"), "1\n");
    let short = Timers::new(GRACE, Duration::from_millis(300));
    for (options, timers, failed) in [
        (
            Options::new().cgroup(&unkillable, Scope::Execution),
            timers(),
            true,
        ),
        (
            Options::new().cgroup(&occupied, Scope::Execution),
            short,
            false,
        ),
        (Options::new(), timers(), false),
    ] {
        let mut side = Side::start(shell("printf ran"), false, options, limits(4096), timers);
        let outcome = finish(&mut side);
        assert_eq!(side.output[0], b"ran");
        assert_eq!(outcome.status(), Some(Status::Exited(0)));
        assert!(!outcome.is_reusable());
        assert!(!outcome.is_truncated(Stream::Stdout));
        assert_eq!(
            side.failures
                .iter()
                .any(|failure| failure.starts_with("settlement")),
            failed,
            "{:?}",
            side.failures
        );
    }
}

#[test]
fn an_unrelated_child_s_status_stays_with_its_owner() {
    let mut unrelated = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 7"])
        .spawn()
        .unwrap();
    while !sys::has_exited(ProcessId::from_child(&unrelated)).unwrap() {
        std::thread::yield_now();
    }
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let mut side = Side::start(
        shell("printf ran"),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(4096),
        timers(),
    );
    assert!(finish(&mut side).is_reusable());
    // Nothing collected the unrelated child's status, which its own wait still finds.
    assert_eq!(unrelated.wait().unwrap().code(), Some(7));
}

#[test]
fn a_caller_that_reaps_hands_the_program_s_status_over() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let mut side = Side::start(
        shell("printf ran; exit 6"),
        false,
        Options::new()
            .cgroup(&cgroup, Scope::Execution)
            .caller_reaps(),
        limits(4096),
        timers(),
    );
    let id = side.binding.id();
    // The binding never waits for the program, nor lists its pidfd.
    assert!(
        side.binding
            .descriptors()
            .all(|(source, _)| source != Source::Exit)
    );
    // The program ends, which nothing in the binding waits for.
    drive(&mut [&mut side], |sides| {
        sides[0].output[0] == b"ran"
            && Stream::ALL
                .into_iter()
                .all(|stream| sides[0].events.contains(&peer::Event::End(stream)))
    });
    let process = ProcessId::from_positive(i64::from(id)).unwrap();
    let started = Instant::now();
    while !sys::has_exited(process).unwrap() {
        assert!(started.elapsed() < PATIENCE, "the program ended in time");
        std::thread::sleep(Duration::from_millis(1));
    }
    drive_for(&mut [&mut side], Duration::from_millis(100));
    assert!(!side.binding.is_finished());
    // The caller's own collector takes the status, and hands it over.
    let status = Child::new(id).unwrap().wait().unwrap();
    assert!(side.binding.observe(id, status));
    let outcome = finish(&mut side);
    assert_eq!(outcome.status(), Some(Status::Exited(6)));
    assert!(outcome.is_reusable());
}

#[test]
fn a_signal_that_the_program_handles_reaches_the_runner_only_as_the_program_s_end() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    // The program handles `SIGTERM` by writing more than the windows hold, and exiting 0, and
    // waits for a descendant in its process group, which the signal ends too, without dash's
    // report of the signal that ended it. It exits 3 unless its handler runs.
    let mut side = Side::start(
        shell(
            "trap 'seq 1 20000; exit 0' TERM; sleep 30 </dev/null >/dev/null 2>&1 & \
             printf ready; wait $! 2>/dev/null; exit 3",
        ),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(4096),
        timers(),
    );
    drive(&mut [&mut side], |sides| sides[0].output[0] == b"ready");
    let refused = side.binding.signal(nix::libc::SIGKILL).unwrap_err();
    assert_eq!(refused.raw_os_error(), Some(nix::libc::EINVAL));
    assert!(side.binding.signal(nix::libc::SIGTERM).unwrap());
    let outcome = finish(&mut side);
    let mut output = b"ready".to_vec();
    output.extend(sequence(20_000));
    assert_eq!(side.output, [output, Vec::new()]);
    assert_eq!(Some(outcome), side.terminal);
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert_eq!(outcome.cause(), None);
    assert!(!outcome.is_truncated(Stream::Stdout));
    assert!(outcome.is_reusable());
    assert!(side.failures.is_empty(), "{:?}", side.failures);
    assert!(!side.binding.signal(nix::libc::SIGTERM).unwrap());
}

#[test]
fn the_program_s_end_reaches_the_runner_before_queued_output_whoever_collects_its_status() {
    // The program writes 24 KiB to a sink that takes nothing: the peer holds a window, 4 KiB,
    // the runner a chunk, 16 KiB, and the pipe the rest, 4 KiB, which even a pipe of one page
    // holds. Then it exits 3, once the test lets it, before the runner's timeout, while nothing
    // collects its status: neither the execution, which the test does not drive meanwhile, nor
    // the caller's own collector, which then hands it over. Every call from then on comes after
    // the timeout, and the sink takes the output 64 bytes at a time, so that every call has
    // queued output to return: the program's end reached the runner first, so the timeout never
    // fires.
    const TIMEOUT: Duration = Duration::from_secs(20);
    const PAYLOAD: usize = 24 * 1024;
    let limits = limits(4096);
    for caller_reaps in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let cgroup = fake_cgroup(temporary.path(), "0\n");
        let gate = temporary.path().join("gate");
        let mut options = Options::new().cgroup(&cgroup, Scope::Execution);
        if caller_reaps {
            options = options.caller_reaps();
        }
        let mut side = Side::start(
            shell(&format!(
                "head -c {PAYLOAD} /dev/zero; while [ ! -e '{}' ]; do sleep 0.01; done; exit 3",
                gate.display()
            )),
            false,
            options,
            limits,
            Timers::new(GRACE, TIMEOUT * 3).timeout(TIMEOUT),
        );
        // Only the runner has the timeout. The calls after it come within its settlement time.
        side.peer = Peer::new(KEY, limits, timers(), Instant::now());
        let late = Instant::now() + TIMEOUT;
        side.stalled = true;
        drive(&mut [&mut side], |sides| {
            sides[0].peer.buffered() == 4096
                && sides[0].binding.runner().read_allowance(Stream::Stdout) == 0
        });
        std::fs::write(&gate, b"").unwrap();
        let id = side.binding.id();
        let process = ProcessId::from_positive(i64::from(id)).unwrap();
        let started = Instant::now();
        while !sys::has_exited(process).unwrap() {
            assert!(started.elapsed() < PATIENCE, "the program ended in time");
            std::thread::sleep(Duration::from_millis(1));
        }
        if caller_reaps {
            let status = Child::new(id).unwrap().wait().unwrap();
            assert!(side.binding.observe(id, status));
        }
        side.stalled = false;
        let mut buffer = [0u8; 64];
        side.serve(late, &mut buffer);
        let event = side.binding.next(late, &mut buffer);
        assert!(
            matches!(
                event,
                Some(Event::Message(runner::Message::Output {
                    stream: Stream::Stdout,
                    ..
                }))
            ),
            "{event:?}"
        );
        side.handle(late, event.unwrap(), &buffer);
        let patience = Instant::now() + PATIENCE;
        while !side.is_finished() {
            assert!(Instant::now() < patience, "the execution finished in time");
            if !side.pump(late, &mut buffer) {
                wait(&[&mut side], Instant::now() + Duration::from_millis(10));
            }
        }
        let terminal = side.terminal.unwrap();
        assert_eq!(terminal.status(), Some(Status::Exited(3)), "{caller_reaps}");
        assert_eq!(terminal.cause(), None, "{caller_reaps}");
        assert!(!terminal.is_truncated(Stream::Stdout), "{caller_reaps}");
        assert!(terminal.is_reusable(), "{caller_reaps}");
        assert_eq!(side.output[0], vec![0; PAYLOAD], "{caller_reaps}");
        assert!(side.failures.is_empty(), "{:?}", side.failures);
    }
}

#[test]
fn the_program_s_end_reaches_the_runner_before_its_timeout_in_a_first_call_after_it() {
    // The program writes a word and exits 4 at once, and its launch report arrives, but the first
    // call comes only after the runner's timeout, whoever collects the program's status: the
    // launch report and the program's end reach the runner before the timeout fires, so the
    // output arrives whole, and the outcome names no cause.
    const TIMEOUT: Duration = Duration::from_secs(20);
    let limits = limits(4096);
    for caller_reaps in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let cgroup = fake_cgroup(temporary.path(), "0\n");
        let mut options = Options::new().cgroup(&cgroup, Scope::Execution);
        if caller_reaps {
            options = options.caller_reaps();
        }
        let mut side = Side::start(
            shell("printf hello; exit 4"),
            false,
            options,
            limits,
            Timers::new(GRACE, TIMEOUT * 3).timeout(TIMEOUT),
        );
        // Only the runner has the timeout.
        side.peer = Peer::new(KEY, limits, timers(), Instant::now());
        let late = Instant::now() + TIMEOUT;
        let id = side.binding.id();
        let process = ProcessId::from_positive(i64::from(id)).unwrap();
        let started = Instant::now();
        while !sys::has_exited(process).unwrap() {
            assert!(started.elapsed() < PATIENCE, "the program ended in time");
            std::thread::sleep(Duration::from_millis(1));
        }
        if caller_reaps {
            let status = Child::new(id).unwrap().wait().unwrap();
            assert!(side.binding.observe(id, status));
        }
        let report = side
            .binding
            .descriptors()
            .find_map(|(source, descriptor)| matches!(source, Source::Launch).then_some(descriptor))
            .expect("the launch report is pending");
        let patience = u16::try_from(PATIENCE.as_millis()).unwrap();
        assert_eq!(
            sys::readable_or_hung_up(&[report], patience).unwrap(),
            [true]
        );
        let mut buffer = [0u8; 64];
        let patience = Instant::now() + PATIENCE;
        while !side.is_finished() {
            assert!(Instant::now() < patience, "the execution finished in time");
            if !side.pump(late, &mut buffer) {
                wait(&[&mut side], Instant::now() + Duration::from_millis(10));
            }
        }
        let terminal = side.terminal.unwrap();
        assert_eq!(terminal.status(), Some(Status::Exited(4)), "{caller_reaps}");
        assert_eq!(terminal.cause(), None, "{caller_reaps}");
        assert!(!terminal.is_truncated(Stream::Stdout), "{caller_reaps}");
        assert!(terminal.is_reusable(), "{caller_reaps}");
        assert_eq!(side.output[0], b"hello", "{caller_reaps}");
        assert!(side.failures.is_empty(), "{:?}", side.failures);
    }
}

#[test]
fn the_program_s_end_reaches_the_runner_before_its_timeout_while_its_launch_report_is_late() {
    // The program writes a word and exits 4 at once, but the test holds a copy of its launch
    // report's writer, as a process that another thread forked while the program was spawning
    // would, so the report arrives only once the program has ended and the first calls, which
    // come after the runner's timeout, have taken every step that they could. The program's end
    // reaches the runner before the timeout fires all the same, whoever collects its status, and
    // the runner holds the program's start and output until the report arrives: the output
    // arrives whole, and the outcome names no cause.
    const TIMEOUT: Duration = Duration::from_secs(20);
    let limits = limits(4096);
    for caller_reaps in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let cgroup = fake_cgroup(temporary.path(), "0\n");
        let mut options = Options::new().cgroup(&cgroup, Scope::Execution);
        if caller_reaps {
            options = options.caller_reaps();
        }
        let (pipes, ends) = Pipes::new(false).unwrap();
        let (spawned, writer) = shell("printf hello; exit 4")
            .stdin(OwnedFd::from(std::fs::File::open("/dev/null").unwrap()))
            .stdout(ends.stdout)
            .stderr(ends.stderr)
            .new_process_group()
            .spawn_holding_report()
            .unwrap();
        let execution = Execution::new(spawned, pipes, options.process_group()).unwrap();
        let mut side = Side::bound(
            execution,
            limits,
            Timers::new(GRACE, TIMEOUT * 3).timeout(TIMEOUT),
        );
        // Only the runner has the timeout.
        side.peer = Peer::new(KEY, limits, timers(), Instant::now());
        let late = Instant::now() + TIMEOUT;
        let id = side.binding.id();
        let process = ProcessId::from_positive(i64::from(id)).unwrap();
        let started = Instant::now();
        while !sys::has_exited(process).unwrap() {
            assert!(started.elapsed() < PATIENCE, "the program ended in time");
            std::thread::sleep(Duration::from_millis(1));
        }
        if caller_reaps {
            let status = Child::new(id).unwrap().wait().unwrap();
            assert!(side.binding.observe(id, status));
        }
        let mut buffer = [0u8; 64];
        while side.pump(late, &mut buffer) {}
        assert!(!side.peer.has_started(), "{caller_reaps}");
        assert!(side.output[0].is_empty(), "{caller_reaps}");
        assert!(side.terminal.is_none(), "{caller_reaps}");
        drop(writer);
        let patience = Instant::now() + PATIENCE;
        while !side.is_finished() {
            assert!(Instant::now() < patience, "the execution finished in time");
            if !side.pump(late, &mut buffer) {
                wait(&[&mut side], Instant::now() + Duration::from_millis(10));
            }
        }
        let terminal = side.terminal.unwrap();
        assert_eq!(terminal.status(), Some(Status::Exited(4)), "{caller_reaps}");
        assert_eq!(terminal.cause(), None, "{caller_reaps}");
        assert!(!terminal.is_truncated(Stream::Stdout), "{caller_reaps}");
        assert!(terminal.is_reusable(), "{caller_reaps}");
        assert_eq!(side.output[0], b"hello", "{caller_reaps}");
        assert!(side.failures.is_empty(), "{:?}", side.failures);
    }
}

#[test]
fn the_runner_gives_up_in_time_on_a_launch_report_that_a_process_holds_after_the_program_s_end() {
    // The program writes a word and exits 4 at once, but the test holds a copy of its launch
    // report's writer throughout, as a process that another thread forked while the program was
    // spawning would if it neither replaced itself with its program nor ended, whether the
    // execution settles or, without a cgroup, does not. Only the runner has a timeout, and the
    // first calls come after it. The runner holds the program's start and output until the
    // settlement time and the grace have passed since the program's end, then gives up on the
    // report: the output arrives whole, without a start, and the outcome keeps the program's
    // status, names no cause, and is fenced.
    const TIMEOUT: Duration = Duration::from_secs(20);
    let limits = limits(4096);
    for settles in [true, false] {
        let temporary = tempfile::tempdir().unwrap();
        let cgroup = fake_cgroup(temporary.path(), "0\n");
        let options = if settles {
            Options::new().cgroup(&cgroup, Scope::Execution)
        } else {
            Options::new()
        };
        let (pipes, ends) = Pipes::new(false).unwrap();
        let (spawned, writer) = shell("printf hello; exit 4")
            .stdin(OwnedFd::from(std::fs::File::open("/dev/null").unwrap()))
            .stdout(ends.stdout)
            .stderr(ends.stderr)
            .new_process_group()
            .spawn_holding_report()
            .unwrap();
        let execution = Execution::new(spawned, pipes, options.process_group()).unwrap();
        let mut side = Side::bound(
            execution,
            limits,
            Timers::new(GRACE, SETTLEMENT).timeout(TIMEOUT),
        );
        // Only the runner has the timeout.
        side.peer = Peer::new(KEY, limits, timers(), Instant::now());
        let late = Instant::now() + TIMEOUT;
        let id = side.binding.id();
        let process = ProcessId::from_positive(i64::from(id)).unwrap();
        let started = Instant::now();
        while !sys::has_exited(process).unwrap() {
            assert!(started.elapsed() < PATIENCE, "the program ended in time");
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut buffer = [0u8; 64];
        // A process that another test's thread forked while the program was spawning may hold
        // the program's output pipes for a moment too, and the runner gives up on the streams
        // still open at the instant at which it gives up on the report, so the test waits for
        // both streams to close first.
        let patience = Instant::now() + PATIENCE;
        while Stream::ALL
            .into_iter()
            .any(|stream| side.binding.runner().read_allowance(stream) > 0)
        {
            assert!(
                Instant::now() < patience,
                "the program's streams closed in time"
            );
            if !side.pump(late, &mut buffer) {
                wait(&[&mut side], Instant::now() + Duration::from_millis(10));
            }
        }
        while side.pump(late, &mut buffer) {}
        let given_up = late + SETTLEMENT + GRACE;
        assert_eq!(side.binding.deadline(), Some(given_up), "{settles}");
        while side.pump(given_up - Duration::from_millis(1), &mut buffer) {}
        assert!(!side.peer.has_started(), "{settles}");
        assert!(side.output[0].is_empty(), "{settles}");
        assert!(side.terminal.is_none(), "{settles}");
        let patience = Instant::now() + PATIENCE;
        while !side.is_finished() {
            assert!(Instant::now() < patience, "the execution finished in time");
            if !side.pump(given_up, &mut buffer) {
                wait(&[&mut side], Instant::now() + Duration::from_millis(10));
            }
        }
        let terminal = side.terminal.unwrap();
        assert_eq!(terminal.status(), Some(Status::Exited(4)), "{settles}");
        assert_eq!(terminal.cause(), None, "{settles}");
        assert_eq!(terminal.launch_failure(), None, "{settles}");
        assert!(!terminal.is_truncated(Stream::Stdout), "{settles}");
        assert!(!terminal.is_reusable(), "{settles}");
        assert_eq!(side.output[0], b"hello", "{settles}");
        assert!(!side.peer.has_started(), "{settles}");
        assert!(side.failures.is_empty(), "{:?}", side.failures);
        drop(writer);
    }
}

/// How the liveness harness ends the stalled execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The peer's caller cancels it.
    Cancel,
    /// The runner's timeout passes.
    Timeout,
    /// The session is reset.
    Reset,
}

#[test]
fn a_stalled_sink_stalls_only_its_own_execution_and_endings_still_finish_in_time() {
    const TIMEOUT: Duration = Duration::from_secs(2);
    for ending in [Ending::Cancel, Ending::Timeout, Ending::Reset] {
        let temporary = tempfile::tempdir().unwrap();
        let stalled_cgroup = fake_cgroup(&temporary.path().join("stalled"), "0\n");
        let other_cgroup = fake_cgroup(&temporary.path().join("other"), "0\n");
        let stalled_timers = match ending {
            Ending::Timeout => timers().timeout(TIMEOUT),
            _ => timers(),
        };
        let mut stalled = Side::start(
            shell("exec head -c 16777216 /dev/zero"),
            false,
            Options::new().cgroup(&stalled_cgroup, Scope::Execution),
            limits(64 * 1024),
            stalled_timers,
        );
        let created = Instant::now();
        stalled.stalled = true;
        // The stalled sink exhausts its window, and its program blocks on its full pipe.
        drive(&mut [&mut stalled], |sides| {
            sides[0].binding.runner().read_allowance(Stream::Stdout) == 0
                && sides[0].peer.buffered() == 64 * 1024
        });
        // Another execution streams meanwhile, and keeps streaming after a MiB.
        let mut other = Side::start(
            shell("seq 1 600000"),
            false,
            Options::new().cgroup(&other_cgroup, Scope::Execution),
            limits(64 * 1024),
            timers(),
        );
        drive(&mut [&mut stalled, &mut other], |sides| {
            sides[1].output[0].len() > 1 << 20
        });
        assert!(stalled.binding.execution().status().is_none(), "{ending:?}");
        assert!(stalled.output[0].is_empty());
        assert_eq!(stalled.peer.buffered(), 64 * 1024, "{ending:?}");
        let streamed = other.output[0].len();
        let ended = match ending {
            Ending::Cancel => {
                let now = Instant::now();
                stalled.peer.cancel(now);
                // The cancellation reaches the runner at once, so that both sides' graces end
                // together: a runner whose grace outlasted the peer's would take the window that
                // the peer's ended grace frees, and could send the rest without a loss of its own.
                stalled.serve(now, &mut [0; 64]);
                now
            }
            Ending::Timeout => created + TIMEOUT,
            Ending::Reset => {
                let now = Instant::now();
                stalled.binding.reset(now);
                stalled.peer.reset(now);
                now
            }
        };
        // The stalled execution ends, while the other one streams on.
        let mut finished = None;
        drive(&mut [&mut stalled, &mut other], |sides| {
            if sides[0].is_finished() {
                finished.get_or_insert_with(Instant::now);
            }
            finished.is_some() && sides[1].output[0].len() > streamed
        });
        // Each side finished within its grace, and the peer, which learns of the timeout only
        // from the terminal outcome, within a second one.
        let finished = finished.unwrap();
        let grace = match ending {
            Ending::Timeout => GRACE + GRACE,
            _ => GRACE,
        };
        assert!(
            finished <= ended + grace + SLACK,
            "{ending:?} took {:?}",
            finished - ended
        );
        let cause = match ending {
            Ending::Cancel => Cause::Cancelled,
            Ending::Timeout => Cause::TimedOut,
            Ending::Reset => Cause::Reset,
        };
        let terminal = stalled.terminal.unwrap();
        assert_eq!(terminal.cause(), Some(cause), "{ending:?}");
        assert_eq!(terminal.status(), Some(Status::Signaled(9)), "{ending:?}");
        assert!(terminal.is_truncated(Stream::Stdout), "{ending:?}");
        assert!(terminal.is_reusable(), "{ending:?}");
        let outcome = stalled.outcome().unwrap();
        assert_eq!(outcome.cause(), Some(cause), "{ending:?}");
        assert!(outcome.is_truncated(Stream::Stdout), "{ending:?}");
        // The other execution still finishes whole.
        let outcome = finish(&mut other);
        assert_eq!(other.output[0], sequence(600_000), "{ending:?}");
        assert!(!outcome.is_truncated(Stream::Stdout));
    }
}

#[test]
fn a_call_takes_a_bounded_number_of_steps_and_asks_to_be_called_again() {
    // The binding reads at most a window and a chunk, 20 KiB, of the program's 24 KiB, so the
    // rest, 4 KiB, which any pipe holds, is left in the pipe once the program wrote everything.
    // The session is reset then, and the binding discards the rest with a buffer of 16 bytes:
    // in more steps than one call takes.
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let written = temporary.path().join("written");
    let mut side = Side::start(
        shell(&format!(
            "head -c 24576 /dev/zero; : >'{}'; exec sleep 60",
            written.display()
        )),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(4096),
        timers(),
    );
    side.stalled = true;
    drive(&mut [&mut side], |sides| {
        sides[0].peer.buffered() == 4096
            && sides[0].binding.runner().read_allowance(Stream::Stdout) == 0
    });
    // The rest fits in the pipe, so the program writes it without another read.
    let started = Instant::now();
    while !written.exists() {
        assert!(started.elapsed() < PATIENCE, "the program wrote in time");
        std::thread::sleep(Duration::from_millis(1));
    }
    let now = Instant::now();
    side.binding.reset(now);
    side.peer.reset(now);
    assert!(side.binding.next(now, &mut [0; 16]).is_none());
    assert_eq!(side.binding.deadline(), Some(now));
    let outcome = finish(&mut side);
    assert_eq!(outcome.cause(), Some(Cause::Reset));
    let terminal = side.terminal.unwrap();
    assert_eq!(terminal.cause(), Some(Cause::Reset));
    assert!(terminal.is_truncated(Stream::Stdout));
    assert!(terminal.is_reusable());
}

#[test]
fn a_call_writes_a_bounded_number_of_chunks_of_input_and_asks_to_be_called_again() {
    // The runner holds a whole window of input, 8 KiB in chunks of 128 bytes, before the binding
    // writes any of it, and the program reads all of it. Each call writes at most a chunk at a
    // time, and at most `STEPS` times, so 2 KiB, where a pipe, which takes a page at least,
    // would take 4 KiB or more at once. A call that stops at that bound with input left sets the
    // deadline to its own instant.
    const CHUNK: usize = 128;
    const WINDOW: usize = 8192;
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let window = Window::bytes(u32::try_from(WINDOW).unwrap());
    let chunk = u32::try_from(CHUNK).unwrap();
    let mut side = Side::start(
        shell("exec cat >/dev/null"),
        true,
        Options::new().cgroup(&cgroup, Scope::Execution),
        Limits::new(chunk, window, Window::bytes(4096)).unwrap(),
        timers(),
    );
    side.input = vec![b'i'; WINDOW];
    let mut buffer = [0u8; 4096];
    let now = Instant::now();
    while let Some(event) = side.binding.next(now, &mut buffer) {
        side.handle(now, event, &buffer);
    }
    while side.sent < WINDOW {
        assert!(
            side.serve(now, &mut buffer),
            "the peer hands its input over"
        );
    }
    assert_eq!(side.binding.runner().buffered(), WINDOW);
    let mut paused = false;
    let patience = Instant::now() + PATIENCE;
    while side.binding.runner().buffered() > 0 {
        assert!(
            Instant::now() < patience,
            "the program took the input in time"
        );
        let before = side.binding.runner().buffered();
        let now = Instant::now();
        let event = side.binding.next(now, &mut buffer);
        let written = before - side.binding.runner().buffered();
        assert!(written <= STEPS * CHUNK, "one call wrote {written} bytes");
        match event {
            Some(event) => side.handle(now, event, &buffer),
            // A pipe takes a write of a chunk, which is atomic, whole or not at all, so a call
            // that wrote this much wrote `STEPS` times.
            None if written == STEPS * CHUNK && side.binding.runner().buffered() > 0 => {
                assert_eq!(side.binding.deadline(), Some(now));
                paused = true;
            }
            None => wait(&[&mut side], patience),
        }
    }
    assert!(paused, "a call stopped at the bound with input left");
    let outcome = finish(&mut side);
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert_eq!(outcome.cause(), None);
    assert!(outcome.is_reusable());
}

#[test]
fn output_flows_while_the_caller_hands_over_input_before_every_call() {
    // The program writes its output once the test lets it, which waits in its pipe, then reads
    // its input to its end. Once the peer knows that the program started, which its output
    // waits for, the caller takes one event of the binding at a time, and before each, hands the
    // binding the input that the peer's credit allows, which the program's pipe takes at once,
    // so that every call frees credit. The source holds a page of input, which any pipe takes
    // whole, so no write waits for the program to read: the output must arrive before the
    // source runs out.
    const INPUT: usize = 4096;
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let go = temporary.path().join("go");
    let written = temporary.path().join("written");
    let mut side = Side::start(
        shell(&format!(
            "while [ ! -e '{}' ]; do sleep 0.01; done; printf ready; : >'{}'; \
             exec cat >/dev/null",
            go.display(),
            written.display()
        )),
        true,
        Options::new().cgroup(&cgroup, Scope::Execution),
        Limits::new(16, Window::bytes(64), Window::bytes(4096)).unwrap(),
        timers(),
    );
    let mut buffer = [0u8; 4096];
    let patience = Instant::now() + PATIENCE;
    while !side.peer.has_started() {
        assert!(Instant::now() < patience, "the program started in time");
        let now = Instant::now();
        match side.binding.next(now, &mut buffer) {
            Some(event) => side.handle(now, event, &buffer),
            None => wait(&[&mut side], patience),
        }
    }
    std::fs::write(&go, b"").unwrap();
    while !written.exists() {
        assert!(Instant::now() < patience, "the program wrote in time");
        std::thread::sleep(Duration::from_millis(1));
    }
    side.input = vec![b'i'; INPUT];
    while side.output[0].is_empty() {
        assert!(
            side.sent < INPUT,
            "the output arrived while the input flowed"
        );
        let now = Instant::now();
        side.serve(now, &mut buffer);
        if let Some(event) = side.binding.next(now, &mut buffer) {
            side.handle(now, event, &buffer);
        }
    }
    assert!(side.sent > 0);
    // The source ends its input there, and the program ends with it.
    side.input.truncate(side.sent);
    let outcome = finish(&mut side);
    assert_eq!(side.output[0], b"ready");
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert_eq!(outcome.cause(), None);
    assert!(!outcome.is_truncated(Stream::Stdout));
    assert!(outcome.is_reusable());
}

/// A process that escaped its execution, which the test kills when it ends, however it ends,
/// unless the process ended already.
struct Stray((ProcessId, sys::FileDescriptor));

impl Drop for Stray {
    /// Kills the process unless its pidfd says that it ended.
    fn drop(&mut self) {
        let (process, pidfd) = &self.0;
        if sys::readable_or_hung_up(&[pidfd.borrowed()], 0).is_ok_and(|ended| ended == [false]) {
            let _ = sys::kill_process(*process);
        }
    }
}

/// Waits until the program names its descendant in `file`, by its ID on one line, and returns
/// the descendant.
fn stray(file: &Path) -> Stray {
    let started = Instant::now();
    loop {
        if let Ok(line) = std::fs::read(file)
            && line.ends_with(b"\n")
        {
            return Stray(descendant(&line));
        }
        assert!(
            started.elapsed() < PATIENCE,
            "the program named its descendant in time"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_pipe_writer_that_escapes_the_kill_fences_its_execution_in_time_and_holds_no_other() {
    // The fake cgroup kills nothing: the program's descendant, in a session of its own, writes
    // to the program's standard output as fast as it can, and outlives the program's
    // cancellation, as a process that escaped the cgroup would. Its sink takes nothing, so its
    // execution discards what it writes once the grace ends, and gives up once the settlement
    // time after the grace ends too.
    let temporary = tempfile::tempdir().unwrap();
    let escaped_cgroup = fake_cgroup(&temporary.path().join("escaped"), "0\n");
    let other_cgroup = fake_cgroup(&temporary.path().join("other"), "0\n");
    let writer = temporary.path().join("writer");
    let settlement = Duration::from_millis(500);
    let mut escaped = Side::start(
        shell(&format!(
            "setsid cat /dev/zero 2>/dev/null & echo $! >'{}'; exec sleep 60",
            writer.display()
        )),
        false,
        Options::new().cgroup(&escaped_cgroup, Scope::Execution),
        limits(64 * 1024),
        Timers::new(GRACE, settlement),
    );
    escaped.stalled = true;
    let _stray = stray(&writer);
    drive(&mut [&mut escaped], |sides| {
        sides[0].peer.buffered() == 64 * 1024
    });
    // Another execution streams meanwhile, and keeps streaming after a MiB.
    let mut other = Side::start(
        shell("seq 1 600000"),
        false,
        Options::new().cgroup(&other_cgroup, Scope::Execution),
        limits(64 * 1024),
        timers(),
    );
    drive(&mut [&mut escaped, &mut other], |sides| {
        sides[1].output[0].len() > 1 << 20
    });
    let streamed = other.output[0].len();
    let cancelled = Instant::now();
    escaped.peer.cancel(cancelled);
    let mut finished = None;
    drive(&mut [&mut escaped, &mut other], |sides| {
        if sides[0].is_finished() {
            finished.get_or_insert_with(Instant::now);
        }
        finished.is_some() && sides[1].output[0].len() > streamed
    });
    let finished = finished.unwrap();
    assert!(
        finished <= cancelled + GRACE + settlement + SLACK,
        "took {:?}",
        finished - cancelled
    );
    // The program ended and its cgroup settled, but its output never did, so the execution is
    // fenced, and its standard output truncated.
    let terminal = escaped.terminal.unwrap();
    assert_eq!(terminal.cause(), Some(Cause::Cancelled));
    assert_eq!(terminal.status(), Some(Status::Signaled(9)));
    assert!(terminal.is_truncated(Stream::Stdout));
    assert!(!terminal.is_reusable());
    assert!(!escaped.outcome().unwrap().is_reusable());
    // The other execution still finishes whole.
    let outcome = finish(&mut other);
    assert_eq!(other.output[0], sequence(600_000));
    assert!(!outcome.is_truncated(Stream::Stdout));
    assert!(outcome.is_reusable());
}

#[test]
fn an_execution_that_does_not_settle_gives_up_in_time_on_the_output_that_a_descendant_holds() {
    // The program ends at once, while a descendant in a session of its own holds its standard
    // output, which nothing ends: neither the fake cgroup, which kills nothing and always counts
    // a process, nor the execution without a cgroup, whose settlement fails at once. The
    // execution does not settle, so the runner gives up on the stream the settlement time and
    // the grace after the program's end, and still sends what it read.
    let settlement = Duration::from_millis(300);
    let temporary = tempfile::tempdir().unwrap();
    let occupied = fake_cgroup(temporary.path(), "1\n");
    for cgroup in [Some(&occupied), None] {
        let options = match cgroup {
            Some(cgroup) => Options::new().cgroup(cgroup, Scope::Execution),
            None => Options::new(),
        };
        let mut side = Side::start(
            shell("setsid sleep 60 2>/dev/null & echo $!"),
            false,
            options,
            limits(16 * 1024),
            Timers::new(GRACE, settlement),
        );
        drive(&mut [&mut side], |sides| {
            sides[0].output[0].ends_with(b"\n") && sides[0].binding.execution().status().is_some()
        });
        let exited = Instant::now();
        let _holder = Stray(descendant(&side.output[0]));
        let outcome = finish(&mut side);
        let finished = Instant::now();
        assert!(
            finished <= exited + settlement + GRACE + SLACK,
            "took {:?}",
            finished - exited
        );
        let terminal = side.terminal.unwrap();
        assert_eq!(outcome, terminal);
        assert_eq!(terminal.status(), Some(Status::Exited(0)));
        assert_eq!(terminal.cause(), None);
        assert!(
            terminal.is_truncated(Stream::Stdout),
            "with a cgroup: {}",
            cgroup.is_some()
        );
        assert!(!terminal.is_truncated(Stream::Stderr));
        assert!(!terminal.is_reusable());
        assert!(side.failures.is_empty(), "{:?}", side.failures);
    }
}

#[test]
fn an_execution_that_does_not_settle_loses_nothing_of_the_output_that_no_process_holds() {
    // The program leaves nothing behind, and ends once its output fits in the window of its
    // stalled sink, the runner's queue, and its pipe, which holds the rest, about 1 KiB, as any
    // pipe does, but the execution does not settle: the fake cgroup always counts a process, and
    // the execution without a cgroup fails its settlement at once. Its streams hung up, so the
    // runner waits for the sink long after the settlement time and the grace, and loses nothing.
    let settlement = Duration::from_millis(100);
    let temporary = tempfile::tempdir().unwrap();
    let occupied = fake_cgroup(temporary.path(), "1\n");
    for cgroup in [Some(&occupied), None] {
        let options = match cgroup {
            Some(cgroup) => Options::new().cgroup(cgroup, Scope::Execution),
            None => Options::new(),
        };
        let mut side = Side::start(
            shell("seq 1 7000; seq 1 100 >&2"),
            false,
            options,
            limits(16 * 1024),
            Timers::new(GRACE, settlement),
        );
        side.stalled = true;
        drive(&mut [&mut side], |sides| {
            sides[0].binding.execution().status().is_some()
        });
        // Past the time at which the runner gives up on the streams that a process may hold.
        drive_for(&mut [&mut side], (settlement + GRACE) * 2);
        let with = cgroup.is_some();
        assert!(!side.is_finished(), "with a cgroup: {with}");
        side.stalled = false;
        let outcome = finish(&mut side);
        assert_eq!(side.output[0], sequence(7000), "with a cgroup: {with}");
        assert_eq!(side.output[1], sequence(100), "with a cgroup: {with}");
        assert_eq!(outcome.status(), Some(Status::Exited(0)));
        assert_eq!(outcome.cause(), None);
        assert!(
            !outcome.is_truncated(Stream::Stdout) && !outcome.is_truncated(Stream::Stderr),
            "with a cgroup: {with}"
        );
        assert!(!outcome.is_reusable());
        assert!(side.failures.is_empty(), "{:?}", side.failures);
    }
}

#[test]
fn a_due_settlement_check_runs_while_output_drains_slowly() {
    // The program writes 32 KiB, which its pipe holds, and ends. The adapter takes 64 bytes at a
    // time, and 2 ms to send each message, so that every call has output to return for longer
    // than the settlement time after the program's end: the settlement's due checks still run,
    // and the execution settles.
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path(), "0\n");
    let settlement = Duration::from_millis(100);
    let mut side = Side::start(
        shell("head -c 32768 /dev/zero"),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
        limits(16 * 1024),
        Timers::new(GRACE, settlement),
    );
    let mut buffer = [0u8; 64];
    let started = Instant::now();
    let mut exited = None;
    while !side.is_finished() {
        assert!(
            started.elapsed() < PATIENCE,
            "the execution finished in time"
        );
        let now = Instant::now();
        match side.binding.next(now, &mut buffer) {
            Some(event) => {
                side.handle(now, event, &buffer);
                std::thread::sleep(Duration::from_millis(2));
            }
            None => wait(&[&mut side], started + PATIENCE),
        }
        side.serve(now, &mut buffer);
        if exited.is_none() && side.binding.execution().status().is_some() {
            exited = Some(Instant::now());
        }
    }
    // The output drained for longer than the settlement time after the program's end.
    let drained = Instant::now() - exited.unwrap();
    assert!(drained > settlement, "drained in {drained:?}");
    assert_eq!(side.output[0], vec![0; 32768]);
    let outcome = side.outcome().unwrap();
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert!(outcome.is_reusable());
    assert!(!outcome.is_truncated(Stream::Stdout));
    assert!(side.failures.is_empty(), "{:?}", side.failures);
}
