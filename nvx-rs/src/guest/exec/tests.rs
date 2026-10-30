//! Checks of executions of real programs.

use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags};

use super::*;
use crate::guest::cgroup::Layout;
use crate::guest::spawn::{Command, LaunchStage, shell};

/// How long a test waits for an execution before it fails.
const PATIENCE: Duration = Duration::from_secs(20);

/// The most bytes that one event carries in these tests: the size of the buffer that they lend
/// the execution, and the allowance that they grant each stream at a time.
const MAX_EVENT_BYTES: usize = 4096;

/// Spawns `command` with new pipes, its standard input piped if `input`, in a process group of
/// its own, and starts its execution with `options`.
fn start(command: Command, input: bool, options: Options) -> Execution {
    let (pipes, ends) = Pipes::new(input).unwrap();
    let mut command = command
        .stdout(ends.stdout)
        .stderr(ends.stderr)
        .new_process_group();
    if let Some(stdin) = ends.stdin {
        command = command.stdin(stdin);
    }
    Execution::new(command.spawn().unwrap(), pipes, options.process_group()).unwrap()
}

/// Makes `SIGPIPE` end the process, as a caller's default disposition does.
fn default_sigpipe() {
    sys::default_sigpipe();
}

/// What a test saw of one execution.
#[derive(Debug, Default)]
struct Record {
    /// The launch report, once it arrived.
    launch: Option<Option<LaunchFailure>>,
    /// The bytes of each stream, in order.
    output: [Vec<u8>; 2],
    /// The streams of the output events, in order.
    order: Vec<Stream>,
    /// The streams that closed, in order.
    closed: Vec<Stream>,
    /// The bytes that the execution discarded, from its events.
    discarded: [u64; 2],
    /// The kill failures, in order.
    kill_failures: Vec<(KillTarget, Errno)>,
    /// The outcome, once the execution ended.
    outcome: Option<Outcome>,
}

impl Record {
    /// Takes the execution's events until it has no step to take, and returns whether it took
    /// one. With `regrant`, it grants each stream's output back, as a sink that takes everything
    /// would.
    fn pump(&mut self, execution: &mut Execution, regrant: bool) -> bool {
        let mut buffer = [0u8; MAX_EVENT_BYTES];
        let mut progressed = false;
        while let Some(event) = execution.next_event(&mut buffer).unwrap() {
            progressed = true;
            match event {
                Event::Launch(report) => {
                    assert!(self.launch.is_none(), "one launch report");
                    self.launch = Some(report.unwrap());
                }
                Event::Output(stream, bytes) => {
                    assert!(!bytes.is_empty());
                    self.output[stream.index()].extend_from_slice(bytes);
                    self.order.push(stream);
                    let length = bytes.len();
                    if regrant {
                        execution.grant(stream, length);
                    }
                }
                Event::Discarded(stream, length) => {
                    // Discarding reads into the same buffer as output.
                    assert!((1..=MAX_EVENT_BYTES).contains(&length), "{length}");
                    self.discarded[stream.index()] += length as u64;
                }
                Event::Closed(stream) => {
                    assert!(!self.closed.contains(&stream), "{stream:?} closes once");
                    self.closed.push(stream);
                }
                Event::ReadFailed(stream, error) => panic!("reading {stream:?} failed: {error}"),
                Event::KillFailed(target, error) => {
                    self.kill_failures.push((target, sys::errno_of(&error)));
                }
                Event::Ended(outcome) => {
                    assert!(self.launch.is_some(), "the launch report comes first");
                    assert_eq!(self.closed.len(), 2, "both streams close first");
                    self.outcome = Some(outcome);
                }
            }
        }
        progressed
    }

    /// Pumps `execution` and waits on it, as a caller's loop does, until `done` holds.
    fn drive_until(
        &mut self,
        execution: &mut Execution,
        regrant: bool,
        mut done: impl FnMut(&Self, &Execution) -> bool,
    ) {
        let started = Instant::now();
        loop {
            self.pump(execution, regrant);
            if done(self, execution) {
                return;
            }
            assert!(
                started.elapsed() < PATIENCE,
                "the execution moved on in time"
            );
            wait_any(&[execution]);
        }
    }
}

/// Waits on the descriptors of `executions` until one polls ready or the earliest deadline
/// passes, as a caller's loop does, and never wakes on its own sooner than 10 seconds.
fn wait_any(executions: &[&Execution]) {
    let descriptors = executions
        .iter()
        .flat_map(|execution| execution.descriptors().map(|(_, descriptor)| descriptor))
        .collect::<Vec<_>>();
    let deadline = executions
        .iter()
        .filter_map(|execution| execution.deadline())
        .min();
    let timeout = deadline.map_or(10_000, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 10_000) as u16
    });
    match sys::readable_or_hung_up(&descriptors, timeout) {
        Ok(_) | Err(Errno::EINTR) => {}
        Err(error) => panic!("poll failed: {error}"),
    }
}

/// Drives `execution` until it ends, granting each stream's output back as it arrives, and
/// returns what it saw.
fn run(execution: &mut Execution) -> Record {
    let mut record = Record::default();
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    record.drive_until(execution, true, |record, _| record.outcome.is_some());
    assert!(execution.descriptors().next().is_none());
    assert!(execution.next_event(&mut [0u8; 16]).unwrap().is_none());
    record
}

/// Returns the signal that ended `status`, if one did.
fn signal_of(status: ExitStatus) -> Option<i32> {
    status.signal()
}

#[test]
fn a_program_s_launch_output_and_status_are_reported() {
    let mut execution = start(
        shell("printf out; printf err >&2; exit 3"),
        false,
        Options::new(),
    );
    let record = run(&mut execution);
    assert_eq!(record.launch, Some(None));
    assert_eq!(record.output, [b"out".to_vec(), b"err".to_vec()]);
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.status().code(), Some(3));
    assert_eq!(outcome.cause(), None);
    assert_eq!(outcome.discarded(Stream::Stdout), 0);
    assert_eq!(outcome.discarded(Stream::Stderr), 0);
    assert!(record.kill_failures.is_empty());
}

#[test]
fn a_failed_launch_stage_is_reported_and_the_execution_still_ends() {
    let mut execution = start(
        shell("exit 0").working_directory(c"/nonexistent/directory".to_owned()),
        false,
        Options::new(),
    );
    let record = run(&mut execution);
    let failure = record.launch.unwrap().expect("the launch failed");
    assert_eq!(failure.stage(), LaunchStage::WorkingDirectory);
    assert_eq!(failure.raw_os_error(), Errno::ENOENT as i32);
    assert_eq!(record.outcome.unwrap().status().code(), Some(125));
}

#[test]
fn an_execution_without_output_ends_through_its_descriptors_alone() {
    // Only the launch report's hangup and the pidfd wake the loop, which would otherwise wait
    // ten seconds in each poll.
    let started = Instant::now();
    let mut execution = start(
        shell("exec >/dev/null 2>&1; sleep 0.2"),
        false,
        Options::new(),
    );
    let sources = execution
        .descriptors()
        .map(|(source, _)| source)
        .collect::<Vec<_>>();
    assert_eq!(sources, [Source::Launch, Source::Exit]);
    let record = run(&mut execution);
    assert!(record.outcome.unwrap().status().success());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn concurrent_executions_progress_independently() {
    let mut executions = (0..16)
        .map(|index| {
            start(
                shell(&format!("cat; printf {index} >&2")),
                true,
                Options::new(),
            )
        })
        .collect::<Vec<_>>();
    let mut records = (0..16).map(|_| Record::default()).collect::<Vec<_>>();
    for (index, execution) in executions.iter_mut().enumerate() {
        execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
        execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
        let input = format!("input {index}\n");
        assert_eq!(
            execution.write_input(input.as_bytes()).unwrap(),
            input.len()
        );
        execution.close_input();
        assert!(execution.input().is_none());
    }
    let started = Instant::now();
    while records.iter().any(|record| record.outcome.is_none()) {
        for (execution, record) in executions.iter_mut().zip(&mut records) {
            record.pump(execution, true);
        }
        assert!(started.elapsed() < PATIENCE);
        let waiting = executions
            .iter()
            .zip(&records)
            .filter(|(_, record)| record.outcome.is_none())
            .map(|(execution, _)| execution)
            .collect::<Vec<_>>();
        if !waiting.is_empty() {
            wait_any(&waiting);
        }
    }
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.output[0], format!("input {index}\n").as_bytes());
        assert_eq!(record.output[1], index.to_string().as_bytes());
        assert!(record.outcome.unwrap().status().success());
    }
}

