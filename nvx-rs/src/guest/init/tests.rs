//! Checks of PID 1: the helpers that it takes over, its priority, what it does with signals and
//! with the children that end, its console, and how it ends the VM.
//!
//! Most checks run PID 1 over a fake system, which records its calls. The checks of the real
//! signals, children, and statuses run in a child of an isolated test process that makes itself a
//! child subreaper, which the kernel makes the parent of its orphaned descendants, as it makes
//! PID 1 theirs, and the check of the real priority needs the privileges of a guest's PID 1. No
//! check stops a VM: each records the status with which PID 1 would.

mod exec;
pub(super) use exec::prepare_child;

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

use super::*;
use crate::guest::managed_agent::tests::isolate_process_test;
use crate::guest::sys::tests::{
    CAP_SYS_NICE, CAP_SYS_RESOURCE, create_fifo, fork_isolated_test, holds_capability, nice_of,
};
use crate::guest::sys::{ForkResult, wait};

/// Drives the unchanged helper/signal scripts through production preparation, collection and
/// shutdown; layered boot itself is exercised by the boot tests.
fn init(system: &mut impl Waiting, arguments: &[CString]) {
    let result = serve(system, arguments);
    finish(system, result);
}

fn serve(system: &mut impl Waiting, arguments: &[CString]) -> Result<(), String> {
    let helpers = prepare(system, arguments)?;
    let mut terminating = false;
    loop {
        collect(system, helpers, |_, _| {})?;
        if terminating {
            return Ok(());
        }
        terminating = system.wait_signal() == Some(Signal::SIGTERM);
    }
}

trait Waiting: System {
    fn wait_signal(&mut self) -> Option<Signal>;
}

impl Waiting for RealSystem {
    fn wait_signal(&mut self) -> Option<Signal> {
        let signals = self.signals.as_ref()?;
        loop {
            if let Some(event) = signals.read() {
                return event.signal;
            }
            match sys::poll([Pollable::Signals(signals)], None) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(_) => std::thread::sleep(Duration::from_secs(1)),
            }
        }
    }
}

/// One operation that PID 1 asked its system for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    /// [`System::listen`].
    Listen,
    /// [`System::raise_priority`] of the process with this ID, or of PID 1.
    Priority(Option<i32>),
    /// [`System::adopt`] of the process with this ID.
    Adopt(i32),
    /// [`System::reap`].
    Reap,
    /// [`Waiting::wait_signal`].
    Wait,
    /// [`System::report`] of this line.
    Report(String),
    /// [`System::shut_down`] with this status.
    ShutDown(u8),
}

/// A system that records PID 1's calls, and answers them from the test's script.
#[derive(Default)]
struct Fake {
    /// The calls, in order.
    calls: Vec<Call>,
    /// The failure of [`System::listen`], if any.
    listen: Option<Errno>,
    /// The failures of [`System::raise_priority`], by the process ID that it names.
    priority: Vec<(Option<i32>, Errno)>,
    /// The answers of [`System::adopt`], by process ID; the other processes run.
    adoptions: Vec<(i32, Result<Option<ChildStatus>, Errno>)>,
    /// The answers of [`System::reap`], in order: a child that ended, or `None` when none did.
    /// Once they run out, no child ended.
    reaped: VecDeque<Option<(i32, ChildStatus)>>,
    /// The signals that [`Waiting::wait_signal`] returns, in order. The check fails if PID 1 waits
    /// for another.
    signals: VecDeque<Signal>,
}

impl System for Fake {
    fn listen(&mut self) -> Result<(), Errno> {
        self.calls.push(Call::Listen);
        self.listen.map_or(Ok(()), Err)
    }

    fn raise_priority(&mut self, process: Option<ProcessId>) -> Result<(), Errno> {
        let process = process.map(ProcessId::raw);
        self.calls.push(Call::Priority(process));
        match self.priority.iter().find(|(failed, _)| *failed == process) {
            Some(&(_, error)) => Err(error),
            None => Ok(()),
        }
    }

    fn adopt(&mut self, process: ProcessId) -> Result<Option<ChildStatus>, Errno> {
        self.calls.push(Call::Adopt(process.raw()));
        self.adoptions
            .iter()
            .find(|(adopted, _)| *adopted == process.raw())
            .map_or(Ok(None), |(_, answer)| *answer)
    }

