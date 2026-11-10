//! Checks of announcer ownership and PID 1's collection. Tests that wait for any child run
//! alone in a test process of their own.

use std::env;
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::prctl;
use nix::sys::ptrace;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{self, Id, WaitPidFlag, WaitStatus};
use nix::unistd::{self, Pid};

use super::{collect, numbered, pid, state_and_parent};

/// The variable that names the part that a run of this test binary plays in a test.
const ROLE: &str = "RUNTIME_ADAPTER_COLLECTION_ROLE";
/// The exit status of a tracee that the system does not let its parent trace.
const UNTRACEABLE: i32 = 99;
/// Longest wait for a process to reach a state.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A shell command that stands for the adapter's announcer program.
#[cfg(target_arch = "x86_64")]
fn announcement_command(script: &str) -> nvx::guest::spawn::Command {
    nvx::guest::spawn::Command::new(
        c"/bin/sh".to_owned(),
        vec![
            c"sh".to_owned(),
            c"-c".to_owned(),
            std::ffi::CString::new(script).unwrap(),
            c"nvx-ingress".to_owned(),
            c"inherit".to_owned(),
            c"10.0.0.1".to_owned(),
        ],
    )
}

#[test]
#[cfg(target_arch = "x86_64")]
fn announcer_launch_keeps_the_socket_and_monitors_its_owned_child() {
    use std::io::{BufRead, BufReader};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::ExitStatusExt;

    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let inherited = fs::read_link(format!("/proc/self/fd/{}", socket.as_raw_fd())).unwrap();
    let (read, write) = UnixStream::pair().unwrap();
    read.set_read_timeout(Some(TIMEOUT)).unwrap();
    let command = announcement_command(
        "read -r id comm state ppid pgrp session tty rest </proc/self/stat; \
         printf '%s %s %s %s\\n' \"$id\" \"$pgrp\" \"$session\" \"$tty\"; \
         readlink /proc/self/fd/0; printf '%s\\n' \"$@\"; exec sleep 60",
    )
    .stdout(write);
    let mut announcement = super::Announcement::launch(command, socket).unwrap();
    let mut read = BufReader::new(read);
    let mut report = String::new();
    for _ in 0..4 {
        assert_ne!(read.read_line(&mut report).unwrap(), 0, "{report}");
    }
    let lines: Vec<_> = report.lines().collect();
    let ids: Vec<_> = lines[0].split_whitespace().collect();
    assert_eq!(ids.len(), 4, "{report}");
    assert_eq!(ids[1], ids[0], "{report}");
    assert_eq!(ids[2], ids[0], "{report}");
    assert_eq!(ids[3], "0", "{report}");
    assert_eq!(lines[1], inherited.to_str().unwrap(), "{report}");
    assert_eq!(lines[2..], ["inherit", "10.0.0.1"], "{report}");
    assert!(announcement.0.try_wait().unwrap().is_none());
    assert_eq!(
        announcement.stop().unwrap().signal(),
        Some(Signal::SIGKILL as i32)
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn announcer_drop_stops_and_reaps_the_child_that_it_owns() {
    use std::os::fd::AsFd;

    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let announcement =
        super::Announcement::launch(announcement_command("exec sleep 60"), socket).unwrap();
    let pidfd = announcement
        .0
        .pidfd()
        .unwrap()
        .try_clone_to_owned()
        .unwrap();
    drop(announcement);
    assert_eq!(
        wait::waitid(
            Id::PIDFd(pidfd.as_fd()),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG,
        ),
        Err(nix::errno::Errno::ECHILD),
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn announcer_failed_exec_reports_the_cause_and_leaves_no_child() {
    const TEST: &str = "worker::tests::announcer_failed_exec_reports_the_cause_and_leaves_no_child";
    if env::var(ROLE).as_deref() != Ok("pid-1") {
        isolated(TEST);
        return;
    }
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let command = nvx::guest::spawn::Command::new(
        c"/proc/self/fd/announcer-missing".to_owned(),
        vec![c"nvx-ingress".to_owned()],
    );
    let error = super::Announcement::launch(command, socket).unwrap_err();
    let failure = error
        .get_ref()
        .unwrap()
        .downcast_ref::<nvx::guest::spawn::LaunchFailure>()
        .unwrap();
    assert_eq!(failure.stage(), nvx::guest::spawn::LaunchStage::Exec);
    assert_eq!(failure.raw_os_error(), nix::libc::ENOENT);
    assert_eq!(
        wait::waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)),
        Err(nix::errno::Errno::ECHILD),
    );
}

#[test]
fn pid_1_goes_on_past_the_stops_of_the_threads_that_it_traces() {
    const TEST: &str = "worker::tests::pid_1_goes_on_past_the_stops_of_the_threads_that_it_traces";
    match env::var(ROLE).as_deref() {
        // A thread that asks its parent to trace it stops at each signal that reaches it, until
        // the parent resumes it, which PID 1 never does. It ends with the thread that started it,
        // even in a test that fails, and otherwise outlives every wait of the test.
        Ok("tracee") => {
            prctl::set_pdeathsig(Signal::SIGKILL).unwrap();
            if ptrace::traceme().is_err() {
                std::process::exit(UNTRACEABLE);
            }
            thread::sleep(2 * TIMEOUT);
        }
        Ok("pid-1") => collect_past_stops(TEST),
        _ => isolated(TEST),
    }
}

/// Runs the test `test` alone, in a run of this test binary of its own, where it plays PID 1.
fn isolated(test: &str) {
    let output = Command::new(env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(ROLE, "pid-1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{}\n{stdout}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Plays PID 1, with a thread of each of two children that it traces, which stop before its worker
/// ends: one with `SIGSTOP`, whose report a wait decodes, and one with a real-time signal, whose
/// report it does not. PID 1 takes both stops, and goes on until it collects the worker's status.
fn collect_past_stops(test: &str) {
    let mut tracees = Vec::new();
    for stop in ["STOP", "40"] {
        let tracee = Command::new(env::current_exe().unwrap())
            .args(["--exact", test])
            .env(ROLE, "tracee")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
            .id();
        tracees.push(tracee);
        // Where the system lets no process trace its children, the test stops here.
        let Some(task) = traced(tracee) else {
            end(&tracees);
            return;
        };
        let kill = format!("kill -{stop} {task}");
        let status = Command::new("/bin/sh")
            .args(["-c", &kill])
            .status()
            .unwrap();
        assert!(status.success(), "{kill}: {status}");
        stopped(tracee, task);
    }
    let worker = Command::new("/bin/sh")
        .args(["-c", "sleep 0.2; exit 7"])
        .spawn()
        .unwrap()
        .id();
    assert_eq!(collect(worker), 7);
    end(&tracees);
}

/// Returns the thread of the process `tracee` that this thread traces, once one asked it to, or
/// `None` once the process ended because the system does not let its parent trace it.
fn traced(tracee: u32) -> Option<u32> {
    // The tracer of a thread that asks its parent to trace it is the thread that started it, which
    // `/proc` names by its thread ID.
    let tracer = format!("TracerPid:\t{}", unistd::gettid());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let found = numbered(&format!("/proc/{tracee}/task")).find(|task| {
            let status = fs::read(format!("/proc/{tracee}/task/{task}/status")).unwrap_or_default();
            status
                .split(|&byte| byte == b'\n')
                .any(|line| line == tracer.as_bytes())
        });
        if found.is_some() {
            return found;
        }
        // Until a thread of the tracee is traced, a wait reports nothing for it but its end.
        let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
        match wait::waitid(Id::Pid(pid(tracee)), flags) {
            Ok(WaitStatus::StillAlive) => {}
            Ok(WaitStatus::Exited(_, UNTRACEABLE)) => return None,
            ended => panic!("{tracee} ended untraced: {ended:?}"),
        }
        assert!(Instant::now() < deadline, "no thread of {tracee} is traced");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until the thread `task` of the process `tracee` stopped for its tracer.
fn stopped(tracee: u32, task: u32) {
    let deadline = Instant::now() + TIMEOUT;
    let path = format!("/proc/{tracee}/task/{task}");
    while state_and_parent(&path).is_none_or(|(state, _)| state != 't') {
        assert!(Instant::now() < deadline, "{path} did not stop");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Kills the processes `tracees`, and collects the statuses of what is left of this process's
/// children, and of their threads that it traces.
fn end(tracees: &[u32]) {
    for &tracee in tracees {
        let _ = signal::kill(pid(tracee), Signal::SIGKILL);
    }
    while wait::waitpid(Pid::from_raw(-1), Some(WaitPidFlag::__WALL)).is_ok() {}
}