#[test]
fn a_stalled_sink_stalls_only_its_own_execution_and_loses_nothing() {
    const LENGTH: usize = 256 * 1024;
    let mut stalled = start(
        shell(&format!("head -c {LENGTH} /dev/zero")),
        false,
        Options::new(),
    );
    let mut other = start(shell("printf done"), false, Options::new());
    stalled.grant(Stream::Stderr, MAX_EVENT_BYTES);
    other.grant(Stream::Stdout, MAX_EVENT_BYTES);
    other.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut stalled_record = Record::default();
    let mut other_record = Record::default();
    let started = Instant::now();
    while other_record.outcome.is_none() {
        stalled_record.pump(&mut stalled, false);
        other_record.pump(&mut other, true);
        assert!(started.elapsed() < PATIENCE);
        if other_record.outcome.is_none() {
            wait_any(&[&stalled, &other]);
        }
    }
    assert_eq!(other_record.output[0], b"done");
    // The stalled program fills its pipe and blocks, and its execution waits for the caller.
    std::thread::sleep(Duration::from_millis(200));
    while stalled_record.launch.is_none() {
        assert!(stalled_record.pump(&mut stalled, false));
    }
    assert!(!stalled_record.pump(&mut stalled, false));
    assert!(stalled.status().is_none());
    assert!(stalled_record.output[0].is_empty());
    assert!(
        stalled
            .descriptors()
            .all(|(source, _)| source != Source::Output(Stream::Stdout))
    );
    // Once its sink takes output again, every byte arrives.
    stalled.grant(Stream::Stdout, MAX_EVENT_BYTES);
    stalled_record.drive_until(&mut stalled, true, |record, _| record.outcome.is_some());
    assert_eq!(stalled_record.output[0].len(), LENGTH);
    assert!(stalled_record.output[0].iter().all(|&byte| byte == 0));
    let outcome = stalled_record.outcome.unwrap();
    assert!(outcome.status().success());
    assert_eq!(outcome.discarded(Stream::Stdout), 0);
}

#[test]
fn cancellation_ends_a_stalled_program_and_the_caller_discards_the_rest() {
    // The program fills its pipe with the first page, which any pipe holds, before it says so.
    let mut execution = start(
        shell("head -c 4096 /dev/zero; printf ready >&2; exec head -c 262144 /dev/zero"),
        true,
        Options::new(),
    );
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    record.drive_until(&mut execution, false, |record, _| {
        record.output[1] == b"ready"
    });
    assert!(execution.input().is_some());
    assert!(execution.cancel());
    assert!(!execution.cancel());
    assert!(!execution.terminate());
    assert!(execution.input().is_none());
    assert_eq!(
        sys::errno_of(&execution.write_input(b"late").unwrap_err()),
        Errno::EPIPE
    );
    // The program ends at once, but its pipe still holds what it wrote, which the caller has
    // yet to take.
    record.drive_until(&mut execution, false, |_, execution| {
        execution.status().is_some()
    });
    assert_eq!(signal_of(execution.status().unwrap()), Some(9));
    assert!(!record.pump(&mut execution, false));
    assert!(record.outcome.is_none());
    execution.discard(Stream::Stdout);
    record.drive_until(&mut execution, false, |record, _| record.outcome.is_some());
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    assert_eq!(signal_of(outcome.status()), Some(9));
    assert!(outcome.discarded(Stream::Stdout) > 0);
    assert_eq!(
        outcome.discarded(Stream::Stdout),
        record.discarded[Stream::Stdout.index()]
    );
    assert!(record.output[0].is_empty());
    assert!(record.kill_failures.is_empty());
}

#[test]
fn stdin_eof_follows_every_byte_that_a_write_took() {
    let input = (0..300_000u32)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let mut execution = start(
        Command::new(c"/bin/cat".to_owned(), vec![c"cat".to_owned()]),
        true,
        Options::new(),
    );
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    let mut written = 0;
    let started = Instant::now();
    while record.outcome.is_none() {
        while written < input.len() {
            match execution.write_input(&input[written..]) {
                Ok(length) => written += length,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("writing the input failed: {error}"),
            }
        }
        if written == input.len() && execution.input().is_some() {
            execution.close_input();
        }
        record.pump(&mut execution, true);
        assert!(started.elapsed() < PATIENCE);
        if record.outcome.is_some() {
            break;
        }
        // Wait for output, or for `cat` to read the full input pipe.
        let mut descriptors = execution
            .descriptors()
            .map(|(_, descriptor)| PollFd::new(descriptor, PollFlags::POLLIN))
            .collect::<Vec<_>>();
        if let Some(input) = execution.input() {
            descriptors.push(PollFd::new(input, PollFlags::POLLOUT));
        }
        let _ = nix::poll::poll(&mut descriptors, 1_000u16);
    }
    assert_eq!(record.output[0], input);
    assert!(record.outcome.unwrap().status().success());
}

#[test]
fn a_program_that_closes_its_input_ends_the_writes_without_sigpipe() {
    // A `SIGPIPE` would end this process under its default disposition, so the test runs in a
    // process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_EXEC_SIGPIPE_TEST_CHILD",
        "guest::exec::tests::a_program_that_closes_its_input_ends_the_writes_without_sigpipe",
    ) {
        return;
    }
    default_sigpipe();
    let mut execution = start(shell("exec 0<&-; sleep 0.3"), true, Options::new());
    let started = Instant::now();
    // The writes meet the closed pipe, at the latest once the pipe is full.
    let error = loop {
        match execution.write_input(&[1u8; 4096]) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => break sys::errno_of(&error),
        }
        assert!(started.elapsed() < PATIENCE);
    };
    assert_eq!(error, Errno::EPIPE);
    assert!(execution.input().is_none());
    assert_eq!(
        sys::errno_of(&execution.write_input(b"late").unwrap_err()),
        Errno::EPIPE
    );
    assert!(run(&mut execution).outcome.unwrap().status().success());
}

#[test]
fn concurrent_writers_meet_closed_inputs_without_sigpipe() {
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_EXEC_SIGPIPE_THREADS_TEST_CHILD",
        "guest::exec::tests::concurrent_writers_meet_closed_inputs_without_sigpipe",
    ) {
        return;
    }
    default_sigpipe();
    let mut executions = (0..4)
        .map(|_| start(shell("exec 0<&-; sleep 0.3"), true, Options::new()))
        .collect::<Vec<_>>();
    std::thread::scope(|scope| {
        for execution in &mut executions {
            scope.spawn(move || {
                let started = Instant::now();
                loop {
                    match execution.write_input(&[1u8; 4096]) {
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => {
                            assert_eq!(sys::errno_of(&error), Errno::EPIPE);
                            break;
                        }
                    }
                    assert!(started.elapsed() < PATIENCE);
                }
            });
        }
    });
    for execution in &mut executions {
        assert!(run(execution).outcome.unwrap().status().success());
    }
}

#[test]
fn a_timeout_ends_the_program() {
    let mut execution = start(
        shell("sleep 30"),
        false,
        Options::new().timeout(Duration::from_millis(100)),
    );
    assert!(execution.deadline().is_some());
    let started = Instant::now();
    let record = run(&mut execution);
    assert!(started.elapsed() < Duration::from_secs(10));
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.cause(), Some(Cause::TimedOut));
    assert_eq!(signal_of(outcome.status()), Some(9));
    assert!(execution.deadline().is_none());
}