    fn reap(&mut self) -> Option<(ProcessId, ChildStatus, i32)> {
        self.calls.push(Call::Reap);
        let (process, status) = self.reaped.pop_front().flatten()?;
        let raw = match status {
            ChildStatus::Exited(code) => code << 8,
            ChildStatus::Signaled(signal) => signal,
            ChildStatus::Other => 0x7f,
        };
        Some((pid(process), status, raw))
    }

    fn report(&mut self, line: &[u8]) {
        self.calls
            .push(Call::Report(String::from_utf8(line.to_vec()).unwrap()));
    }

    fn shut_down(&mut self, status: u8) {
        self.calls.push(Call::ShutDown(status));
    }
}

impl Waiting for Fake {
    fn wait_signal(&mut self) -> Option<Signal> {
        self.calls.push(Call::Wait);
        Some(
            self.signals
                .pop_front()
                .expect("PID 1 waits for no signal that the check sends"),
        )
    }
}

#[test]
fn the_shared_collector_forwards_workload_status_and_keeps_helper_failure_fatal() {
    let helpers = Helpers {
        time: pid(42),
        announcer: Some(pid(43)),
    };
    let mut system = Fake {
        reaped: [
            Some((51, ChildStatus::Exited(7))),
            Some((52, ChildStatus::Signaled(35))),
            Some((43, ChildStatus::Exited(0))),
        ]
        .into(),
        ..Fake::default()
    };
    let mut observed = Vec::new();
    let error = collect(&mut system, helpers, |id, status| {
        observed.push((id, status))
    })
    .unwrap_err();
    assert_eq!(observed[0].0, 51);
    assert_eq!(observed[0].1.code(), Some(7));
    assert_eq!(observed[1].0, 52);
    assert_eq!(observed[1].1.signal(), Some(35));
    assert_eq!(observed.len(), 2);
    assert_eq!(error, "the announcer, process 43, exited with status 0");
}

#[test]
fn the_runtime_wakes_for_children_without_control_input_and_prioritizes_a_helper_over_sigterm() {
    let name = "guest::init::tests::the_runtime_wakes_for_children_without_control_input_and_prioritizes_a_helper_over_sigterm";
    if isolate_process_test("NVX_INIT_RUNTIME_WAKE_TEST", name) {
        return;
    }
    in_child(|| {
        sys::reset_child_signal();
        let signals = SignalDescriptor::new(&SIGNALS).unwrap();
        let mut helper = Command::new("sleep").arg("30").spawn().unwrap();
        let helper_id = ProcessId::from_child(&helper);
        let descriptor = sys::open_pidfd(helper_id).unwrap();
        let mut runtime = Runtime {
            system: RealSystem {
                signals: Some(signals),
            },
            helpers: Helpers {
                time: helper_id,
                announcer: None,
            },
            terminating: false,
        };
        let mut execution = Exec::start_simulated(exec::request("exit 7"), None).unwrap();
        let [wake] = sys::poll([runtime.wake()], Some(Duration::from_secs(5))).unwrap();
        assert!(
            wake.readable && !wake.error,
            "the child's end did not wake init"
        );
        assert!(
            !runtime
                .service(|id, status| {
                    execution.observe(id, status);
                })
                .unwrap()
        );
        assert_eq!(execution.status().unwrap().code(), Some(7));
        helper.kill().unwrap();
        assert!(sys::readable_or_hung_up(&[descriptor.borrowed()], 1000).unwrap()[0]);
        nix::sys::signal::kill(nix::unistd::getpid(), Signal::SIGTERM).unwrap();
        let error = runtime.service(|_, _| {}).unwrap_err();
        assert!(error.contains("the time daemon"));
        assert!(error.contains("signal 9"));
        assert_eq!(
            helper.wait().unwrap_err().raw_os_error(),
            Some(Errno::ECHILD as i32)
        );
    });
}

/// A system whose signals, children, and statuses are the real ones, which changes no priority,
/// as that needs privileges, and which records the diagnostics, the signals that PID 1 read, and
/// the end of the VM instead.
struct Subreaper {
    /// The real operations.
    real: RealSystem,
    /// What happens before PID 1 first waits for a signal, if anything.
    before_wait: Option<Box<dyn FnOnce()>>,
    /// The signals that PID 1 read, in order.
    signals: Vec<Signal>,
    /// The IDs of the children whose status PID 1 collected, in order.
    reaped: Vec<i32>,
    /// The diagnostics, concatenated.
    reports: String,
    /// The status with which PID 1 ended the VM.
    status: Option<u8>,
}

