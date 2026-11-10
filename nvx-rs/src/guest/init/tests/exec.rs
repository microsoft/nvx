//! The plain-process core, including privileged checks run explicitly in a Linux guest.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use super::{in_child, isolate_process_test, pid};
use crate::guest::init::*;

thread_local! {
    static CHILD_SETUP: std::cell::Cell<Option<Option<Errno>>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn prepare_child(spec: &mut sys::ChildSpec) {
    if let Some(failure) = CHILD_SETUP.get() {
        spec.simulate_credentials(failure);
    }
}

impl Exec {
    /// Uses the existing syscall seam for host tests; guest proofs call the real start path.
    pub(crate) fn start_simulated(
        request: ExecRequest,
        failure: Option<Errno>,
    ) -> Result<Self, StartFailure> {
        struct Restore(Option<Option<Errno>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CHILD_SETUP.set(self.0);
            }
        }
        let _restore = Restore(CHILD_SETUP.replace(Some(failure)));
        Self::start(request)
    }
}

/// One execution's observed streams, launch, outcome, and collected child IDs.
#[derive(Default)]
struct Collected {
    /// Standard output.
    stdout: Vec<u8>,
    /// Standard error.
    stderr: Vec<u8>,
    /// The launch result, once it arrives.
    launch: Option<Option<LaunchFailure>>,
    /// The execution's final outcome.
    outcome: Option<Outcome>,
    /// The children reaped by this PID-1-style collector.
    reaped: Vec<u32>,
}

/// Describes a shell in numeric-account mode, with no inherited environment.
pub(super) fn request(script: &str) -> ExecRequest {
    ExecRequest {
        program: c"/bin/sh".to_owned(),
        arguments: vec![
            c"sh".to_owned(),
            c"-c".to_owned(),
            CString::new(script).unwrap(),
        ],
        environment: vec![c"PATH=/bin:/sbin:/usr/bin:/usr/sbin".to_owned()],
        directory: c"/".to_owned(),
        uid: 0,
        gid: 0,
        account: AccountMode::Numeric,
        timeout: None,
        input: true,
        search_path: None,
        physical_pwd: false,
    }
}

/// Runs a process-global check in a single-threaded, isolated child subreaper.
fn isolated(name: &str, check: fn()) {
    let test = format!("guest::init::tests::exec::{name}");
    if std::env::var_os("NVX_INIT_EXEC_TEST_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--ignored", "--nocapture"])
            .env("NVX_INIT_EXEC_TEST_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success(), "the isolated privileged check failed");
        return;
    }
    in_child(|| {
        sys::reset_child_signal();
        nix::sys::prctl::set_child_subreaper(true).unwrap();
        check();
    });
}

/// Advances the sole reaper and the pipe driver once, without blocking.
fn step(execution: &mut Exec, collected: &mut Collected) {
    while let Some(child) = sys::reap_any_nonblocking() {
        let id = child.process.raw().unsigned_abs();
        collected.reaped.push(id);
        execution.observe(id, ExitStatus::from_raw(child.wait_status));
    }
    let mut buffer = [0; 4096];
    match execution.next_event(&mut buffer).unwrap() {
        Some(Event::Launch(report)) => collected.launch = Some(report.unwrap()),
        Some(Event::Output(Stream::Stdout, bytes)) => collected.stdout.extend_from_slice(bytes),
        Some(Event::Output(Stream::Stderr, bytes)) => collected.stderr.extend_from_slice(bytes),
        Some(Event::Ended(outcome)) => collected.outcome = Some(outcome),
        Some(Event::KillFailed(target, error)) => panic!("kill {target:?}: {error}"),
        Some(Event::ReadFailed(stream, error)) => panic!("read {stream:?}: {error}"),
        Some(_) | None => {}
    }
}

