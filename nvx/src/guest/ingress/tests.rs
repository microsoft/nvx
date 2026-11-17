//! Checks of the shared file helpers, and of the launch of the announcer that `start` runs.

use std::os::unix::fs::PermissionsExt;

use nix::sys::signal::SigSet;
use nix::sys::wait::WaitStatus;

use super::*;

/// The gateway of the tests' announcers.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

/// Writes an executable shell script in `directory` whose body is `script`, which stands for the
/// announcer's program, and returns its path.
fn announcer(directory: &Path, script: &str) -> PathBuf {
    let path = directory.join("announcer");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Returns a socket that stands for the announcer's, bound to a port that needs no privilege.
fn socket() -> sys::UdpSocket {
    let socket = sys::UdpSocket::ipv4().unwrap();
    socket.bind_any(0).unwrap();
    socket
}

/// Launches the announcer `script` as `start` does, and waits until it exits with 0, which the
/// test process, that runs alone, collects as its only child.
fn launch_and_wait(directory: &Path, script: &str) {
    launch(&announcer(directory, script), GATEWAY, socket()).unwrap();
    assert!(matches!(
        nix::sys::wait::wait().unwrap(),
        WaitStatus::Exited(_, 0)
    ));
}

#[test]
fn an_announcer_runs_its_program_with_its_socket_in_a_session_of_its_own() {
    // The test collects the status of any child, so it runs in a process of its own.
    if sys::tests::isolate_process_test(
        "NVX_INGRESS_LAUNCH_TEST_CHILD",
        "guest::ingress::tests::an_announcer_runs_its_program_with_its_socket_in_a_session_of_its_own",
    ) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report");
    let script = format!(
        "read -r pid comm state ppid group session terminal rest </proc/self/stat\n\
         {{ echo \"$pid $group $session $terminal\"; printf '%s\\n' \"$@\"; \
         stat -L -c %F /dev/stdin; }} >'{}'",
        report.display()
    );
    launch_and_wait(directory.path(), &script);
    let reported = std::fs::read_to_string(&report).unwrap();
    let lines: Vec<_> = reported.lines().collect();
    let ids: Vec<_> = lines[0].split(' ').collect();
    // It leads a session and a process group of its own, without a controlling terminal.
    assert_eq!(ids[1..], [ids[0], ids[0], "0"], "{reported}");
    // The program takes the gateway after `inherit`, and the socket as its standard input.
    assert_eq!(lines[1..], ["inherit", "10.0.0.1", "socket"], "{reported}");
}

#[test]
fn an_announcer_starts_with_every_signal_s_default_disposition_and_none_blocked() {
    // Ignoring a signal changes the whole process, so the test runs in a process of its own.
    if sys::tests::isolate_process_test(
        "NVX_INGRESS_SIGNALS_TEST_CHILD",
        "guest::ingress::tests::an_announcer_starts_with_every_signal_s_default_disposition_and_none_blocked",
    ) {
        return;
    }
    // The caller ignores every signal that it may, but `SIGCHLD`, which would have the kernel
    // collect the announcer's status, and blocks every signal.
    let ignored: Vec<_> = sys::Signal::iterator()
        .filter(|signal| {
            ![
                sys::Signal::SIGKILL,
                sys::Signal::SIGSTOP,
                sys::Signal::SIGCHLD,
            ]
            .contains(signal)
        })
        .collect();
    sys::ignore_signals(&ignored);
    SigSet::all().thread_block().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report");
    let script = format!("exec cat /proc/self/status >'{}'", report.display());
    launch_and_wait(directory.path(), &script);
    let status = std::fs::read_to_string(&report).unwrap();
    for name in ["SigIgn:", "SigBlk:"] {
        let set = status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("{name} is missing from {status}"));
        assert_eq!(u64::from_str_radix(set.trim(), 16), Ok(0), "{status}");
    }
}

#[test]
fn an_announcer_whose_program_does_not_run_does_not_start() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let problem = launch(&missing, GATEWAY, socket()).unwrap_err();
    let error = io::Error::from(Errno::ENOENT);
    assert_eq!(
        problem,
        format!("{} did not start: {error}", missing.display())
    );
}

#[test]
fn an_announcer_does_not_start_without_its_program_when_its_caller_ignores_sigchld() {
    // Ignoring `SIGCHLD` changes the whole process, so the test runs in a process of its own.
    if sys::tests::isolate_process_test(
        "NVX_INGRESS_IGNORED_SIGCHLD_TEST_CHILD",
        "guest::ingress::tests::an_announcer_does_not_start_without_its_program_when_its_caller_ignores_sigchld",
    ) {
        return;
    }
    // A caller inherits an ignored `SIGCHLD` across `exec`, which has the kernel collect the
    // child that does not run the program.
    sys::tests::reap_children_automatically(true).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let problem = launch(&missing, GATEWAY, socket()).unwrap_err();
    let error = io::Error::from(Errno::ENOENT);
    assert_eq!(
        problem,
        format!("{} did not start: {error}", missing.display())
    );
    assert!(!sys::tests::has_child());
}

/// Verifies that decimal files hold exactly one plain number, and their failure outcomes.
#[test]
fn decimal_files_hold_one_plain_number() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("value");
    for (contents, expected) in [
        (&b"0\n"[..], DecimalFile::Value(0)),
        (b"1000\n", DecimalFile::Value(1000)),
        (b" \t42 \n", DecimalFile::Value(42)),
        (b"18446744073709551615\n", DecimalFile::Value(u64::MAX)),
        (b"18446744073709551616\n", DecimalFile::Malformed),
        (b"+7\n", DecimalFile::Malformed),
        (b"-1\n", DecimalFile::Malformed),
        (b"0x10\n", DecimalFile::Malformed),
        (b"0trailing\n", DecimalFile::Malformed),
        (b"1\n2\n", DecimalFile::Malformed),
        (b"\x0b7\n", DecimalFile::Malformed),
        (b"\n", DecimalFile::Malformed),
        (b"", DecimalFile::Malformed),
    ] {
        std::fs::write(&path, contents).unwrap();
        assert_eq!(
            read_decimal_file(&path),
            expected,
            "{}",
            contents.escape_ascii()
        );
    }
    assert_eq!(
        read_decimal_file(&directory.path().join("missing")),
        DecimalFile::Missing
    );
    assert_eq!(read_decimal_file(directory.path()), DecimalFile::Malformed);
}