impl Subreaper {
    /// Makes the calling process a child subreaper, and returns a system that collected no
    /// status yet.
    fn new() -> Self {
        nix::sys::prctl::set_child_subreaper(true).unwrap();
        Self {
            real: RealSystem { signals: None },
            before_wait: None,
            signals: Vec::new(),
            reaped: Vec::new(),
            reports: String::new(),
            status: None,
        }
    }
}

impl System for Subreaper {
    fn listen(&mut self) -> Result<(), Errno> {
        self.real.listen()
    }

    fn raise_priority(&mut self, _: Option<ProcessId>) -> Result<(), Errno> {
        Ok(())
    }

    fn adopt(&mut self, process: ProcessId) -> Result<Option<ChildStatus>, Errno> {
        self.real.adopt(process)
    }

    fn reap(&mut self) -> Option<(ProcessId, ChildStatus, i32)> {
        let child = self.real.reap()?;
        self.reaped.push(child.0.raw());
        Some(child)
    }

    fn report(&mut self, line: &[u8]) {
        self.reports.push_str(std::str::from_utf8(line).unwrap());
    }

    fn shut_down(&mut self, status: u8) {
        self.status = Some(status);
    }
}

impl Waiting for Subreaper {
    fn wait_signal(&mut self) -> Option<Signal> {
        if let Some(before_wait) = self.before_wait.take() {
            before_wait();
        }
        let signal = self.real.wait_signal();
        self.signals.extend(signal);
        signal
    }
}

/// A system whose priorities, children, and statuses are the real ones, which records the
/// priorities of PID 1 and its helpers when PID 1 first waits for a signal, and then asks it to
/// end the VM, which it records instead.
struct Observer {
    /// The real operations.
    real: RealSystem,
    /// The helpers whose priorities it records after PID 1's.
    helpers: [ProcessId; 2],
    /// The out-of-memory score adjustment and the nice value of PID 1 and of each helper.
    observed: Vec<(i32, i32)>,
    /// The diagnostics, concatenated.
    reports: String,
    /// The status with which PID 1 ended the VM.
    status: Option<u8>,
}

impl System for Observer {
    fn listen(&mut self) -> Result<(), Errno> {
        self.real.listen()
    }

    fn raise_priority(&mut self, process: Option<ProcessId>) -> Result<(), Errno> {
        self.real.raise_priority(process)
    }

    fn adopt(&mut self, process: ProcessId) -> Result<Option<ChildStatus>, Errno> {
        self.real.adopt(process)
    }

    fn reap(&mut self) -> Option<(ProcessId, ChildStatus, i32)> {
        self.real.reap()
    }

    fn report(&mut self, line: &[u8]) {
        self.reports.push_str(std::str::from_utf8(line).unwrap());
    }

    fn shut_down(&mut self, status: u8) {
        self.status = Some(status);
    }
}

impl Waiting for Observer {
    fn wait_signal(&mut self) -> Option<Signal> {
        let processes = std::iter::once(sys::current_process()).chain(self.helpers);
        self.observed = processes
            .map(|process| {
                let path = format!("/proc/{}/oom_score_adj", process.raw());
                let adjustment = std::fs::read_to_string(path).unwrap();
                (adjustment.trim().parse().unwrap(), nice_of(process))
            })
            .collect();
        Some(Signal::SIGTERM)
    }
}

/// Returns the process ID `raw`.
fn pid(raw: i32) -> ProcessId {
    ProcessId::from_positive(raw.into()).unwrap()
}

/// Returns `values` as the arguments of `nvx-init` after its name.
fn arguments(values: &[&str]) -> Vec<CString> {
    values
        .iter()
        .map(|value| CString::new(*value).unwrap())
        .collect()
}

/// Runs PID 1 over `system` with the helpers `values`, and returns the calls that it made.
fn calls(mut system: Fake, values: &[&str]) -> Vec<Call> {
    init(&mut system, &arguments(values));
    system.calls
}