#[test]
fn a_timeout_whose_signals_the_kernel_refuses_fires_once() {
    // The kernel refuses the execution's signals, as it does those that the caller may not
    // send, so the program outlives its timeout.
    super::REFUSED.set(Some(Errno::EPERM));
    let mut execution = start(
        shell("sleep 0.3; echo done"),
        false,
        Options::new().timeout(Duration::from_millis(50)),
    );
    let record = run(&mut execution);
    super::REFUSED.set(None);
    // The timeout ends the program once, and the execution goes on with its output.
    assert_eq!(
        record.kill_failures,
        [
            (KillTarget::Program, Errno::EPERM),
            (KillTarget::ProcessGroup, Errno::EPERM)
        ]
    );
    assert_eq!(record.output[0], b"done\n");
    let outcome = record.outcome.unwrap();
    assert!(outcome.status().success());
    assert_eq!(outcome.cause(), Some(Cause::TimedOut));
}

#[test]
fn a_disconnection_discards_the_output_and_ends_the_execution() {
    let mut execution = start(shell("yes"), true, Options::new());
    execution.grant(Stream::Stdout, 10_000);
    let mut record = Record::default();
    record.drive_until(&mut execution, false, |record, _| {
        record.output[0].len() == 10_000
    });
    execution.disconnect();
    assert!(execution.input().is_none());
    record.drive_until(&mut execution, false, |record, _| record.outcome.is_some());
    assert_eq!(record.output[0].len(), 10_000);
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.cause(), Some(Cause::Disconnected));
    assert_eq!(signal_of(outcome.status()), Some(9));
    assert_eq!(
        outcome.discarded(Stream::Stdout),
        record.discarded[Stream::Stdout.index()]
    );
}

#[test]
fn causes_count_only_while_the_program_runs() {
    let mut execution = start(shell("printf x"), false, Options::new());
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    record.drive_until(&mut execution, false, |_, execution| {
        execution.status().is_some()
    });
    // The program has ended, but its output waits for the caller.
    assert!(record.outcome.is_none());
    assert!(!execution.cancel());
    assert!(!execution.terminate());
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    assert_eq!(record.output[0], b"x");
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.cause(), None);
    assert!(outcome.status().success());
    assert!(!execution.cancel());
}

#[test]
fn an_observed_status_ends_the_execution() {
    let mut execution = start(shell("exit 4"), true, Options::new());
    let id = execution.id();
    // The caller's own collector takes the status, as a PID 1 does, and hands it over.
    let mut collector = Child::new(id).unwrap();
    let status = collector.wait().unwrap();
    assert!(!execution.observe(id + 1, status));
    assert!(execution.input().is_some());
    assert!(execution.observe(id, status));
    assert!(execution.input().is_none());
    assert!(!execution.observe(id, status));
    let record = run(&mut execution);
    assert_eq!(record.outcome.unwrap().status().code(), Some(4));
}

#[test]
fn a_caller_that_reaps_is_the_program_s_only_collector() {
    let mut execution = start(shell("exit 6"), false, Options::new().caller_reaps());
    assert!(
        execution
            .descriptors()
            .all(|(source, _)| source != Source::Exit)
    );
    let id = execution.id();
    let process = ProcessId::from_positive(i64::from(id)).unwrap();
    let started = Instant::now();
    while !sys::has_exited(process).unwrap() {
        assert!(started.elapsed() < PATIENCE, "the program ended in time");
        std::thread::sleep(Duration::from_millis(1));
    }
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    record.drive_until(&mut execution, true, |record, _| {
        record.launch.is_some() && record.closed.len() == 2
    });
    // The execution took every other step, but left the status, which its collector finds.
    assert!(execution.status().is_none());
    assert!(execution.descriptors().next().is_none());
    let status = Child::new(id).unwrap().wait().unwrap();
    assert!(execution.observe(id, status));
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.status().code(), Some(6));
    assert_eq!(outcome.cause(), None);
}

/// Returns a command whose program starts a descendant in its process group, which outlives the
/// program, writes the descendant's ID to `file`, and exits with `code`.
fn leave_a_descendant(file: &Path, code: u8) -> Command {
    shell(&format!(
        "sleep 30 </dev/null >/dev/null 2>&1 & echo $! >'{}'; exit {code}",
        file.display()
    ))
}

/// The descendant that [`leave_a_descendant`] started, with a pidfd that polls readable once it
/// ends.
struct Descendant(ProcessId, FileDescriptor);

impl Descendant {
    /// Opens the descendant whose ID `file` holds.
    fn open(file: &Path) -> Self {
        let id = std::fs::read_to_string(file).unwrap();
        let process = ProcessId::from_positive(id.trim().parse().unwrap()).unwrap();
        Self(process, sys::open_pidfd(process).unwrap())
    }

    /// Asserts that the descendant still runs, as it does unless a signal to its process group
    /// reached it, which would end it at once, and ends it.
    fn assert_runs(self) {
        assert_eq!(
            sys::readable_or_hung_up(&[self.1.borrowed()], 200).unwrap(),
            [false]
        );
        sys::kill_process(self.0).unwrap();
    }

    /// Asserts that the descendant ends, as a signal to its process group ends it.
    fn assert_ends(self) {
        let patience = u16::try_from(PATIENCE.as_millis()).unwrap();
        assert_eq!(
            sys::readable_or_hung_up(&[self.1.borrowed()], patience).unwrap(),
            [true]
        );
    }
}

#[test]
fn ending_the_program_ends_the_process_group_that_it_leads() {
    let temporary = tempfile::tempdir().unwrap();
    let file = temporary.path().join("descendant");
    // Without a cgroup, only a signal to the program's process group reaches the descendant.
    let mut execution = start(
        shell(&format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! >'{}'; echo started; exec sleep 30",
            file.display()
        )),
        false,
        Options::new(),
    );
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    record.drive_until(&mut execution, true, |record, _| {
        record.output[0] == b"started\n"
    });
    let descendant = Descendant::open(&file);
    assert!(execution.cancel());
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    let outcome = record.outcome.unwrap();
    assert_eq!(signal_of(outcome.status()), Some(9));
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    descendant.assert_ends();
}

#[test]
fn a_status_that_another_collector_took_first_is_left_to_it() {
    let temporary = tempfile::tempdir().unwrap();
    let file = temporary.path().join("descendant");
    // Before the execution looks for the status itself, the caller may cancel it, or wait for it.
    for (cancel_first, wait_first) in [(false, false), (true, false), (false, true)] {
        let mut execution = start(
            leave_a_descendant(&file, 5),
            false,
            Options::new().timeout(Duration::from_secs(3600)),
        );
        let id = execution.id();
        // A collector of the caller's, such as a reaper on another thread, takes the status
        // before the execution looks for it. The program's ID may name another process from
        // then on, and so may that of its process group once the descendant leaves it.
        let status = Child::new(id).unwrap().wait().unwrap();
        let descendant = Descendant::open(&file);
        if cancel_first {
            assert!(!execution.cancel(), "nothing to cancel");
        }
        if wait_first {
            let error = execution.wait().unwrap_err();
            assert_eq!(error.raw_os_error(), Some(Errno::ECHILD as i32));
        }
        execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
        execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
        let mut record = Record::default();
        let mut buffer = [0u8; 64];
        let mut failures = 0;
        let started = Instant::now();
        // The descendant holds the output until it redirects its own.
        while record.closed.len() < 2 {
            match execution.next_event(&mut buffer) {
                Ok(Some(Event::Launch(report))) => record.launch = Some(report.unwrap()),
                Ok(Some(Event::Closed(stream))) => record.closed.push(stream),
                Ok(Some(event)) => panic!("{event:?}"),
                Ok(None) => {
                    assert!(started.elapsed() < PATIENCE, "the output closed in time");
                    wait_any(&[&execution]);
                }
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(Errno::ECHILD as i32));
                    failures += 1;
                }
            }
        }
        assert!(execution.next_event(&mut buffer).unwrap().is_none());
        // Collecting fails once, unless the cancellation or the wait found the status gone first.
        assert_eq!(
            failures,
            usize::from(!cancel_first && !wait_first),
            "{cancel_first} {wait_first}"
        );
        assert!(execution.status().is_none());
        assert!(execution.deadline().is_none());
        assert!(
            execution
                .descriptors()
                .all(|(source, _)| source != Source::Exit)
        );
        assert!(!execution.cancel());
        execution.disconnect();
        descendant.assert_runs();
        assert!(execution.observe(id, status));
        record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
        let outcome = record.outcome.unwrap();
        assert_eq!(outcome.status().code(), Some(5));
        assert_eq!(outcome.cause(), None);
    }
}

