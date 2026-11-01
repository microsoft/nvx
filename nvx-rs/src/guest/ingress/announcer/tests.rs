use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::time::{Duration, Instant};

use super::*;
use crate::guest::spawn::LaunchStage;

/// How long a test waits for an announcer to do what it expects.
const PATIENCE: Duration = Duration::from_secs(10);

/// The gateway of the tests' announcers.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

/// Writes an executable shell script at `path` whose body is `script`, which stands for the
/// announcer's program, and returns its path.
fn program(directory: &Path, name: &str, script: &str) -> CString {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    CString::new(path.as_os_str().as_bytes()).unwrap()
}

/// Returns a socket that stands for the announcer's, bound to a port that needs no privilege.
fn socket() -> sys::UdpSocket {
    let socket = sys::UdpSocket::ipv4().unwrap();
    socket.bind_any(0).unwrap();
    socket
}

/// Returns whether `announcer`'s pidfd polls readable within `milliseconds`.
fn ended(announcer: &Announcer, milliseconds: u16) -> bool {
    sys::readable_or_hung_up(&[announcer.pidfd()], milliseconds).unwrap() == [true]
}

#[test]
fn an_announcer_runs_its_program_with_its_socket_until_it_is_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report");
    let script = format!(
        "{{ printf '%s\\n' \"$0\" \"$@\"; stat -L -c %F /dev/stdin; }} >{0}.tmp\n\
             mv {0}.tmp {0}\nexec sleep 60",
        report.display()
    );
    let program = program(directory.path(), "announcer", &script);
    let announcer = Announcer::spawn(&program, GATEWAY, socket()).unwrap();
    let deadline = Instant::now() + PATIENCE;
    while !report.exists() {
        assert!(Instant::now() < deadline, "the announcer reported nothing");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The program takes the gateway after `inherit`, and the socket as its standard input.
    let reported = fs::read_to_string(&report).unwrap();
    let lines: Vec<_> = reported.lines().collect();
    assert_eq!(lines[1..], ["inherit", "10.0.0.1", "socket"], "{reported}");
    assert!(!ended(&announcer, 0));
    let status = announcer.stop().unwrap();
    assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
}

#[test]
fn an_announcer_that_ends_polls_readable_and_keeps_its_status() {
    let directory = tempfile::tempdir().unwrap();
    let program = program(directory.path(), "announcer", "exit 3");
    let mut announcer = Announcer::spawn(&program, GATEWAY, socket()).unwrap();
    assert!(ended(&announcer, 10_000));
    let status = announcer.try_wait().unwrap().unwrap();
    assert_eq!(status.code(), Some(3));
    assert_eq!(announcer.try_wait().unwrap(), Some(status));
    // Its stop signals nothing, and returns the status again.
    assert_eq!(announcer.stop().unwrap(), status);
}

#[test]
fn a_dropped_announcer_ends() {
    let directory = tempfile::tempdir().unwrap();
    let program = program(directory.path(), "announcer", "exec sleep 60");
    let announcer = Announcer::spawn(&program, GATEWAY, socket()).unwrap();
    let process = ProcessId::from_positive(i64::from(announcer.id())).unwrap();
    drop(announcer);
    // The drop ended it and collected its status, so its process ID names no process.
    assert!(!sys::process_exists(process));
}

#[test]
fn an_announcer_whose_program_does_not_run_does_not_start() {
    let directory = tempfile::tempdir().unwrap();
    let missing = CString::new(directory.path().join("missing").as_os_str().as_bytes()).unwrap();
    let error = Announcer::spawn(&missing, GATEWAY, socket()).unwrap_err();
    let StartError::Launch(failure) = &error else {
        panic!("{error}");
    };
    assert_eq!(failure.stage(), LaunchStage::Exec);
    assert_eq!(failure.raw_os_error(), nix::libc::ENOENT);
    assert!(
        error
            .to_string()
            .starts_with("/sbin/nvx-ingress did not start: the exec launch stage failed: "),
        "{error}"
    );
}

#[test]
fn start_errors_name_their_cause() {
    let denied = io::Error::from(Errno::EACCES);
    for (error, expected) in [
        (
            StartError::Socket(io::Error::from(Errno::EMFILE)),
            "the announcer's socket cannot be created: ",
        ),
        (
            StartError::Bind(denied),
            "the announcer's socket cannot be bound to a privileged source port: ",
        ),
        (
            StartError::NoPrivilegedPort,
            "no privileged source port, from 1023 down to 512, is free for the announcer's \
                 socket",
        ),
        (
            StartError::Spawn(io::Error::from(Errno::EAGAIN)),
            "/sbin/nvx-ingress cannot be started: ",
        ),
    ] {
        assert!(error.to_string().starts_with(expected), "{error}");
    }
}