/// Returns the calls with which PID 1 takes over, when all goes well, the helpers `processes`:
/// the time daemon, and then the announcer.
fn takeover(processes: &[i32]) -> Vec<Call> {
    let mut calls = vec![Call::Listen, Call::Priority(None)];
    for &process in processes {
        calls.extend([Call::Adopt(process), Call::Priority(Some(process))]);
    }
    calls
}

/// Returns the call that reports the failure that `text` describes.
fn failure(text: &str) -> Call {
    Call::Report(format!("NVX-INIT-ERROR: {text}\n"))
}

/// Runs `check` alone in a child of the isolated test process, and checks that it passed.
///
/// The checks block signals, collect the status of every child that ends, or replace standard
/// error, which would disturb the harness's other tests.
fn in_child(check: impl FnOnce() + std::panic::UnwindSafe) {
    match fork_isolated_test().unwrap() {
        ForkResult::Child => {
            let outcome = std::panic::catch_unwind(check);
            sys::exit_immediately(u8::from(outcome.is_err()));
        }
        ForkResult::Parent(child) => {
            assert_eq!(wait(child), Some(ChildStatus::Exited(0)));
        }
    }
}

#[test]
fn the_helpers_are_the_time_daemon_and_an_optional_announcer() {
    assert_eq!(
        Helpers::parse(&arguments(&["42"])),
        Some(Helpers {
            time: pid(42),
            announcer: None,
        })
    );
    assert_eq!(
        Helpers::parse(&arguments(&["42", "7"])),
        Some(Helpers {
            time: pid(42),
            announcer: Some(pid(7)),
        })
    );
    for invalid in [
        &[][..],
        &["42", "7", "8"],
        &["0"],
        &["1"],
        &["42", "1"],
        &["42", "42"],
        &["-5"],
        &["+5"],
        &[" 42"],
        &["4x"],
        &[""],
        &["2147483648"],
    ] {
        assert_eq!(Helpers::parse(&arguments(invalid)), None, "{invalid:?}");
    }
}

#[test]
fn pid_1_raises_its_own_priority_and_then_each_helpers() {
    let system = Fake {
        signals: [Signal::SIGTERM].into(),
        ..Fake::default()
    };
    let mut expected = takeover(&[42, 7]);
    expected.extend([Call::Reap, Call::Wait, Call::Reap, Call::ShutDown(0)]);
    assert_eq!(calls(system, &["42", "7"]), expected);
}

#[test]
fn without_an_announcer_only_the_time_daemon_is_a_helper() {
    // Process 7 is an orphan here, whose end leaves the VM running.
    let system = Fake {
        reaped: [Some((7, ChildStatus::Exited(1)))].into(),
        signals: [Signal::SIGTERM].into(),
        ..Fake::default()
    };
    let mut expected = takeover(&[42]);
    expected.extend([
        Call::Reap,
        Call::Reap,
        Call::Wait,
        Call::Reap,
        Call::ShutDown(0),
    ]);
    assert_eq!(calls(system, &["42"]), expected);
}

#[test]
fn sigchld_has_pid_1_collect_every_child_that_ended_and_sigterm_ends_the_vm() {
    let system = Fake {
        reaped: [
            Some((50, ChildStatus::Exited(3))),
            Some((51, ChildStatus::Signaled(9))),
            None,
            Some((52, ChildStatus::Exited(0))),
        ]
        .into(),
        signals: [Signal::SIGCHLD, Signal::SIGCHLD, Signal::SIGTERM].into(),
        ..Fake::default()
    };
    let mut expected = takeover(&[42, 7]);
    expected.extend([
        // The children that ended before PID 1 waited, then those that each signal tells of, and
        // after `SIGTERM` those that ended before it.
        Call::Reap,
        Call::Reap,
        Call::Reap,
        Call::Wait,
        Call::Reap,
        Call::Reap,
        Call::Wait,
        Call::Reap,
        Call::Wait,
        Call::Reap,
        Call::ShutDown(0),
    ]);
    assert_eq!(calls(system, &["42", "7"]), expected);
}