#[test]
fn a_start_after_another_collector_took_the_status_ends_nothing_by_id() {
    let temporary = tempfile::tempdir().unwrap();
    let file = temporary.path().join("descendant");
    let (pipes, ends) = Pipes::new(false).unwrap();
    let spawned = leave_a_descendant(&file, 0)
        .stdout(ends.stdout)
        .stderr(ends.stderr)
        .new_process_group()
        .spawn()
        .unwrap();
    let status = Child::new(spawned.id()).unwrap().wait().unwrap();
    assert!(status.success());
    let descendant = Descendant::open(&file);
    let error = Execution::new(spawned, pipes, Options::new().process_group()).unwrap_err();
    assert_eq!(sys::errno_of(&error), Errno::ESRCH);
    descendant.assert_runs();
}

#[test]
fn the_program_s_end_closes_its_input_for_a_descendant_that_reads_it() {
    // Without a cgroup, nothing ends the descendant, which holds the output open until its
    // input ends. A shell gives a background job the null device as its input, unless the job
    // redirects it.
    let mut execution = start(
        shell("exec 3<&0; cat <&3 & printf started"),
        true,
        Options::new(),
    );
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    record.drive_until(&mut execution, true, |_, execution| {
        execution.status().is_some()
    });
    assert!(execution.input().is_none());
    assert_eq!(
        sys::errno_of(&execution.write_input(b"late").unwrap_err()),
        Errno::EPIPE
    );
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    assert_eq!(record.output[0], b"started");
    let outcome = record.outcome.unwrap();
    assert!(outcome.status().success());
    assert_eq!(outcome.cause(), None);
}

#[test]
fn dropping_an_unfinished_execution_ends_its_program() {
    let execution = start(shell("sleep 30"), false, Options::new());
    let id = execution.id();
    drop(execution);
    let mut child = Child::new(id).unwrap();
    assert_eq!(signal_of(child.wait().unwrap()), Some(9));
}

#[test]
fn an_unrelated_child_s_status_is_left_alone() {
    let mut unrelated = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 7"])
        .spawn()
        .unwrap();
    while !sys::has_exited(ProcessId::from_child(&unrelated)).unwrap() {
        std::thread::yield_now();
    }
    let mut execution = start(shell("printf ran"), false, Options::new());
    assert_eq!(run(&mut execution).output[0], b"ran");
    // Nothing collected the unrelated child's status, which its own wait still finds.
    assert_eq!(unrelated.wait().unwrap().code(), Some(7));
}

#[test]
fn both_streams_take_turns_while_each_has_output() {
    // Each pipe takes the program's 8 KiB whole, and the execution reads neither stream without
    // an allowance, so both hold all of their output once the program has ended.
    let mut execution = start(
        shell("head -c 8192 /dev/zero; head -c 8192 /dev/zero >&2"),
        false,
        Options::new(),
    );
    let mut record = Record::default();
    // The launch report may arrive after the status: a process that another test's thread forked
    // while the program was spawning holds the report's writer until it replaces itself with its
    // program or ends.
    record.drive_until(&mut execution, false, |record, execution| {
        record.launch.is_some() && execution.status().is_some()
    });
    assert!(record.order.is_empty());
    execution.grant(Stream::Stdout, usize::MAX);
    execution.grant(Stream::Stderr, usize::MAX);
    // Reads smaller than what each pipe holds leave both streams ready for several turns.
    let mut buffer = [0u8; 1024];
    while let Some(event) = execution.next_event(&mut buffer).unwrap() {
        match event {
            Event::Output(stream, bytes) => {
                record.output[stream.index()].extend_from_slice(bytes);
                record.order.push(stream);
            }
            Event::Closed(stream) => record.closed.push(stream),
            Event::Ended(outcome) => record.outcome = Some(outcome),
            event => panic!("{event:?}"),
        }
    }
    assert_eq!(record.output.each_ref().map(Vec::len), [8192, 8192]);
    assert_eq!(record.order.len(), 16);
    assert!(
        record.order.windows(2).all(|pair| pair[0] != pair[1]),
        "{:?}",
        record.order
    );
    assert!(record.outcome.unwrap().status().success());
}

/// Creates a fake cgroup in `root`, a workload cgroup whose `cgroup.kill` records the kills, and
/// returns its handle.
fn fake_cgroup(root: &Path) -> ExecutionCgroup {
    let layout = Layout::new(root.join("agent"), root.join("workload")).unwrap();
    std::fs::create_dir_all(layout.workload()).unwrap();
    std::fs::write(layout.workload().join("cgroup.kill"), b"").unwrap();
    ExecutionCgroup::open(&layout, None).unwrap()
}

#[test]
fn the_cgroup_is_killed_once_another_collector_took_the_status_with_the_output_held() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path());
    let file = temporary.path().join("descendant");
    // The descendant holds the program's output until it ends.
    let mut execution = start(
        shell(&format!("sleep 30 & echo $! >'{}'", file.display())),
        true,
        Options::new().cgroup(&cgroup, Scope::Execution),
    );
    let id = execution.id();
    // A collector of the caller's, such as the kernel's while `SIGCHLD` is ignored, takes the
    // status before the execution looks for it.
    let status = Child::new(id).unwrap().wait().unwrap();
    let descendant = Descendant::open(&file);
    let mut record = Record::default();
    let mut buffer = [0u8; 64];
    let started = Instant::now();
    let error = loop {
        match execution.next_event(&mut buffer) {
            Ok(Some(Event::Launch(report))) => record.launch = Some(report.unwrap()),
            Ok(Some(event)) => panic!("{event:?}"),
            Ok(None) => {
                assert!(
                    started.elapsed() < PATIENCE,
                    "the collection failed in time"
                );
                wait_any(&[&execution]);
            }
            Err(error) => break error,
        }
    };
    assert_eq!(error.raw_os_error(), Some(Errno::ECHILD as i32));
    // The program has ended, so only another process can hold its output, or read its input.
    assert_eq!(
        std::fs::read(cgroup.path().join("cgroup.kill")).unwrap(),
        b"1"
    );
    assert!(execution.input().is_none());
    // The fake cgroup ends nothing, so the test ends the descendant.
    descendant.assert_runs();
    assert!(execution.observe(id, status));
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    let outcome = record.outcome.unwrap();
    assert!(outcome.status().success());
    assert_eq!(outcome.cause(), None);
}

#[test]
fn the_cgroup_is_killed_once_the_program_ends_with_its_output_held() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path());
    let kill = cgroup.path().join("cgroup.kill");
    // The descendant holds the output open, and the fake cgroup kills nothing.
    let mut execution = start(
        shell("sleep 30 & printf $!"),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
    );
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    record.drive_until(&mut execution, true, |record, execution| {
        execution.status().is_some() && !record.output[0].is_empty()
    });
    assert_eq!(std::fs::read(&kill).unwrap(), b"1");
    assert!(record.outcome.is_none());
    let descendant = std::str::from_utf8(&record.output[0])
        .unwrap()
        .parse::<i32>()
        .unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(descendant),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    assert!(record.outcome.unwrap().status().success());
    assert!(record.kill_failures.is_empty());
}

#[test]
fn a_failed_kill_is_reported() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path());
    std::fs::remove_file(cgroup.path().join("cgroup.kill")).unwrap();
    let mut execution = start(
        shell("sleep 30"),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
    );
    assert!(execution.cancel());
    let record = run(&mut execution);
    assert_eq!(record.kill_failures[0], (KillTarget::Cgroup, Errno::ENOENT));
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    assert_eq!(signal_of(outcome.status()), Some(9));
}