/// Drives the execution until `ready` holds, or fails rather than hanging a guest check.
fn until(execution: &mut Exec, collected: &mut Collected, ready: impl Fn(&Collected) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready(collected) {
        assert!(
            Instant::now() < deadline,
            "execution did not reach its expected state"
        );
        step(execution, collected);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Drives the execution to its final outcome.
fn finish(execution: &mut Exec, collected: &mut Collected) {
    until(execution, collected, |collected| {
        collected.outcome.is_some()
    });
}

#[test]
fn the_plain_core_owns_its_state_without_legacy_engines() {
    let source = include_str!("../../init.rs");
    for path in [
        "super::entry",
        "super::exec",
        "super::spawn",
        "super::process",
        "super::cgroup",
        "guest::entry",
        "guest::exec",
        "guest::spawn",
        "guest::process",
        "guest::cgroup",
    ] {
        assert!(
            !source.contains(path),
            "legacy execution dependency: {path}"
        );
    }
    let words: Vec<_> = source
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .collect();
    for engine in ["Entry", "Command", "Execution", "Spawned", "Pipes", "Child"] {
        assert!(!words.contains(&engine), "legacy engine retained: {engine}");
    }
}

fn report_pipe() -> (LaunchReport, FileDescriptor) {
    let (reader, writer) = sys::create_pipe().unwrap();
    reader.set_nonblocking().unwrap();
    (
        LaunchReport {
            reader,
            bytes: [0; sys::REPORT_LENGTH],
            length: 0,
        },
        writer,
    )
}

#[test]
fn launch_reports_are_nonblocking_until_complete_and_reject_truncation_or_entry_stages() {
    let name = "guest::init::tests::exec::launch_reports_are_nonblocking_until_complete_and_reject_truncation_or_entry_stages";
    if isolate_process_test("NVX_INIT_EXEC_REPORT_TEST", name) {
        return;
    }
    let (mut report, writer) = report_pipe();
    assert!(report.read().is_none());
    writer.write_all(&18u32.to_ne_bytes()).unwrap();
    assert!(report.read().is_none());
    writer
        .write_all(&(Errno::EACCES as i32).to_ne_bytes())
        .unwrap();
    assert_eq!(
        report.read().unwrap().unwrap(),
        Some(LaunchFailure {
            stage: 18,
            error: Errno::EACCES as i32
        })
    );
    for bytes in [
        vec![1],
        [5u32.to_ne_bytes(), (Errno::EPERM as i32).to_ne_bytes()].concat(),
    ] {
        let (mut report, writer) = report_pipe();
        writer.write_all(&bytes).unwrap();
        drop(writer);
        assert_eq!(
            report.read().unwrap().unwrap_err().raw_os_error(),
            Some(Errno::EPROTO as i32)
        );
    }
    let (mut report, writer) = report_pipe();
    drop(writer);
    assert_eq!(report.read().unwrap().unwrap(), None);
}

/// This deliberately invalid pidfd makes every attempted kill fail without signaling anything.
fn pipe_execution(stdout: &[u8], stderr: &[u8]) -> Exec {
    let output = |bytes| {
        let (reader, writer) = sys::create_pipe().unwrap();
        reader.set_nonblocking().unwrap();
        writer.write_all(bytes).unwrap();
        Some(reader)
    };
    let (report, writer) = report_pipe();
    drop(writer);
    Exec {
        leader: pid(42),
        group: sys::open(
            c"/dev/null",
            OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )
        .unwrap(),
        launch: Some(report),
        input: None,
        outputs: [output(stdout), output(stderr)],
        discarding: [false; 2],
        discarded: [0; 2],
        turn: 0,
        status: None,
        status_lost: true,
        cause: None,
        deadline: None,
        failures: [None, None],
        ended: false,
    }
}

#[test]
fn only_the_collectors_terminal_status_settles_the_owned_leader_and_output() {
    let name = "guest::init::tests::exec::only_the_collectors_terminal_status_settles_the_owned_leader_and_output";
    if isolate_process_test("NVX_INIT_EXEC_STATUS_TEST", name) {
        return;
    }
    let mut execution = pipe_execution(b"out", b"err");
    let sources: Vec<_> = execution.descriptors().map(|(source, _)| source).collect();
    assert_eq!(
        sources,
        [
            Source::Launch,
            Source::Output(Stream::Stdout),
            Source::Output(Stream::Stderr)
        ]
    );
    assert!(!execution.observe(43, ExitStatus::from_raw(7 << 8)));
    assert!(!execution.observe(42, ExitStatus::from_raw((19 << 8) | 0x7f)));
    let mut buffer = [0; 2];
    let mut closed = Vec::new();
    let mut collected = [Vec::new(), Vec::new()];
    let deadline = Instant::now() + Duration::from_secs(5);
    while execution.launch.is_some() || execution.has_open_output() {
        assert!(Instant::now() < deadline, "launch and output did not close");
        match execution.next_event(&mut buffer).unwrap() {
            Some(Event::Output(stream, bytes)) => {
                collected[stream as usize].extend_from_slice(bytes)
            }
            Some(Event::Closed(stream)) => closed.push(stream),
            Some(Event::Launch(report)) => assert_eq!(report.unwrap(), None),
            None => std::thread::sleep(Duration::from_millis(1)),
            event => panic!("unexpected {event:?}"),
        }
    }
    assert_eq!(collected, [b"out".to_vec(), b"err".to_vec()]);
    assert_eq!(closed.len(), 2);
    assert!(!execution.has_open_output());
    assert!(execution.output_hung_up(Stream::Stdout).unwrap());
    assert!(execution.status().is_none());
    assert!(execution.observe(42, ExitStatus::from_raw(7 << 8)));
    assert!(!execution.observe(42, ExitStatus::from_raw(0)));
    assert!(matches!(execution.next_event(&mut buffer).unwrap(),
        Some(Event::KillFailed(KillTarget::ProcessGroup, error)) if error.raw_os_error() == Some(Errno::EBADF as i32)));
    let Some(Event::Ended(outcome)) = execution.next_event(&mut buffer).unwrap() else {
        panic!("no outcome");
    };
    assert_eq!(outcome.status.code(), Some(7));
    assert_eq!(outcome.cause, None);
    assert_eq!(outcome.discarded, [0; 2]);
    assert!(execution.next_event(&mut buffer).unwrap().is_none());
}

#[test]
fn ending_failures_are_reported_and_discarding_preserves_the_first_cause() {
    let name = "guest::init::tests::exec::ending_failures_are_reported_and_discarding_preserves_the_first_cause";
    if isolate_process_test("NVX_INIT_EXEC_ENDING_TEST", name) {
        return;
    }
    let mut execution = pipe_execution(b"drop", b"keep");
    execution.status_lost = false;
    execution.discard(Stream::Stdout);
    execution.cancel();
    execution.end();
    assert!(matches!(
        execution.kill_failure(),
        Some(Event::KillFailed(KillTarget::Program, _))
    ));
    assert!(matches!(
        execution.kill_failure(),
        Some(Event::KillFailed(KillTarget::ProcessGroup, _))
    ));
    assert!(execution.observe(42, ExitStatus::from_raw(9 | 0x80)));
    let mut buffer = [0; 2];
    let mut dropped = 0;
    let mut retained = Vec::new();
    loop {
        match execution.next_event(&mut buffer).unwrap() {
            Some(Event::Discarded(Stream::Stdout, length)) => dropped += length,
            Some(Event::Output(Stream::Stderr, bytes)) => retained.extend_from_slice(bytes),
            Some(Event::Ended(outcome)) => {
                assert_eq!(outcome.cause, Some(Cause::Cancelled));
                assert_eq!(outcome.status.signal(), Some(9));
                assert!(outcome.status.core_dumped());
                assert_eq!(outcome.discarded, [4, 0]);
                break;
            }
            Some(Event::ReadFailed(_, error)) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(dropped, 4);
    assert_eq!(retained, b"keep");
}

#[test]
fn invalid_requests_fail_before_a_child_starts() {
    let mut invalid = request("exit 0");
    invalid.arguments.clear();
    let failure = Exec::start(invalid).err().unwrap();
    assert_eq!(failure.error.raw_os_error(), Some(Errno::EINVAL as i32));
    assert!(failure.process.is_none());
    let mut invalid = request("exit 0");
    invalid.directory = c"relative".to_owned();
    assert_eq!(
        Exec::start(invalid).err().unwrap().error.raw_os_error(),
        Some(Errno::EINVAL as i32)
    );
    let mut invalid = request("exit 0");
    invalid.uid = u32::MAX;
    assert!(Exec::start(invalid).is_err());
    let mut missing = request("exit 0");
    missing.uid = 4_000_000_000;
    missing.account = AccountMode::Existing;
    assert!(Exec::start(missing).is_err());
}

/// Forces real failures at both post-fork descriptor boundaries while init remains the reaper.
fn failed_start_collector_checks(simulated: bool) {
    struct Helper(std::process::Child);
    impl Drop for Helper {
        fn drop(&mut self) {
            match ProcessId::from_child(&self.0).is_unreaped() {
                Ok(true) => {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
                Ok(false) => {}
                Err(error) => sys::write_standard_error(
                    format!("cannot verify the owned helper for cleanup: {error}\n").as_bytes(),
                ),
            }
        }
    }
    sys::reset_child_signal();
    let signals = SignalDescriptor::new(&SIGNALS).unwrap();
    let mut helper = Helper(
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap(),
    );
    let mut runtime = Runtime {
        system: RealSystem {
            signals: Some(signals),
        },
        helpers: Helpers {
            time: ProcessId::from_child(&helper.0),
            announcer: None,
        },
        terminating: false,
    };
    for report_failure in [true, true, false, false] {
        let (failure, expected) = if report_failure {
            let mut spec = sys::ChildSpec::new(
                c"/bin/sh".to_owned(),
                vec![
                    c"sh".to_owned(),
                    c"-c".to_owned(),
                    c"exec sleep 30".to_owned(),
                ],
            );
            spec.set_workload(0, 0);
            if simulated {
                spec.simulate_credentials(None);
            }
            let mut started = sys::PreparedChild::new(spec).unwrap().spawn().unwrap();
            started.report = sys::open(
                c"/",
                OpenFlags::O_PATH | OpenFlags::O_CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            (
                Exec::from_started(started.process, started.report, None, [None, None], None)
                    .err()
                    .unwrap(),
                Errno::EBADF,
            )
        } else {
            sys::tests::deny_system_call(nix::libc::SYS_pidfd_open);
            let values = request("exec sleep 30");
            let failure = if simulated {
                Exec::start_simulated(values, None)
            } else {
                Exec::start(values)
            };
            (failure.err().unwrap(), Errno::EPERM)
        };
        let process = failure.process.expect("failed start lost its owned child");
        assert_eq!(failure.error.raw_os_error(), Some(expected as i32));
        assert!(failure.cleanup.is_none(), "{failure:?}");
        assert!(failure.is_pending().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = Vec::new();
        while observed.is_empty() {
            assert!(
                Instant::now() < deadline,
                "failed start did not wake its owner"
            );
            let [wake] = sys::poll([runtime.wake()], Some(Duration::from_millis(100))).unwrap();
            if !wake.readable {
                continue;
            }
            assert!(
                !runtime
                    .service(|id, status| observed.push((id, status)))
                    .unwrap()
            );
        }
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].0, process.raw().unsigned_abs());
        assert_eq!(observed[0].1.signal(), Some(9));
        assert_eq!(failure.error.raw_os_error(), Some(expected as i32));
        assert!(!failure.is_pending().unwrap());
        assert_eq!(sys::try_wait(process), Err(Errno::ECHILD));
    }
    helper.0.kill().unwrap();
    nix::sys::signal::kill(nix::unistd::getpid(), Signal::SIGTERM).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "helper failure was not collected"
        );
        match runtime.service(|_, _| {}) {
            Err(error) => {
                assert!(error.contains("the time daemon"));
                assert!(error.contains("signal 9"));
                break;
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

#[test]
fn postfork_start_failures_wake_the_owner_and_retain_each_child_until_collected() {
    let name = "guest::init::tests::exec::postfork_start_failures_wake_the_owner_and_retain_each_child_until_collected";
    if isolate_process_test("NVX_INIT_START_FAILURE_TEST", name) {
        return;
    }
    in_child(|| failed_start_collector_checks(true));
}

#[test]
fn a_failed_start_detects_automatic_status_collection_without_losing_its_first_error() {
    let name = "guest::init::tests::exec::a_failed_start_detects_automatic_status_collection_without_losing_its_first_error";
    if isolate_process_test("NVX_INIT_START_AUTO_REAP_TEST", name) {
        return;
    }
    for ignored in [false, true] {
        in_child(|| {
            sys::tests::reap_children_automatically(ignored).unwrap();
            sys::tests::deny_system_call(nix::libc::SYS_pidfd_open);
            let failure = Exec::start_simulated(request("exec sleep 30"), None)
                .err()
                .unwrap();
            assert!(failure.process.is_some());
            let deadline = Instant::now() + Duration::from_secs(5);
            while failure.is_pending().unwrap() {
                assert!(
                    Instant::now() < deadline,
                    "automatically collected child remained pending"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(failure.error.raw_os_error(), Some(Errno::EPERM as i32));
            assert!(failure.cleanup.is_none());
        });
    }
}

#[test]
fn a_failed_start_preserves_its_first_errno_and_the_cleanup_failure() {
    let name = "guest::init::tests::exec::a_failed_start_preserves_its_first_errno_and_the_cleanup_failure";
    if isolate_process_test("NVX_INIT_START_CLEANUP_TEST", name) {
        return;
    }
    in_child(|| {
        sys::reset_child_signal();
        let signals = SignalDescriptor::new(&SIGNALS).unwrap();
        let spec = sys::ChildSpec::new(c"/bin/true".to_owned(), vec![c"true".to_owned()]);
        let mut started = sys::PreparedChild::new(spec).unwrap().spawn().unwrap();
        let process = started.process;
        let descriptor = sys::open_pidfd(process).unwrap();
        assert!(sys::readable_or_hung_up(&[descriptor.borrowed()], 1000).unwrap()[0]);
        started.report = sys::open(
            c"/",
            OpenFlags::O_PATH | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        sys::tests::deny_system_call(nix::libc::SYS_kill);
        let failure = Exec::from_started(started.process, started.report, None, [None, None], None)
            .err()
            .unwrap();
        assert!(failure.is_pending().unwrap());
        let (error, owned, cleanup) = failure.into_parts();
        assert_eq!(owned, Some(process));
        assert_eq!(error.raw_os_error(), Some(Errno::EBADF as i32));
        assert_eq!(cleanup, Some(Errno::EPERM));
        let mut runtime = Runtime {
            system: RealSystem {
                signals: Some(signals),
            },
            helpers: Helpers {
                time: pid(2_000_000),
                announcer: None,
            },
            terminating: false,
        };
        let mut observed = Vec::new();
        runtime
            .service(|id, status| observed.push((id, status)))
            .unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].0, owned.unwrap().raw().unsigned_abs());
        assert!(observed[0].1.success());
        assert!(!owned.unwrap().is_unreaped().unwrap());
        assert_eq!(error.raw_os_error(), Some(Errno::EBADF as i32));
        assert_eq!(cleanup, Some(Errno::EPERM));
    });
}

#[test]
#[ignore = "requires a privileged Linux guest"]
fn privileged_failed_start_handoff_wakes_the_sole_collector_without_control_input() {
    isolated(
        "privileged_failed_start_handoff_wakes_the_sole_collector_without_control_input",
        || failed_start_collector_checks(false),
    );
}

#[test]
#[ignore = "requires a privileged Linux guest"]
fn privileged_cleanup_keeps_group_failure_when_the_leader_is_already_gone() {
    isolated(
        "privileged_cleanup_keeps_group_failure_when_the_leader_is_already_gone",
        || {
            sys::tests::reap_children_automatically(true).unwrap();
            let (mut output, writer) = sys::create_pipe().unwrap();
            let mut spec = sys::ChildSpec::new(
                c"/bin/sh".to_owned(),
                vec![
                    c"sh".to_owned(),
                    c"-c".to_owned(),
                    c"sleep 30 >/dev/null 2>&1 & echo $!; exit 0".to_owned(),
                ],
            );
            spec.set_workload(0, 0);
            spec.set_stream(1, writer);
            let started = sys::PreparedChild::new(spec).unwrap().spawn().unwrap();
            let leader = started.process;
            let mut text = String::new();
            output.read_to_string(&mut text).unwrap();
            let descendant = ProcessId::from_positive(text.trim().parse().unwrap()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while leader.is_unreaped().unwrap() {
                assert!(
                    Instant::now() < deadline,
                    "leader was not automatically collected"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(descendant.is_unreaped().unwrap());
            sys::tests::deny_group_kill();
            let failure = Exec::from_started(leader, started.report, None, [None, None], None)
                .err()
                .unwrap();
            sys::kill_process(descendant).unwrap();
            assert_eq!(failure.process, Some(leader));
            assert_eq!(failure.error.raw_os_error(), Some(Errno::ESRCH as i32));
            assert_eq!(failure.cleanup, Some(Errno::EPERM));
            assert!(!failure.is_pending().unwrap());
        },
    );
}

#[test]
fn host_launch_tests_use_the_syscall_seam_without_mocking_the_execution_core() {
    let name = "guest::init::tests::exec::host_launch_tests_use_the_syscall_seam_without_mocking_the_execution_core";
    if crate::guest::managed_agent::tests::isolate_process_test("NVX_INIT_NATIVE_LAUNCH_TEST", name)
    {
        return;
    }
    in_child(|| {
        sys::reset_child_signal();
        let mut execution = Exec::start_simulated(request("printf native; exit 9"), None).unwrap();
        let mut collected = Collected::default();
        finish(&mut execution, &mut collected);
        assert_eq!(collected.stdout, b"native");
        assert_eq!(collected.outcome.unwrap().status.code(), Some(9));
        let mut execution = Exec::start_simulated(request("exit 99"), Some(Errno::EPERM)).unwrap();
        let mut collected = Collected::default();
        finish(&mut execution, &mut collected);
        assert_eq!(
            collected.launch,
            Some(Some(LaunchFailure {
                stage: 14,
                error: Errno::EPERM as i32
            }))
        );
        assert_eq!(collected.outcome.unwrap().status.code(), Some(125));
    });
}

#[test]
fn native_workload_cwd_accepts_procfs_links_but_closes_inherited_descriptors() {
    let name = "guest::init::tests::exec::native_workload_cwd_accepts_procfs_links_but_closes_inherited_descriptors";
    if crate::guest::managed_agent::tests::isolate_process_test("NVX_INIT_PROCFS_CWD_TEST", name) {
        return;
    }
    in_child(|| {
        sys::reset_child_signal();
        procfs_workload_cwd_checks(true);
    });
}

fn procfs_workload_cwd_checks(simulated: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = sys::path_c_string(directory.path()).unwrap();
    sys::chmod(&path, Mode::from_bits_truncate(0o755)).unwrap();
    nix::unistd::chdir(directory.path()).unwrap();
    let inherited = std::fs::File::open(directory.path()).unwrap();
    let descriptor = std::os::fd::AsRawFd::as_raw_fd(&inherited);
    let (uid, gid) = if simulated {
        (
            nix::unistd::geteuid().as_raw(),
            nix::unistd::getegid().as_raw(),
        )
    } else {
        (65534, 65534)
    };
    let expected = format!(
        "{uid}\n{gid}\n{}\n",
        std::env::current_dir().unwrap().display()
    );
    for (directory, failure) in [
        (c"/proc/self/cwd".to_owned(), None),
        (
            CString::new(format!("/proc/self/fd/{descriptor}")).unwrap(),
            Some(Errno::ENOENT),
        ),
    ] {
        let mut values = request("id -u; id -g; pwd -P");
        values.uid = uid;
        values.gid = gid;
        values.directory = directory;
        let mut execution = if simulated {
            Exec::start_simulated(values, None)
        } else {
            Exec::start(values)
        }
        .unwrap();
        let mut collected = Collected::default();
        finish(&mut execution, &mut collected);
        match failure {
            None => {
                assert_eq!(collected.launch, Some(None));
                assert_eq!(collected.stdout, expected.as_bytes());
                assert!(collected.outcome.unwrap().status.success());
            }
            Some(error) => {
                assert_eq!(
                    collected.launch,
                    Some(Some(LaunchFailure {
                        stage: 18,
                        error: error as i32
                    }))
                );
                assert!(collected.stdout.is_empty());
                assert_eq!(collected.outcome.unwrap().status.code(), Some(125));
            }
        }
    }
}

#[test]
#[ignore = "requires a guest's root credentials"]
fn privileged_workload_cwd_accepts_procfs_links_after_dropping_credentials() {
    isolated(
        "privileged_workload_cwd_accepts_procfs_links_after_dropping_credentials",
        || procfs_workload_cwd_checks(false),
    );
}

#[test]
fn a_stolen_or_auto_reaped_status_is_reported_once_and_never_reclassified_by_an_ending() {
    let name = "guest::init::tests::exec::a_stolen_or_auto_reaped_status_is_reported_once_and_never_reclassified_by_an_ending";
    if crate::guest::managed_agent::tests::isolate_process_test("NVX_INIT_LOST_STATUS_TEST", name) {
        return;
    }
    in_child(|| {
        for automatic in [None, Some(false), Some(true)] {
            if let Some(ignored) = automatic {
                sys::tests::reap_children_automatically(ignored).unwrap();
            } else {
                sys::reset_child_signal();
            }
            let mut values = request("printf retained; exit 7");
            values.timeout = Some(Duration::from_secs(30));
            let mut execution = Exec::start_simulated(values, None).unwrap();
            let status = if automatic.is_some() {
                7 << 8
            } else {
                sys::wait_named(sys::WaitTarget::Process(execution.leader), true)
                    .unwrap()
                    .unwrap()
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match execution.next_event(&mut []) {
                    Err(error) => {
                        assert_eq!(error.raw_os_error(), Some(Errno::ECHILD as i32));
                        break;
                    }
                    Ok(_) => {
                        assert!(Instant::now() < deadline, "lost status never surfaced");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            assert!(execution.input().is_none());
            assert!(execution.deadline().is_none());
            execution.cancel();
            execution.end();
            execution.reset();
            assert_eq!(execution.cause, None);
            assert!(execution.next_event(&mut []).is_ok());
            assert!(execution.observe(execution.id(), ExitStatus::from_raw(status)));
            let mut collected = Collected::default();
            finish(&mut execution, &mut collected);
            assert_eq!(collected.outcome.unwrap().status.code(), Some(7));
            assert_eq!(collected.outcome.unwrap().cause, None);
        }
    });
}

#[test]
fn a_capture_sweep_refuses_to_run_outside_pid_1() {
    let helpers = Helpers {
        time: pid(42),
        announcer: Some(pid(43)),
    };
    assert_eq!(
        helpers.sweep(Instant::now() + CAPTURE_TIMEOUT),
        Err(Errno::EPERM)
    );
}

#[test]
fn the_sweep_repeats_and_preserves_only_pid_1_and_registered_helpers() {
    let helpers = Helpers {
        time: pid(42),
        announcer: Some(pid(43)),
    };
    let mut passes = 0;
    let mut killed = Vec::new();
    helpers
        .sweep_with(
            || {
                passes += 1;
                Ok(match passes {
                    1 => vec![pid(1), pid(42), pid(43), pid(51)],
                    2 => vec![pid(1), pid(42), pid(43), pid(52)],
                    _ => vec![pid(1), pid(42), pid(43)],
                })
            },
            |process| {
                killed.push(process.raw());
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(passes, 3);
    assert_eq!(killed, [51, 52]);
    assert_eq!(
        helpers.sweep_with(|| Err(Errno::EIO), |_| Ok(()), || Ok(())),
        Err(Errno::EIO)
    );
    assert_eq!(
        helpers.sweep_with(|| Ok(vec![pid(51)]), |_| Err(Errno::EPERM), || Ok(())),
        Err(Errno::EPERM)
    );
    let mut passes = 0;
    helpers
        .sweep_with(
            || {
                passes += 1;
                Ok(if passes == 1 { vec![pid(51)] } else { vec![] })
            },
            |_| Err(Errno::ESRCH),
            || Ok(()),
        )
        .unwrap();
}

#[test]
#[ignore = "requires a guest's root credentials and priority capabilities"]
fn privileged_exec_resets_priority_credentials_and_signals_before_exec() {
    isolated(
        "privileged_exec_resets_priority_credentials_and_signals_before_exec",
        || {
            RealSystem { signals: None }.raise_priority(None).unwrap();
            let _signals = SignalDescriptor::new(&SIGNALS).unwrap();
            let groups = [
                nix::unistd::Gid::from_raw(1000),
                nix::unistd::Gid::from_raw(2000),
            ];
            nix::unistd::setgroups(&groups).unwrap();
            sys::tests::seed_inheritable_and_ambient().unwrap();
            let parent = std::fs::read_to_string("/proc/self/status").unwrap();
            assert!(parent.contains("CapInh:\t0000000000000001\n"));
            assert!(parent.contains("CapAmb:\t0000000000000001\n"));
            for uid in [0, 65534] {
                let mut values = request(
                    "cat /proc/self/status; printf 'oom='; cat /proc/self/oom_score_adj; \
                 printf 'nice='; cut -d ' ' -f 19 /proc/self/stat; \
                 if (printf '%s\\n' -1000 > /proc/self/oom_score_adj) 2>/dev/null; \
                 then exit 99; fi; printf 'floor=locked\\n'",
                );
                values.uid = uid;
                values.gid = uid;
                let mut execution = Exec::start(values).unwrap();
                let mut collected = Collected::default();
                finish(&mut execution, &mut collected);
                assert_eq!(collected.launch, Some(None));
                assert!(collected.outcome.unwrap().status.success());
                let output = String::from_utf8(collected.stdout).unwrap();
                for expected in [
                    format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
                    format!("Gid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
                    "NoNewPrivs:\t1\n".to_owned(),
                    "SigBlk:\t0000000000000000\n".to_owned(),
                    "SigIgn:\t0000000000000000\n".to_owned(),
                    "oom=0\n".to_owned(),
                    "nice=0\n".to_owned(),
                    "floor=locked\n".to_owned(),
                ] {
                    assert!(
                        output.contains(&expected),
                        "missing {expected:?} in {output:?}"
                    );
                }
                assert!(
                    output
                        .lines()
                        .find_map(|line| line.strip_prefix("Groups:"))
                        .unwrap()
                        .trim()
                        .is_empty(),
                    "supplementary groups were not cleared: {output}"
                );
                for set in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
                    assert!(
                        output.contains(&format!("{set}:\t0000000000000000\n")),
                        "{output}"
                    );
                }
                sys::write_standard_error(
                    format!("NVX-INIT-CREDENTIAL-CONTROL uid={uid}\n{output}").as_bytes(),
                );
            }
            assert_eq!(sys::read_number(kernel::OOM_SCORE_ADJUSTMENT), Ok(-1000));
            assert_eq!(nix::unistd::getgroups().unwrap(), groups);
            assert_eq!(
                crate::guest::sys::tests::nice_of(sys::current_process()),
                -10
            );
        },
    );
}

#[test]
#[ignore = "requires a guest's root credentials"]
fn privileged_exec_streams_pipes_and_reports_its_status() {
    isolated(
        "privileged_exec_streams_pipes_and_reports_its_status",
        || {
            let mut execution = Exec::start(request(
                "IFS= read -r line; printf 'out:%s\\n' \"$line\"; \
             printf 'err:%s\\n' \"$line\" >&2; exit 7",
            ))
            .unwrap();
            assert!(execution.input().is_some());
            assert!(execution.descriptors().next().is_some());
            assert!(execution.deadline().is_none());
            assert_eq!(execution.write_input(b"hello\n").unwrap(), 6);
            execution.close_input();
            assert!(execution.input().is_none());
            let mut collected = Collected::default();
            finish(&mut execution, &mut collected);
            assert_eq!(collected.launch, Some(None));
            assert_eq!(collected.stdout, b"out:hello\n");
            assert_eq!(collected.stderr, b"err:hello\n");
            assert_eq!(collected.outcome.unwrap().status.code(), Some(7));
            assert!(execution.descriptors().next().is_none());
            assert!(!execution.observe(execution.id(), ExitStatus::from_raw(0)));
            let mut values = request("if read -r line; then exit 99; fi; pwd");
            values.input = false;
            values.program = c"sh".to_owned();
            values.search_path = Some(c"/bin:/usr/bin".to_owned());
            let mut execution = Exec::start(values).unwrap();
            assert!(execution.input().is_none());
            assert_eq!(
                execution.write_input(b"closed").unwrap_err().raw_os_error(),
                Some(Errno::EPIPE as i32)
            );
            let mut collected = Collected::default();
            finish(&mut execution, &mut collected);
            assert_eq!(collected.stdout, b"/\n");
            assert!(collected.outcome.unwrap().status.success());
            let mut values = request("");
            values.program = c"env".to_owned();
            values.arguments = vec![c"env".to_owned()];
            values.directory = c"/tmp".to_owned();
            values.search_path = Some(c"/bin:/usr/bin".to_owned());
            values.physical_pwd = true;
            values.input = false;
            let mut execution = Exec::start(values).unwrap();
            let mut collected = Collected::default();
            finish(&mut execution, &mut collected);
            assert!(
                String::from_utf8(collected.stdout)
                    .unwrap()
                    .lines()
                    .any(|line| line == "PWD=/tmp")
            );
            assert!(collected.outcome.unwrap().status.success());
        },
    );
}

#[test]
#[ignore = "requires a guest's root credentials"]
fn privileged_exec_checks_account_mode_and_enters_cwd_after_dropping_credentials() {
    isolated(
        "privileged_exec_checks_account_mode_and_enters_cwd_after_dropping_credentials",
        || {
            for account in [AccountMode::Existing, AccountMode::Numeric] {
                let mut values = request("pwd");
                values.account = account;
                if account == AccountMode::Numeric {
                    values.uid = 4_000_000_000;
                    values.gid = 4_000_000_000;
                }
                let mut execution = Exec::start(values).unwrap();
                let mut collected = Collected::default();
                finish(&mut execution, &mut collected);
                assert_eq!(collected.launch, Some(None));
                assert_eq!(collected.stdout, b"/\n");
            }
            let directory = tempfile::tempdir().unwrap();
            sys::chmod(
                &sys::path_c_string(directory.path()).unwrap(),
                Mode::S_IRWXU,
            )
            .unwrap();
            let mut values = request("exit 0");
            values.uid = 65534;
            values.gid = 65534;
            values.directory = CString::new(directory.path().as_os_str().as_bytes()).unwrap();
            let mut execution = Exec::start(values).unwrap();
            let mut collected = Collected::default();
            finish(&mut execution, &mut collected);
            let failure = collected.launch.unwrap().unwrap();
            assert_eq!(failure.stage, 18);
            assert_eq!(failure.error, Errno::EACCES as i32);
            assert_eq!(collected.outcome.unwrap().status.code(), Some(125));
            let path = sys::path_c_string(directory.path()).unwrap();
            sys::chown(&path, 65534, 65534).unwrap();
            let mut values = request("exit 99");
            values.directory = path;
            let mut execution = Exec::start(values).unwrap();
            let mut collected = Collected::default();
            finish(&mut execution, &mut collected);
            assert_eq!(
                collected.launch,
                Some(Some(LaunchFailure {
                    stage: 18,
                    error: Errno::EACCES as i32
                }))
            );
            assert_eq!(collected.outcome.unwrap().status.code(), Some(125));
            let root = tempfile::tempdir().unwrap();
            let etc = root.path().join("etc");
            std::fs::create_dir(&etc).unwrap();
            let passwd = "root:x:0:0::/:/bin/sh\n";
            let group = "root:x:0:\n";
            for (users, groups, expected) in [
                (passwd.to_owned(), group, Ok(())),
                (
                    format!("# ignored:x:0:0::/:/bin/sh\n{passwd}"),
                    group,
                    Ok(()),
                ),
                (format!("{passwd}{passwd}"), group, Err(Errno::EINVAL)),
                (
                    "other:x:1:0::/:/bin/sh\n".to_owned(),
                    group,
                    Err(Errno::ENOENT),
                ),
                ("root:x:0:0::/\n".to_owned(), group, Err(Errno::EINVAL)),
                (":x:0:0::/:/bin/sh\n".to_owned(), group, Err(Errno::EINVAL)),
                (
                    "root:x:0:1::/:/bin/sh\n".to_owned(),
                    group,
                    Err(Errno::EINVAL),
                ),
                (
                    "root:x:0:0::relative:/bin/sh\n".to_owned(),
                    group,
                    Err(Errno::EINVAL),
                ),
                (passwd.to_owned(), "other:x:1:\n", Err(Errno::EINVAL)),
            ] {
                std::fs::write(etc.join("passwd"), users).unwrap();
                std::fs::write(etc.join("group"), groups).unwrap();
                check_account_fixture(root.path(), expected);
            }
            std::fs::write(etc.join("group"), group).unwrap();
            std::fs::write(etc.join("users"), passwd).unwrap();
            std::fs::remove_file(etc.join("passwd")).unwrap();
            std::os::unix::fs::symlink("users", etc.join("passwd")).unwrap();
            check_account_fixture(root.path(), Err(Errno::ELOOP));
            std::fs::remove_file(etc.join("passwd")).unwrap();
            nix::unistd::mkfifo(&etc.join("passwd"), nix::sys::stat::Mode::S_IRWXU).unwrap();
            check_account_fixture(root.path(), Err(Errno::EINVAL));
            std::fs::remove_file(etc.join("passwd")).unwrap();
            std::fs::File::create(etc.join("passwd"))
                .unwrap()
                .set_len((1 << 20) + 1)
                .unwrap();
            check_account_fixture(root.path(), Err(Errno::EINVAL));
        },
    );
}

/// Reads fixture account files only in a guest test's forked child; production never changes root.
fn check_account_fixture(root: &std::path::Path, expected: Result<(), Errno>) {
    match sys::fork().unwrap() {
        sys::ForkResult::Child => {
            nix::unistd::chroot(root).unwrap();
            nix::unistd::chdir("/").unwrap();
            let mut values = request("exit 99");
            values.account = AccountMode::Existing;
            assert_eq!(
                values.check().map_err(|error| error.raw_os_error()),
                expected.map_err(|error| Some(error as i32))
            );
            sys::exit_immediately(0);
        }
        sys::ForkResult::Parent(child) => {
            assert_eq!(sys::wait(child), Some(ChildStatus::Exited(0)));
        }
    }
}

#[test]
#[ignore = "requires a guest's root credentials"]
fn privileged_every_ending_kills_the_process_group_including_normal_completion() {
    isolated(
        "privileged_every_ending_kills_the_process_group_including_normal_completion",
        || {
            for ending in 0..5 {
                let script = if ending == 0 {
                    "sleep 30 & echo $!; exit 7"
                } else {
                    "sleep 30 & echo $!; wait"
                };
                let mut values = request(script);
                if ending == 3 {
                    values.timeout = Some(Duration::from_secs(2));
                }
                let mut execution = Exec::start(values).unwrap();
                let mut collected = Collected::default();
                until(&mut execution, &mut collected, |collected| {
                    collected.stdout.contains(&b'\n')
                });
                let descendant: u32 = std::str::from_utf8(&collected.stdout)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                match ending {
                    0 => {}
                    1 => execution.end(),
                    2 => execution.cancel(),
                    3 => assert!(execution.deadline().is_some()),
                    4 => execution.reset(),
                    _ => unreachable!(),
                }
                if ending != 0 {
                    let descendant = pid(i32::try_from(descendant).unwrap());
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while ProcessId::live_userspace().unwrap().contains(&descendant) {
                        if ending == 3 {
                            let mut buffer = [0; 4096];
                            match execution.next_event(&mut buffer).unwrap() {
                                Some(Event::KillFailed(target, error)) => {
                                    panic!("kill {target:?}: {error}")
                                }
                                Some(Event::ReadFailed(stream, error)) => {
                                    panic!("read {stream:?}: {error}")
                                }
                                _ => {}
                            }
                        }
                        assert!(
                            Instant::now() < deadline,
                            "ending left its group alive before observation"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    assert!(sys::process_exists(pid(
                        i32::try_from(execution.id()).unwrap()
                    )));
                    sys::write_standard_error(format!("NVX-INIT-GROUP-CONTROL ending={ending} descendant-dead-before-observe\n").as_bytes());
                }
                finish(&mut execution, &mut collected);
                until(&mut execution, &mut collected, |collected| {
                    collected.reaped.contains(&descendant)
                });
                let outcome = collected.outcome.unwrap();
                let expected = [
                    None,
                    Some(Cause::Terminated),
                    Some(Cause::Cancelled),
                    Some(Cause::TimedOut),
                    Some(Cause::Disconnected),
                ][ending];
                assert_eq!(outcome.cause, expected);
                if ending == 0 {
                    assert_eq!(outcome.status.code(), Some(7));
                } else {
                    assert_eq!(outcome.status.signal(), Some(nix::libc::SIGKILL));
                }
                assert!(!sys::process_exists(
                    pid(i32::try_from(descendant).unwrap())
                ));
            }
        },
    );
}

#[test]
#[ignore = "requires a guest's root credentials and private PID/procfs mounts"]
fn privileged_capture_sweep_kills_setsid_escapees_and_spares_both_helpers() {
    isolated(
        "privileged_capture_sweep_kills_setsid_escapees_and_spares_both_helpers",
        || {
            use nix::mount::{MsFlags, mount};
            use nix::sched::{CloneFlags, unshare};
            unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID).unwrap();
            mount(
                None::<&str>,
                "/",
                None::<&str>,
                MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                None::<&str>,
            )
            .unwrap();
            match sys::fork().unwrap() {
                sys::ForkResult::Parent(child) => {
                    assert_eq!(sys::wait(child), Some(ChildStatus::Exited(0)))
                }
                sys::ForkResult::Child => {
                    mount(
                        Some("proc"),
                        "/proc",
                        Some("proc"),
                        MsFlags::empty(),
                        None::<&str>,
                    )
                    .unwrap();
                    assert_eq!(sys::current_process().raw(), 1);
                    let mut time = std::process::Command::new("sleep")
                        .arg("30")
                        .spawn()
                        .unwrap();
                    let mut announcer = std::process::Command::new("sleep")
                        .arg("30")
                        .spawn()
                        .unwrap();
                    let helpers = Helpers {
                        time: ProcessId::from_child(&time),
                        announcer: Some(ProcessId::from_child(&announcer)),
                    };
                    let arguments = helpers
                        .named()
                        .map(|(_, process)| CString::new(process.raw().to_string()).unwrap())
                        .collect::<Vec<_>>();
                    let runtime = Runtime::new(&arguments).unwrap();
                    assert!(matches!(runtime.wake(), Pollable::Signals(_)));
                    let mut execution =
                        Exec::start(request("setsid sleep 30 >/dev/null 2>&1 & echo $!; wait"))
                            .unwrap();
                    let mut collected = Collected::default();
                    until(&mut execution, &mut collected, |collected| {
                        collected.stdout.contains(&b'\n')
                    });
                    let escaped: i32 = std::str::from_utf8(&collected.stdout)
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    let escaped = pid(escaped);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while sys::process_group_of(escaped).unwrap() != escaped {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    execution.end();
                    finish(&mut execution, &mut collected);
                    assert!(sys::process_exists(escaped));
                    let mut held = Exec::start(request("setsid sleep 30 & echo $!; wait")).unwrap();
                    let mut held_output = Collected::default();
                    until(&mut held, &mut held_output, |output| {
                        output.stdout.contains(&b'\n')
                    });
                    let held_id = pid(std::str::from_utf8(&held_output.stdout)
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap());
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while sys::process_group_of(held_id).unwrap() != held_id {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    held.reset();
                    finish(&mut held, &mut held_output);
                    assert!(sys::process_exists(held_id));
                    assert_eq!(
                        held_output.outcome.unwrap().cause,
                        Some(Cause::Disconnected)
                    );
                    #[allow(clippy::zombie_processes)]
                    let unrelated = std::process::Command::new("sleep")
                        .arg("30")
                        .spawn()
                        .unwrap();
                    let unrelated = ProcessId::from_child(&unrelated);
                    let (ready, writer) = sys::create_pipe().unwrap();
                    let threaded = match sys::fork().unwrap() {
                        sys::ForkResult::Child => {
                            drop(ready);
                            std::thread::spawn(move || {
                                writer.write_all(b"1").unwrap();
                                loop {
                                    std::thread::sleep(Duration::from_secs(1));
                                }
                            });
                            sys::tests::exit_main_thread();
                        }
                        sys::ForkResult::Parent(child) => {
                            drop(writer);
                            let mut byte = [0];
                            assert_eq!(ready.read(&mut byte), Ok(1));
                            child
                        }
                    };
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        let stat =
                            std::fs::read_to_string(format!("/proc/{}/stat", threaded.raw()))
                                .unwrap();
                        if stat.rsplit_once(')').unwrap().1.split_whitespace().next() == Some("Z") {
                            break;
                        }
                        assert!(Instant::now() < deadline, "the main thread did not exit");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    assert!(ProcessId::live_userspace().unwrap().contains(&threaded));
                    helpers.sweep(Instant::now() + CAPTURE_TIMEOUT).unwrap();
                    while let Some(child) = sys::reap_any_nonblocking() {
                        collected.reaped.push(child.process.raw().unsigned_abs());
                    }
                    assert!(!ProcessId::live_userspace().unwrap().contains(&escaped));
                    assert!(!ProcessId::live_userspace().unwrap().contains(&held_id));
                    assert!(!ProcessId::live_userspace().unwrap().contains(&unrelated));
                    assert!(!ProcessId::live_userspace().unwrap().contains(&threaded));
                    assert!(sys::process_exists(helpers.time));
                    assert!(sys::process_exists(helpers.announcer.unwrap()));
                    assert_eq!(sys::current_process().raw(), 1);
                    sys::write_standard_error(b"NVX-INIT-SWEEP-CONTROL escaped-and-threaded-dead helpers-alive pid1-alive\n");
                    time.kill().unwrap();
                    announcer.kill().unwrap();
                    time.wait().unwrap();
                    announcer.wait().unwrap();
                    sys::exit_immediately(0);
                }
            }
        },
    );
}