#[test]
fn sigterm_ends_the_vm_with_0_only_if_no_helper_ended_before_it() {
    // A child ends just before `SIGTERM` comes, and the descriptor returns `SIGTERM` before that
    // child's `SIGCHLD`. A helper's end ends the VM as a failure...
    let system = Fake {
        reaped: [None, Some((42, ChildStatus::Exited(7)))].into(),
        signals: [Signal::SIGTERM, Signal::SIGCHLD].into(),
        ..Fake::default()
    };
    let mut expected = takeover(&[42]);
    expected.extend([
        Call::Reap,
        Call::Wait,
        Call::Reap,
        failure("the time daemon, process 42, exited with status 7"),
        Call::ShutDown(FAILURE),
    ]);
    assert_eq!(calls(system, &["42"]), expected);
    // ...but an orphan's end leaves it to end with 0.
    let system = Fake {
        reaped: [None, Some((50, ChildStatus::Exited(1)))].into(),
        signals: [Signal::SIGTERM, Signal::SIGCHLD].into(),
        ..Fake::default()
    };
    let mut expected = takeover(&[42]);
    expected.extend([
        Call::Reap,
        Call::Wait,
        Call::Reap,
        Call::Reap,
        Call::ShutDown(0),
    ]);
    assert_eq!(calls(system, &["42"]), expected);
}

#[test]
fn the_end_of_either_helper_ends_the_vm_whatever_its_status() {
    for (status, how) in [
        (ChildStatus::Exited(0), "exited with status 0"),
        (ChildStatus::Exited(7), "exited with status 7"),
        (ChildStatus::Signaled(9), "was killed by signal 9"),
        // A real-time signal, which `nix`'s `Signal` cannot represent.
        (ChildStatus::Signaled(34), "was killed by signal 34"),
        (ChildStatus::Other, "ended"),
    ] {
        for (process, name) in [(42, "the time daemon"), (7, "the announcer")] {
            // An orphan ends first, which leaves the VM running.
            let system = Fake {
                reaped: [
                    Some((50, ChildStatus::Exited(0))),
                    None,
                    Some((process, status)),
                ]
                .into(),
                signals: [Signal::SIGCHLD].into(),
                ..Fake::default()
            };
            let mut expected = takeover(&[42, 7]);
            expected.extend([
                Call::Reap,
                Call::Reap,
                Call::Wait,
                Call::Reap,
                failure(&format!("{name}, process {process}, {how}")),
                Call::ShutDown(FAILURE),
            ]);
            assert_eq!(calls(system, &["42", "7"]), expected, "{name} {how}");
        }
    }
}

#[test]
fn a_helper_that_ended_before_pid_1_started_ends_the_vm() {
    let system = Fake {
        adoptions: vec![(7, Ok(Some(ChildStatus::Exited(1))))],
        ..Fake::default()
    };
    let mut expected = takeover(&[42]);
    expected.extend([
        Call::Adopt(7),
        failure("the announcer, process 7, exited with status 1"),
        Call::ShutDown(FAILURE),
    ]);
    assert_eq!(calls(system, &["42", "7"]), expected);
}

#[test]
fn a_helper_that_is_not_a_child_of_pid_1_ends_the_vm() {
    let system = Fake {
        adoptions: vec![(42, Err(Errno::ECHILD))],
        ..Fake::default()
    };
    assert_eq!(
        calls(system, &["42", "7"]),
        [
            Call::Listen,
            Call::Priority(None),
            Call::Adopt(42),
            failure(&format!(
                "the time daemon, process 42, is not a child of PID 1: {}",
                Errno::ECHILD.desc()
            )),
            Call::ShutDown(FAILURE),
        ]
    );
}

#[test]
fn pid_1_ends_the_vm_when_it_cannot_take_over() {
    let system = Fake {
        listen: Some(Errno::EMFILE),
        ..Fake::default()
    };
    assert_eq!(
        calls(system, &["42"]),
        [
            Call::Listen,
            failure(&format!("cannot read signals: {}", Errno::EMFILE.desc())),
            Call::ShutDown(FAILURE),
        ]
    );
    let system = Fake {
        priority: vec![(None, Errno::EACCES)],
        ..Fake::default()
    };
    assert_eq!(
        calls(system, &["42"]),
        [
            Call::Listen,
            Call::Priority(None),
            failure(&format!(
                "cannot raise the priority of PID 1: {}",
                Errno::EACCES.desc()
            )),
            Call::ShutDown(FAILURE),
        ]
    );
    let system = Fake {
        priority: vec![(Some(7), Errno::ESRCH)],
        ..Fake::default()
    };
    let mut expected = takeover(&[42]);
    expected.extend([
        Call::Adopt(7),
        Call::Priority(Some(7)),
        failure(&format!(
            "cannot raise the priority of the announcer, process 7: {}",
            Errno::ESRCH.desc()
        )),
        Call::ShutDown(FAILURE),
    ]);
    assert_eq!(calls(system, &["42", "7"]), expected);
    // PID 1 cannot exit, so invalid arguments end the VM too.
    assert_eq!(
        calls(Fake::default(), &["42", "42"]),
        [
            Call::Listen,
            Call::Priority(None),
            failure("usage: nvx-init TIME_DAEMON [ANNOUNCER]"),
            Call::ShutDown(FAILURE),
        ]
    );
}