#[test]
fn a_kill_never_reaches_a_cgroup_that_took_the_path_later() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path());
    let kill = cgroup.path().join("cgroup.kill");
    let mut execution = start(
        shell("sleep 30"),
        false,
        Options::new().cgroup(&cgroup, Scope::Execution),
    );
    std::fs::remove_file(&kill).unwrap();
    std::fs::remove_dir(cgroup.path()).unwrap();
    std::fs::create_dir(cgroup.path()).unwrap();
    std::fs::write(&kill, b"").unwrap();
    assert!(execution.cancel());
    let record = run(&mut execution);
    assert_eq!(record.kill_failures[0], (KillTarget::Cgroup, Errno::ENOENT));
    assert_eq!(std::fs::read(&kill).unwrap(), b"");
}

#[test]
fn a_failed_start_ends_the_program_and_its_cgroup_even_without_descriptors() {
    // Other tests could open nothing under the lowered descriptor limit, so this test runs in a
    // process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_EXEC_DESCRIPTOR_LIMIT_TEST_CHILD",
        "guest::exec::tests::a_failed_start_ends_the_program_and_its_cgroup_even_without_descriptors",
    ) {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path());
    let (pipes, ends) = Pipes::new(false).unwrap();
    let spawned = shell("sleep 30")
        .stdout(ends.stdout)
        .stderr(ends.stderr)
        .new_process_group()
        .spawn()
        .unwrap();
    let id = spawned.id();
    // Without a spare descriptor, the pidfd does not open. `new` then ends the program, closes
    // the descriptors that it took, and kills the cgroup, which opens `cgroup.kill` with one of
    // them. The process opens its lowest free descriptor next.
    let lowest = std::fs::File::open("/dev/null").unwrap().as_raw_fd();
    let previous = sys::set_descriptor_limit(lowest as _).unwrap();
    let started = Execution::new(
        spawned,
        pipes,
        Options::new()
            .process_group()
            .cgroup(&cgroup, Scope::Execution),
    );
    sys::set_descriptor_limit(previous).unwrap();
    assert_eq!(sys::errno_of(&started.unwrap_err()), Errno::EMFILE);
    assert_eq!(
        std::fs::read(cgroup.path().join("cgroup.kill")).unwrap(),
        b"1"
    );
    assert_eq!(signal_of(Child::new(id).unwrap().wait().unwrap()), Some(9));
}

#[test]
fn a_failed_start_ends_a_program_that_joined_another_process_group() {
    const TEST: &str =
        "guest::exec::tests::a_failed_start_ends_a_program_that_joined_another_process_group";
    // The test runs its own executable again as the program, with this variable naming a process
    // group and a file. The program joins that group, which leaves the group that it led,
    // creates the file to say so, and waits to be ended.
    const PROGRAM: &str = "NVX_EXEC_GROUP_JOINER_PROGRAM";
    if let Some(value) = std::env::var_os(PROGRAM) {
        let value = value.into_string().unwrap();
        let (group, joined) = value.split_once(':').unwrap();
        nix::unistd::setpgid(
            nix::unistd::Pid::from_raw(0),
            nix::unistd::Pid::from_raw(group.parse().unwrap()),
        )
        .unwrap();
        std::fs::write(joined, b"").unwrap();
        std::thread::sleep(PATIENCE);
        return;
    }
    // As the test above, it lowers the descriptor limit, so it runs in a process of its own.
    if crate::guest::managed_agent::isolate_process_test("NVX_EXEC_GROUP_JOINER_TEST_CHILD", TEST) {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let joined = temporary.path().join("joined");
    let executable = std::env::current_exe().unwrap();
    let executable = CString::new(executable.as_os_str().as_bytes()).unwrap();
    let (pipes, ends) = Pipes::new(false).unwrap();
    let spawned = Command::new(
        executable.clone(),
        vec![
            executable,
            c"--exact".to_owned(),
            CString::new(TEST).unwrap(),
            c"--nocapture".to_owned(),
        ],
    )
    .environment(vec![
        CString::new(format!(
            "{PROGRAM}={}:{}",
            nix::unistd::getpgrp(),
            joined.display()
        ))
        .unwrap(),
    ])
    .stdout(ends.stdout)
    .stderr(ends.stderr)
    .new_process_group()
    .spawn()
    .unwrap();
    let id = spawned.id();
    let started = Instant::now();
    while !joined.exists() {
        assert!(
            started.elapsed() < PATIENCE,
            "the program joined the test's group in time"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // No process is left in the group that the program led, so a signal to the group alone
    // would reach nothing.
    let program = nix::unistd::Pid::from_raw(i32::try_from(id).unwrap());
    assert_eq!(nix::sys::signal::killpg(program, None), Err(Errno::ESRCH));
    // Without a spare descriptor, the pidfd does not open, and no cgroup holds the program.
    let lowest = std::fs::File::open("/dev/null").unwrap().as_raw_fd();
    let previous = sys::set_descriptor_limit(lowest as _).unwrap();
    let started = Execution::new(spawned, pipes, Options::new().process_group());
    sys::set_descriptor_limit(previous).unwrap();
    assert_eq!(sys::errno_of(&started.unwrap_err()), Errno::EMFILE);
    assert_eq!(signal_of(Child::new(id).unwrap().wait().unwrap()), Some(9));
}

#[test]
fn the_program_s_ends_block_and_the_execution_s_do_not() {
    let nonblocking = |descriptor: BorrowedFd<'_>| {
        OFlag::from_bits_retain(fcntl(descriptor, FcntlArg::F_GETFL).unwrap())
            .contains(OFlag::O_NONBLOCK)
    };
    let (pipes, ends) = Pipes::new(true).unwrap();
    for end in [
        ends.stdin.as_ref().unwrap().as_fd(),
        ends.stdout.as_fd(),
        ends.stderr.as_fd(),
    ] {
        assert!(!nonblocking(end));
    }
    assert!(nonblocking(pipes.input.as_ref().unwrap().borrowed()));
    for output in &pipes.outputs {
        assert!(nonblocking(output.borrowed()));
    }
    let (pipes, ends) = Pipes::new(false).unwrap();
    assert!(pipes.input.is_none() && ends.stdin.is_none());
}

/// Starts `/bin/sh -c script` by a launcher other than [`spawn`](crate::guest::spawn), the
/// standard library's, as a caller with a launcher of its own does: in a process group of its
/// own, with new pipes, its standard input piped if `input` and the null device otherwise. It
/// returns the program's ID and the test's ends of its pipes.
fn start_own(script: &str, input: bool) -> (u32, Pipes) {
    let (stdout, stdout_writer) = std::io::pipe().unwrap();
    let (stderr, stderr_writer) = std::io::pipe().unwrap();
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", script])
        .stdout(stdout_writer)
        .stderr(stderr_writer)
        .process_group(0);
    let writer = if input {
        let (reader, writer) = std::io::pipe().unwrap();
        command.stdin(reader);
        Some(OwnedFd::from(writer))
    } else {
        command.stdin(std::process::Stdio::null());
        None
    };
    // The execution collects the program's status, rather than the standard library's handle.
    #[allow(clippy::zombie_processes)]
    let program = command.spawn().unwrap();
    // The command holds the program's ends until it drops.
    drop(command);
    let pipes = Pipes::from_ends(writer, stdout.into(), stderr.into()).unwrap();
    (program.id(), pipes)
}

/// Adopts the program `id` that [`start_own`] started, by a pidfd that the test opened for it, as
/// a caller's launcher holds one, with `options` that end its process group too.
fn adopt(id: u32, pipes: Pipes, options: Options) -> Execution {
    let pidfd = sys::open_pidfd(ProcessId::from_positive(i64::from(id)).unwrap()).unwrap();
    let child = Child::from_pidfd(id, pidfd.into()).unwrap();
    Execution::adopt(child, pipes, options.process_group()).unwrap()
}

#[test]
fn an_adopted_program_s_launch_comes_at_once_then_its_output_and_status() {
    let (id, pipes) = start_own("printf out; printf err >&2; exit 3", false);
    let mut execution = adopt(id, pipes, Options::new());
    assert_eq!(execution.id(), id);
    // No descriptor waits for the launch, which the first step reports with no failure, and the
    // execution collects the status through the program's pidfd.
    assert!(
        execution
            .descriptors()
            .all(|(source, _)| source != Source::Launch)
    );
    assert!(
        execution
            .descriptors()
            .any(|(source, _)| source == Source::Exit)
    );
    let mut buffer = [0u8; MAX_EVENT_BYTES];
    assert!(matches!(
        execution.next_event(&mut buffer).unwrap(),
        Some(Event::Launch(Ok(None)))
    ));
    let mut record = Record {
        launch: Some(None),
        ..Record::default()
    };
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    assert_eq!(record.output, [b"out".to_vec(), b"err".to_vec()]);
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.status().code(), Some(3));
    assert_eq!(outcome.cause(), None);
    assert!(execution.descriptors().next().is_none());
}

#[test]
fn an_adopted_program_reads_its_input_to_its_end() {
    let (id, pipes) = start_own("cat", true);
    let mut execution = adopt(id, pipes, Options::new());
    assert_eq!(execution.write_input(b"in").unwrap(), 2);
    execution.close_input();
    let record = run(&mut execution);
    assert_eq!(record.launch, Some(None));
    assert_eq!(record.output[0], b"in");
    assert!(record.outcome.unwrap().status().success());
}

#[test]
fn an_adopted_program_that_a_nonblocking_pidfd_names_runs_as_any_other() {
    let (id, pipes) = start_own("printf done; exit 4", false);
    let process = ProcessId::from_positive(i64::from(id)).unwrap();
    let pidfd = OwnedFd::from(sys::open_pidfd(process).unwrap());
    // As `PIDFD_NONBLOCK` opens one, through which a blocking wait fails while the program runs.
    let flags = OFlag::from_bits_retain(fcntl(&pidfd, FcntlArg::F_GETFL).unwrap());
    fcntl(&pidfd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).unwrap();
    let child = Child::from_pidfd(id, pidfd).unwrap();
    let mut execution = Execution::adopt(child, pipes, Options::new().process_group()).unwrap();
    let record = run(&mut execution);
    assert_eq!(record.output[0], b"done");
    assert_eq!(record.outcome.unwrap().status().code(), Some(4));
}

#[test]
fn an_adopted_program_that_its_id_names_is_cancelled_with_its_process_group() {
    let temporary = tempfile::tempdir().unwrap();
    let file = temporary.path().join("descendant");
    // Without a cgroup, only a signal to the program's process group reaches the descendant.
    let (id, pipes) = start_own(
        &format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! >'{}'; echo started; exec sleep 30",
            file.display()
        ),
        false,
    );
    // Nothing has collected the program's status, so `adopt` opens its pidfd by its ID.
    let mut execution = Execution::adopt(
        Child::new(id).unwrap(),
        pipes,
        Options::new().process_group(),
    )
    .unwrap();
    assert!(
        execution
            .descriptors()
            .any(|(source, _)| source == Source::Exit)
    );
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    record.drive_until(&mut execution, true, |record, _| {
        record.output[0] == b"started\n"
    });
    let descendant = Descendant::open(&file);
    assert!(execution.cancel());
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    let outcome = record.outcome.unwrap();
    assert_eq!(signal_of(outcome.status()), Some(9));
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    descendant.assert_ends();
}