#[test]
fn diagnostics_go_to_the_console() {
    let directory = tempfile::tempdir().unwrap();
    let console = sys::path_c_string(&directory.path().join("console")).unwrap();
    create_fifo(&console, Mode::from_bits_truncate(0o600)).unwrap();
    let flags = OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_CLOEXEC;
    let reader = sys::open(&console, flags, Mode::empty()).unwrap();
    // Each line opens the console anew.
    write_console(&console, b"NVX-INIT-ERROR: one\n");
    write_console(&console, b"NVX-INIT-ERROR: two\n");
    let mut buffer = [0; 64];
    let count = reader.read(&mut buffer).unwrap();
    assert_eq!(
        &buffer[..count],
        b"NVX-INIT-ERROR: one\nNVX-INIT-ERROR: two\n"
    );
}

#[test]
fn diagnostics_go_to_standard_error_without_a_console() {
    if isolate_process_test(
        "NVX_INIT_STANDARD_ERROR_TEST_CHILD",
        "guest::init::tests::diagnostics_go_to_standard_error_without_a_console",
    ) {
        return;
    }
    in_child(|| {
        let (reader, writer) = sys::create_pipe().unwrap();
        nix::unistd::dup2_stderr(writer.borrowed()).unwrap();
        drop(writer);
        write_console(c"/nonexistent/console", b"NVX-INIT-ERROR: lost\n");
        let mut buffer = [0; 64];
        let count = reader.read(&mut buffer).unwrap();
        assert_eq!(&buffer[..count], b"NVX-INIT-ERROR: lost\n");
    });
}

#[test]
fn pid_1_collects_its_orphans_and_ends_the_vm_once_a_helper_ends() {
    if isolate_process_test(
        "NVX_INIT_ORPHANS_TEST_CHILD",
        "guest::init::tests::pid_1_collects_its_orphans_and_ends_the_vm_once_a_helper_ends",
    ) {
        return;
    }
    in_child(|| {
        let mut system = Subreaper::new();
        // The shell leaves an orphan, whose parent the kernel then makes this subreaper, and
        // which ends while PID 1 waits for signals.
        let output = Command::new("sh")
            .args(["-c", "sleep 0.2 >/dev/null 2>&1 & echo $!"])
            .output()
            .unwrap();
        let orphan: i32 = String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The helper ends with status 7 once PID 1 collected the orphan's status, after which no
        // process has the orphan's ID.
        let script = format!("while kill -0 {orphan} 2>/dev/null; do sleep 0.01; done; exit 7");
        // PID 1 collects the helper's status, rather than the standard library's handle.
        #[allow(clippy::zombie_processes)]
        let helper = Command::new("sh").args(["-c", &script]).spawn().unwrap();
        let helper = ProcessId::from_child(&helper).raw();
        init(&mut system, &arguments(&[&helper.to_string()]));
        assert_eq!(
            system.reports,
            format!("NVX-INIT-ERROR: the time daemon, process {helper}, exited with status 7\n")
        );
        assert_eq!(system.status, Some(FAILURE));
        assert_eq!(system.reaped, [orphan, helper]);
        // No child is left, ended or not.
        let any = nix::unistd::Pid::from_raw(-1);
        assert_eq!(
            nix::sys::wait::waitpid(any, Some(nix::sys::wait::WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        );
    });
}

#[test]
fn sigterm_ends_the_vm_with_status_0() {
    if isolate_process_test(
        "NVX_INIT_SIGTERM_TEST_CHILD",
        "guest::init::tests::sigterm_ends_the_vm_with_status_0",
    ) {
        return;
    }
    in_child(|| {
        let mut system = Subreaper::new();
        // PID 1 listens again when it starts, but the signal must not reach this process before
        // it first listened.
        system.listen().unwrap();
        let mut helper = Command::new("sleep").arg("30").spawn().unwrap();
        let process = ProcessId::from_child(&helper);
        let script = format!("kill -TERM {}", sys::current_process().raw());
        assert!(
            Command::new("sh")
                .args(["-c", &script])
                .status()
                .unwrap()
                .success()
        );
        init(&mut system, &arguments(&[&process.raw().to_string()]));
        assert_eq!(system.status, Some(0));
        assert_eq!(system.reports, "");
        assert_eq!(system.reaped, Vec::<i32>::new());
        helper.kill().unwrap();
        assert_eq!(helper.wait().unwrap().signal(), Some(nix::libc::SIGKILL));
    });
}

#[test]
fn a_helper_that_ended_before_sigterm_ends_the_vm_as_a_failure() {
    if isolate_process_test(
        "NVX_INIT_HELPER_BEFORE_SIGTERM_TEST_CHILD",
        "guest::init::tests::a_helper_that_ended_before_sigterm_ends_the_vm_as_a_failure",
    ) {
        return;
    }
    in_child(|| {
        let mut system = Subreaper::new();
        // PID 1 collects the helper's status, rather than the standard library's handle.
        #[allow(clippy::zombie_processes)]
        let helper = Command::new("sleep").arg("30").spawn().unwrap();
        let helper = nix::unistd::Pid::from_raw(ProcessId::from_child(&helper).raw());
        // Once PID 1 found that no child ended, and before it reads a signal, the helper ends,
        // which leaves its status for PID 1 to collect, and then `SIGTERM` comes.
        system.before_wait = Some(Box::new(move || {
            nix::sys::signal::kill(helper, Signal::SIGKILL).unwrap();
            let flags = nix::sys::wait::WaitPidFlag::WEXITED | nix::sys::wait::WaitPidFlag::WNOWAIT;
            nix::sys::wait::waitid(nix::sys::wait::Id::Pid(helper), flags).unwrap();
            nix::sys::signal::kill(nix::unistd::Pid::this(), Signal::SIGTERM).unwrap();
        }));
        init(&mut system, &arguments(&[&helper.to_string()]));
        // PID 1 read `SIGTERM` first, and then collected the helper's status.
        assert_eq!(system.signals, [Signal::SIGTERM]);
        assert_eq!(
            system.reports,
            format!("NVX-INIT-ERROR: the time daemon, process {helper}, was killed by signal 9\n")
        );
        assert_eq!(system.status, Some(FAILURE));
        assert_eq!(system.reaped, [helper.as_raw()]);
    });
}

#[test]
fn privileged_pid_1_and_its_helpers_run_ahead_of_the_workloads() {
    if isolate_process_test(
        "NVX_INIT_PRIORITY_TEST_CHILD",
        "guest::init::tests::privileged_pid_1_and_its_helpers_run_ahead_of_the_workloads",
    ) {
        return;
    }
    if !holds_capability(CAP_SYS_NICE).unwrap() || !holds_capability(CAP_SYS_RESOURCE).unwrap() {
        return;
    }
    // A process keeps the priority that it raises for itself, so a child of the test's process
    // runs PID 1.
    in_child(|| {
        let mut daemon = Command::new("sleep").arg("30").spawn().unwrap();
        let mut announcer = Command::new("sleep").arg("30").spawn().unwrap();
        let helpers = [&daemon, &announcer].map(ProcessId::from_child);
        let mut system = Observer {
            real: RealSystem { signals: None },
            helpers,
            observed: Vec::new(),
            reports: String::new(),
            status: None,
        };
        let values = helpers.map(|helper| helper.raw().to_string());
        init(&mut system, &arguments(&[&values[0], &values[1]]));
        assert_eq!((system.status, system.reports.as_str()), (Some(0), ""));
        // PID 1, the time daemon, and the announcer.
        assert_eq!(system.observed, [(-1000, -10); 3]);
        daemon.kill().unwrap();
        announcer.kill().unwrap();
        assert_eq!(daemon.wait().unwrap().signal(), Some(nix::libc::SIGKILL));
        assert_eq!(announcer.wait().unwrap().signal(), Some(nix::libc::SIGKILL));
    });
}