#[test]
fn an_adopted_program_whose_status_was_observed_has_ended_and_its_output_is_read() {
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path());
    let (id, pipes) = start_own("printf done", true);
    // A collector of the caller's took the status and handed it over before the adoption, so the
    // program's ID may name another process, and `adopt` opens nothing by it.
    let status = Child::new(id).unwrap().wait().unwrap();
    let mut child = Child::new(id).unwrap();
    assert!(child.observe(id, status));
    let mut execution = Execution::adopt(
        child,
        pipes,
        Options::new()
            .process_group()
            .cgroup(&cgroup, Scope::Execution),
    )
    .unwrap();
    // The program has ended: its input is closed, nothing waits for its status, and, as its
    // output is still open, the cgroup's processes are killed.
    assert_eq!(execution.status(), Some(status));
    assert!(execution.input().is_none());
    assert!(
        execution
            .descriptors()
            .all(|(source, _)| source != Source::Exit)
    );
    assert_eq!(
        std::fs::read(cgroup.path().join("cgroup.kill")).unwrap(),
        b"1"
    );
    assert!(!execution.cancel(), "nothing to cancel");
    let record = run(&mut execution);
    assert_eq!(record.launch, Some(None));
    assert_eq!(record.output[0], b"done");
    let outcome = record.outcome.unwrap();
    assert!(outcome.status().success());
    assert_eq!(outcome.cause(), None);
}

#[test]
fn adopting_a_program_whose_status_another_collector_took_ends_nothing_by_id() {
    let temporary = tempfile::tempdir().unwrap();
    let file = temporary.path().join("descendant");
    let (id, pipes) = start_own(
        &format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! >'{}'",
            file.display()
        ),
        false,
    );
    let status = Child::new(id).unwrap().wait().unwrap();
    assert!(status.success());
    let descendant = Descendant::open(&file);
    // Nothing handed the status over, so `adopt` looks for the program by its ID, which names no
    // process now, while the descendant keeps the program's process group.
    let error = Execution::adopt(
        Child::new(id).unwrap(),
        pipes,
        Options::new().process_group(),
    )
    .unwrap_err();
    assert_eq!(sys::errno_of(&error), Errno::ESRCH);
    descendant.assert_runs();
}

#[test]
fn a_failed_adoption_ends_the_program_and_its_cgroup_without_descriptors() {
    // As for a failed start, the lowered descriptor limit runs this test in a process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_EXEC_ADOPT_DESCRIPTOR_LIMIT_TEST_CHILD",
        "guest::exec::tests::a_failed_adoption_ends_the_program_and_its_cgroup_without_descriptors",
    ) {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let cgroup = fake_cgroup(temporary.path());
    let (id, pipes) = start_own("sleep 30", false);
    // Without a spare descriptor, the pidfd does not open by the program's ID. `adopt` then ends
    // the program, closes the descriptors that it took, and kills the cgroup.
    let lowest = std::fs::File::open("/dev/null").unwrap().as_raw_fd();
    let previous = sys::set_descriptor_limit(lowest as _).unwrap();
    let adopted = Execution::adopt(
        Child::new(id).unwrap(),
        pipes,
        Options::new()
            .process_group()
            .cgroup(&cgroup, Scope::Execution),
    );
    sys::set_descriptor_limit(previous).unwrap();
    assert_eq!(sys::errno_of(&adopted.unwrap_err()), Errno::EMFILE);
    assert_eq!(
        std::fs::read(cgroup.path().join("cgroup.kill")).unwrap(),
        b"1"
    );
    assert_eq!(signal_of(Child::new(id).unwrap().wait().unwrap()), Some(9));
}

#[test]
fn adopted_ends_are_pipe_ends_that_go_the_way_of_their_streams() {
    // For a moment the caller's ends are inheritable, and a child that another test's thread
    // started meanwhile could keep them, so this test runs in a process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_EXEC_ADOPTED_ENDS_TEST_CHILD",
        "guest::exec::tests::adopted_ends_are_pipe_ends_that_go_the_way_of_their_streams",
    ) {
        return;
    }
    let nonblocking = |descriptor: BorrowedFd<'_>| {
        OFlag::from_bits_retain(fcntl(descriptor, FcntlArg::F_GETFL).unwrap())
            .contains(OFlag::O_NONBLOCK)
    };
    let cloexec = |descriptor: BorrowedFd<'_>| {
        FdFlag::from_bits_retain(fcntl(descriptor, FcntlArg::F_GETFD).unwrap())
            .contains(FdFlag::FD_CLOEXEC)
    };
    // The caller's ends, which its launcher left inheritable.
    let pipe = || nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap();
    let inheritable = |end: OwnedFd| {
        fcntl(&end, FcntlArg::F_SETFD(FdFlag::empty())).unwrap();
        end
    };
    let ((stdin, input), (stdout, stdout_writer), (stderr, stderr_writer)) =
        (pipe(), pipe(), pipe());
    let pipes = Pipes::from_ends(
        Some(inheritable(input)),
        inheritable(stdout),
        inheritable(stderr),
    )
    .unwrap();
    for end in [
        pipes.input.as_ref().unwrap(),
        &pipes.outputs[0],
        &pipes.outputs[1],
    ] {
        assert!(nonblocking(end.borrowed()) && cloexec(end.borrowed()));
    }
    // The program's ends of the same pipes keep blocking.
    for end in [stdin.as_fd(), stdout_writer.as_fd(), stderr_writer.as_fd()] {
        assert!(!nonblocking(end));
    }
    // An end of another kind of file, or one that goes the other way, is refused.
    let null = |writes: bool| {
        let file = std::fs::OpenOptions::new()
            .read(!writes)
            .write(writes)
            .open("/dev/null")
            .unwrap();
        OwnedFd::from(file)
    };
    for (input, stdout, stderr) in [
        (Some(pipe().0), pipe().0, pipe().0),
        (None, pipe().1, pipe().0),
        (None, pipe().0, null(false)),
        (Some(null(true)), pipe().0, pipe().0),
    ] {
        let error = Pipes::from_ends(input, stdout, stderr).unwrap_err();
        assert_eq!(sys::errno_of(&error), Errno::EINVAL);
    }
}

/// The signal with which these tests stop a program gracefully.
const SIGTERM: i32 = nix::libc::SIGTERM;

/// Spawns `command` with new pipes and no input, as the leader of a process group of its own if
/// `leads`, and starts its execution with `options` as they are.
fn start_leading(command: Command, leads: bool, options: Options) -> Execution {
    let (pipes, ends) = Pipes::new(false).unwrap();
    let mut command = command.stdout(ends.stdout).stderr(ends.stderr);
    if leads {
        command = command.new_process_group();
    }
    Execution::new(command.spawn().unwrap(), pipes, options).unwrap()
}

/// Waits until the program `id` has ended, and collects its status, as the caller's own
/// collector does.
fn collect_once_ended(id: u32) -> ExitStatus {
    let process = ProcessId::from_positive(i64::from(id)).unwrap();
    let started = Instant::now();
    while !sys::has_exited(process).unwrap() {
        assert!(started.elapsed() < PATIENCE, "the program ended in time");
        std::thread::sleep(Duration::from_millis(1));
    }
    Child::new(id).unwrap().wait().unwrap()
}

#[test]
fn a_program_that_handles_a_signal_ends_with_its_own_status_and_all_of_its_output() {
    // The program handles `SIGTERM`, starts a descendant in its process group, which the signal
    // ends too, says that it is ready, and waits for the descendant, without dash's report of the
    // signal that ended it. It exits 3 unless its handler runs.
    let script = "trap 'printf bye; exit 0' TERM; sleep 30 </dev/null >/dev/null 2>&1 & \
                  printf ready; wait $! 2>/dev/null; exit 3";
    for (adopted, caller_reaps) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut options = Options::new().timeout(Duration::from_secs(3600));
        if caller_reaps {
            options = options.caller_reaps();
        }
        let mut execution = if adopted {
            let (id, pipes) = start_own(script, true);
            adopt(id, pipes, options)
        } else {
            start(shell(script), true, options)
        };
        let id = execution.id();
        execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
        execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
        let mut record = Record::default();
        record.drive_until(&mut execution, true, |record, _| {
            record.output[0] == b"ready"
        });
        let deadline = execution.deadline();
        assert!(deadline.is_some());
        assert!(
            execution.signal(SIGTERM).unwrap(),
            "{adopted} {caller_reaps}"
        );
        // The signal ended neither the execution nor its timeout, and left the input open.
        assert_eq!(execution.deadline(), deadline);
        assert!(execution.input().is_some());
        if caller_reaps {
            let status = collect_once_ended(id);
            assert!(execution.observe(id, status));
        }
        record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
        assert_eq!(
            record.output,
            [b"readybye".to_vec(), Vec::new()],
            "{adopted} {caller_reaps}"
        );
        let outcome = record.outcome.unwrap();
        assert_eq!(outcome.status().code(), Some(0), "{adopted} {caller_reaps}");
        assert_eq!(outcome.cause(), None);
        assert!(record.kill_failures.is_empty());
        // The program has ended, so the execution signals it no more.
        assert!(!execution.signal(SIGTERM).unwrap());
    }
}

#[test]
fn a_program_that_ignores_a_signal_runs_until_the_caller_terminates_it() {
    let mut execution = start(
        shell("trap '' TERM; printf ready; exec sleep 30"),
        true,
        Options::new().timeout(Duration::from_secs(3600)),
    );
    let process = ProcessId::from_positive(i64::from(execution.id())).unwrap();
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    record.drive_until(&mut execution, true, |record, _| {
        record.output[0] == b"ready"
    });
    let deadline = execution.deadline();
    for _ in 0..2 {
        assert!(execution.signal(SIGTERM).unwrap());
    }
    std::thread::sleep(Duration::from_millis(200));
    record.pump(&mut execution, true);
    assert!(!sys::has_exited(process).unwrap());
    assert!(execution.status().is_none());
    assert_eq!(execution.deadline(), deadline);
    assert!(execution.input().is_some());
    // The caller's own deadline passed, so it ends the program, and the outcome says why.
    assert!(execution.terminate());
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    assert_eq!(record.output[0], b"ready");
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.cause(), Some(Cause::Terminated));
    assert_eq!(signal_of(outcome.status()), Some(9));
    assert!(record.kill_failures.is_empty());
}

#[test]
fn a_signal_reaches_the_process_group_that_the_program_leads_if_the_options_say_so() {
    let temporary = tempfile::tempdir().unwrap();
    let file = temporary.path().join("descendant");
    // The descendant starts before the program ignores `SIGTERM`, so the signal ends it alone.
    let script = format!(
        "sleep 30 </dev/null >/dev/null 2>&1 & echo $! >'{}'; trap '' TERM; echo started; \
         exec sleep 30",
        file.display()
    );
    // Whether the options name the group, and whether the execution signals it through the
    // program's pidfd or, as before Linux 6.9, by its ID.
    for (process_group, before_linux_6_9) in [(false, false), (true, false), (true, true)] {
        super::BEFORE_LINUX_6_9.set(before_linux_6_9);
        // The program leads a process group of its own either way.
        let options = if process_group {
            Options::new().process_group()
        } else {
            Options::new()
        };
        let mut execution = start_leading(shell(&script), true, options);
        execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
        execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
        let mut record = Record::default();
        record.drive_until(&mut execution, true, |record, _| {
            record.output[0] == b"started\n"
        });
        let descendant = Descendant::open(&file);
        assert!(execution.signal(SIGTERM).unwrap());
        if process_group {
            descendant.assert_ends();
        } else {
            descendant.assert_runs();
        }
        // The program ignores the signal, so it runs until the caller ends it.
        assert!(execution.cancel());
        record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
        super::BEFORE_LINUX_6_9.set(false);
        let outcome = record.outcome.unwrap();
        assert_eq!(
            outcome.cause(),
            Some(Cause::Cancelled),
            "{process_group} {before_linux_6_9}"
        );
        assert_eq!(signal_of(outcome.status()), Some(9));
        assert!(record.kill_failures.is_empty());
    }
}

#[test]
fn a_signal_reaches_each_process_once() {
    // The program handles `SIGTERM` once and exits: a second signal would run its handler again.
    // Its handler also ends the descendant, which the group's signal may not reach, and it waits
    // for the descendant without dash's report of the signal that ended it.
    let script = "trap 'kill $! 2>/dev/null; printf bye; exit 0' TERM; \
                  sleep 30 </dev/null >/dev/null 2>&1 & printf ready; \
                  wait $! 2>/dev/null; exit 3";
    // Whether the program leads its process group, whether the options name the group, what the
    // first lookup of the program's group finds, if a test hook gives it, and what the execution
    // sends through the program's pidfd. A program that leads its group receives the group's
    // signal alone, with no signal 0 beforehand, which would need permission to signal it. One
    // that the lookup found leading its group, but that left it before the group's signal, as the
    // hook has it, receives a signal of its own after the group's.
    let cases = [
        (true, true, None, vec![(SIGTERM, true)]),
        (false, true, None, vec![(SIGTERM, false), (SIGTERM, true)]),
        (
            false,
            true,
            Some(true),
            vec![(SIGTERM, true), (SIGTERM, false)],
        ),
        (true, false, None, vec![(SIGTERM, false)]),
    ];
    for (leads, process_group, looked_up, sent) in cases {
        let case = format!("{leads} {process_group} {looked_up:?}");
        let options = if process_group {
            Options::new().process_group()
        } else {
            Options::new()
        };
        let mut execution = start_leading(shell(script), leads, options);
        execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
        execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
        let mut record = Record::default();
        record.drive_until(&mut execution, true, |record, _| {
            record.output[0] == b"ready"
        });
        super::SENT.take();
        super::LEADS.set(looked_up);
        assert!(execution.signal(SIGTERM).unwrap(), "{case}");
        assert_eq!(super::SENT.take(), sent, "{case}");
        record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
        assert_eq!(record.output, [b"readybye".to_vec(), Vec::new()], "{case}");
        let outcome = record.outcome.unwrap();
        assert!(outcome.status().success(), "{case}");
        assert_eq!(outcome.cause(), None);
    }
}

#[test]
fn a_signal_reaches_nothing_once_a_collector_took_the_program_s_status() {
    // Whether the program was adopted, whether the caller reaps it, whether a signal to its group
    // would go through its pidfd or, as before Linux 6.9, by the group's ID, and whether the
    // lookup of the program's group finds a leader, as a test hook has it, as when a group leader
    // took the program's ID since.
    for case in 0..16 {
        signal_once_a_collector_took_the_status(
            case & 1 != 0,
            case & 2 != 0,
            case & 4 != 0,
            case & 8 != 0,
        );
    }
}

/// Checks that a signal reaches nothing once a collector of the caller's took the status of a
/// program, which may have been `adopted`, and whose status the caller may reap, that leaves a
/// descendant in its process group, when a signal to the group would go as before Linux 6.9 or
/// not, and when the lookup of the program's group finds a `leader` or not.
fn signal_once_a_collector_took_the_status(
    adopted: bool,
    caller_reaps: bool,
    before_linux_6_9: bool,
    leader: bool,
) {
    let case = format!("{adopted} {caller_reaps} {before_linux_6_9} {leader}");
    let temporary = tempfile::tempdir().unwrap();
    let file = temporary.path().join("descendant");
    let script = format!(
        "sleep 30 </dev/null >/dev/null 2>&1 & echo $! >'{}'",
        file.display()
    );
    super::BEFORE_LINUX_6_9.set(before_linux_6_9);
    let mut options = Options::new();
    if caller_reaps {
        options = options.caller_reaps();
    }
    let mut execution = if adopted {
        let (id, pipes) = start_own(&script, true);
        adopt(id, pipes, options)
    } else {
        start(shell(&script), true, options)
    };
    let id = execution.id();
    // A collector of the caller's takes the status before the execution looks for it, or before
    // the caller hands it over. The program's ID may name another process from then on, and so
    // may that of its process group once the descendant leaves it.
    let status = collect_once_ended(id);
    let descendant = Descendant::open(&file);
    super::SENT.take();
    for _ in 0..2 {
        super::LEADS.set(leader.then_some(true));
        assert!(!execution.signal(SIGTERM).unwrap(), "{case}");
    }
    // Nothing reached the program's process group, which the descendant still holds, and the
    // signals changed nothing in the execution. A program that the lookup found leading its group
    // got nothing at all, as the wait through its pidfd found its status taken.
    let sent = super::SENT.take();
    assert!(sent.iter().all(|&(_, group)| !group), "{case}");
    assert!(!leader || sent.is_empty(), "{case}");
    descendant.assert_runs();
    assert!(execution.input().is_some());
    // The execution learns of the program's end as ever: one that collects the status itself
    // fails to, once, and closes the program's input.
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    let mut buffer = [0u8; 64];
    let mut failures = 0;
    let started = Instant::now();
    while record.closed.len() < 2 {
        match execution.next_event(&mut buffer) {
            Ok(Some(Event::Launch(report))) => record.launch = Some(report.unwrap()),
            Ok(Some(Event::Closed(stream))) => record.closed.push(stream),
            Ok(Some(event)) => panic!("{event:?}"),
            Ok(None) => {
                assert!(started.elapsed() < PATIENCE, "the output closed in time");
                wait_any(&[&execution]);
            }
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(Errno::ECHILD as i32));
                failures += 1;
            }
        }
    }
    assert_eq!(failures, usize::from(!caller_reaps), "{case}");
    assert_eq!(execution.input().is_none(), !caller_reaps, "{case}");
    assert!(execution.observe(id, status));
    assert!(execution.input().is_none());
    assert!(!execution.signal(SIGTERM).unwrap());
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    super::BEFORE_LINUX_6_9.set(false);
    let outcome = record.outcome.unwrap();
    assert!(outcome.status().success(), "{case}");
    assert_eq!(outcome.cause(), None);
    assert!(record.kill_failures.is_empty());
}

#[test]
fn a_failed_group_lookup_is_an_error_that_sends_no_second_signal() {
    // A seccomp filter lasts as long as the thread that installs it, and passes to the processes
    // that the thread starts, so the test runs in a process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_EXEC_DENIED_GETPGID_TEST_CHILD",
        "guest::exec::tests::a_failed_group_lookup_is_an_error_that_sends_no_second_signal",
    ) {
        return;
    }
    // The program leads its group, and ignores `SIGTERM`, so it runs until the test ends it.
    let mut execution = start(
        shell("trap '' TERM; printf ready; exec sleep 30"),
        false,
        Options::new(),
    );
    execution.grant(Stream::Stdout, MAX_EVENT_BYTES);
    execution.grant(Stream::Stderr, MAX_EVENT_BYTES);
    let mut record = Record::default();
    record.drive_until(&mut execution, true, |record, _| {
        record.output[0] == b"ready"
    });
    sys::deny_system_call(nix::libc::SYS_getpgid);
    // A first lookup that fails sends nothing.
    super::SENT.take();
    let error = execution.signal(SIGTERM).unwrap_err();
    assert_eq!(sys::errno_of(&error), Errno::EPERM);
    assert!(super::SENT.take().is_empty());
    // One that fails after the group's signal, where the hook answers the first, sends the program
    // no signal of its own.
    super::LEADS.set(Some(true));
    let error = execution.signal(SIGTERM).unwrap_err();
    assert_eq!(sys::errno_of(&error), Errno::EPERM);
    assert_eq!(super::SENT.take(), [(SIGTERM, true)]);
    // The kill needs no lookup.
    assert!(execution.cancel());
    record.drive_until(&mut execution, true, |record, _| record.outcome.is_some());
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    assert_eq!(signal_of(outcome.status()), Some(9));
    assert!(record.kill_failures.is_empty());
}

#[test]
fn a_kill_and_a_signal_that_the_kernel_refuses_are_errors_that_end_nothing() {
    let mut execution = start(shell("exec sleep 30"), false, Options::new());
    let process = ProcessId::from_positive(i64::from(execution.id())).unwrap();
    // A kill stays with the execution's causes, and the kernel refuses a number that names no
    // signal.
    for signal in [nix::libc::SIGKILL, 65] {
        let error = execution.signal(signal).unwrap_err();
        assert_eq!(sys::errno_of(&error), Errno::EINVAL, "{signal}");
    }
    // The kernel refuses the execution's signals, as it does those that the caller may not send.
    super::REFUSED.set(Some(Errno::EPERM));
    let refused = execution.signal(SIGTERM);
    super::REFUSED.set(None);
    assert_eq!(sys::errno_of(&refused.unwrap_err()), Errno::EPERM);
    // The program runs on, the execution has no cause yet, and no event reports the refusals.
    std::thread::sleep(Duration::from_millis(100));
    assert!(!sys::has_exited(process).unwrap());
    assert!(execution.cancel());
    let record = run(&mut execution);
    assert!(record.kill_failures.is_empty());
    let outcome = record.outcome.unwrap();
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    assert_eq!(signal_of(outcome.status()), Some(9));
}
