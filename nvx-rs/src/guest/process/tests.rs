use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout};

use super::*;
use crate::guest::spawn::tests::shell;

/// Spawns `/bin/sh -c script` and returns its process ID.
fn spawn_shell(script: &str) -> u32 {
    shell(script).spawn().unwrap().id()
}

/// Returns whether `pidfd` polls readable within `timeout`.
fn readable(pidfd: BorrowedFd<'_>, timeout: Duration) -> bool {
    let mut descriptors = [PollFd::new(pidfd, PollFlags::POLLIN)];
    let timeout = PollTimeout::try_from(timeout).unwrap();
    nix::poll::poll(&mut descriptors, timeout).unwrap() == 1
}

/// Spawns a child that ends at once, and waits until it has ended without collecting its
/// status.
fn unrelated_zombie() -> std::process::Child {
    let child = std::process::Command::new("/bin/true").spawn().unwrap();
    let process = ProcessId::from_child(&child);
    while !sys::tests::has_exited(process).unwrap() {
        std::thread::yield_now();
    }
    child
}

#[test]
fn a_named_child_s_status_is_collected_and_no_other() {
    let mut unrelated = unrelated_zombie();
    for (script, code, signal) in [
        ("exit 3", Some(3), None),
        ("kill -TERM $$", None, Some(nix::libc::SIGTERM)),
        // A real-time signal, which `nix`'s wait decoding cannot represent.
        ("kill -40 $$", None, Some(40)),
    ] {
        for with_pidfd in [false, true] {
            let id = spawn_shell(script);
            let mut child = if with_pidfd {
                Child::with_pidfd(id).unwrap()
            } else {
                Child::new(id).unwrap()
            };
            assert_eq!(child.id(), id);
            assert_eq!(child.pidfd().is_some(), with_pidfd);
            let status = child.wait().unwrap();
            assert_eq!((status.code(), status.signal()), (code, signal), "{script}");
            // The status stays, and no further wait takes place.
            assert_eq!(child.try_wait().unwrap(), Some(status));
            assert_eq!(child.wait().unwrap(), status);
            assert_eq!(child.status(), Some(status));
        }
    }
    // Every wait named its child, so the unrelated child's status is still there.
    assert!(unrelated.try_wait().unwrap().unwrap().success());
}

#[test]
fn a_pidfd_polls_readable_once_its_child_ends() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    let spawned = shell("read line").stdin(reader).spawn().unwrap();
    let mut child = Child::with_pidfd(spawned.id()).unwrap();
    let pidfd = child.pidfd().unwrap().try_clone_to_owned().unwrap();
    assert!(!readable(pidfd.as_fd(), Duration::ZERO));
    assert_eq!(child.try_wait().unwrap(), None);
    std::io::Write::write_all(&mut writer, b"go\n").unwrap();
    assert!(readable(pidfd.as_fd(), Duration::from_secs(10)));
    assert!(child.try_wait().unwrap().unwrap().success());
}

#[test]
fn a_child_named_by_the_caller_s_pidfd_is_waited_for_through_it() {
    let id = spawn_shell("exit 4");
    let pidfd = sys::open_pidfd(process_id(id).unwrap()).unwrap();
    let mut child = Child::from_pidfd(id, pidfd.into()).unwrap();
    assert_eq!(child.id(), id);
    assert!(child.pidfd().is_some());
    assert_eq!(child.wait().unwrap().code(), Some(4));
    // A collector of the caller's that took the status first leaves the pidfd's wait none,
    // until it hands the status over.
    let id = spawn_shell("exit 5");
    let pidfd = sys::open_pidfd(process_id(id).unwrap()).unwrap();
    let collected = Child::new(id).unwrap().wait().unwrap();
    let mut child = Child::from_pidfd(id, pidfd.into()).unwrap();
    assert_eq!(sys::errno_of(&child.try_wait().unwrap_err()), Errno::ECHILD);
    assert!(child.observe(id, collected));
    assert_eq!(child.wait().unwrap().code(), Some(5));
    // No process can have these IDs.
    for id in [0, 1 << 31] {
        let pidfd = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let error = Child::from_pidfd(id, pidfd).unwrap_err();
        assert_eq!(sys::errno_of(&error), Errno::EINVAL);
    }
}

#[test]
fn a_child_that_reports_its_end_with_no_signal_is_waited_for() {
    let id = sys::tests::spawn_child_without_exit_signal(7)
        .unwrap()
        .raw()
        .unsigned_abs();
    let mut child = Child::with_pidfd(id).unwrap();
    assert!(readable(child.pidfd().unwrap(), Duration::from_secs(10)));
    assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(7));
    let id = sys::tests::spawn_child_without_exit_signal(8)
        .unwrap()
        .raw()
        .unsigned_abs();
    assert_eq!(Child::new(id).unwrap().wait().unwrap().code(), Some(8));
}

#[test]
fn an_observed_status_belongs_to_its_child_alone() {
    // The test collects the child's status itself, as a PID 1's own loop would.
    let mut process = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 7"])
        .spawn()
        .unwrap();
    let id = process.id();
    let collected = process.wait().unwrap();
    let mut child = Child::new(id).unwrap();
    assert!(!child.observe(id + 1, ExitStatus::from_raw(0)));
    assert_eq!(child.status(), None);
    assert!(child.observe(id, collected));
    // A later status for the ID belongs to another process.
    assert!(!child.observe(id, ExitStatus::from_raw(0)));
    assert_eq!(child.status().unwrap().code(), Some(7));
    // The observed status answers the waits, which would otherwise find no child.
    assert_eq!(child.try_wait().unwrap(), Some(collected));
    assert_eq!(child.wait().unwrap(), collected);
}

#[test]
fn stops_and_continuations_do_not_end_an_observed_child() {
    // The statuses that `waitpid` reports with `WUNTRACED` and `WCONTINUED` for a child that
    // lives on.
    let stopped = ExitStatus::from_raw((nix::libc::SIGSTOP << 8) | 0x7f);
    let continued = ExitStatus::from_raw(0xffff);
    assert_eq!(stopped.stopped_signal(), Some(nix::libc::SIGSTOP));
    assert!(continued.continued());
    let mut child = Child::new(4242).unwrap();
    for status in [stopped, continued] {
        assert!(!child.observe(4242, status), "{status:?}");
        assert_eq!(child.status(), None);
    }
    // The final status still counts.
    let killed = ExitStatus::from_raw(nix::libc::SIGKILL);
    assert!(child.observe(4242, killed));
    assert_eq!(child.status().unwrap().signal(), Some(nix::libc::SIGKILL));
    assert!(!child.observe(4242, stopped));
    assert_eq!(child.status(), Some(killed));
}

/// Returns whether the process `id` reaches a tracing stop within ten seconds, or `false` once
/// it has ended instead.
fn stops_for_its_tracer(id: u32) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{id}/stat")).unwrap();
        // The state follows the command name, in parentheses, which may hold any character.
        match stat
            .rsplit_once(") ")
            .and_then(|(_, rest)| rest.chars().next())
        {
            Some('t') => return true,
            Some('Z') => return false,
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{id} neither stopped nor ended"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_traced_child_s_stop_does_not_end_it() {
    for with_pidfd in [false, true] {
        let id = sys::tests::spawn_traced_child(7).raw().unsigned_abs();
        let mut child = if with_pidfd {
            Child::with_pidfd(id)
        } else {
            Child::new(id)
        }
        .unwrap();
        if !stops_for_its_tracer(id) {
            // The system refuses tracing, which leaves nothing to check.
            assert_eq!(child.wait().unwrap().code(), Some(99));
            return;
        }
        // A wait reports the stop to the child's tracer whatever its options, but the child
        // still runs.
        assert_eq!(child.try_wait().unwrap(), None);
        assert_eq!(child.status(), None);
        sys::tests::continue_traced(child.process());
        assert_eq!(child.wait().unwrap().code(), Some(7));
    }
    // A blocking wait goes on past the stop until the child ends, here by `SIGKILL`.
    let id = sys::tests::spawn_traced_child(7).raw().unsigned_abs();
    let mut child = Child::new(id).unwrap();
    assert!(stops_for_its_tracer(id));
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let process = nix::unistd::Pid::from_raw(i32::try_from(id).unwrap());
        nix::sys::signal::kill(process, nix::sys::signal::Signal::SIGKILL).unwrap();
    });
    assert_eq!(child.wait().unwrap().signal(), Some(nix::libc::SIGKILL));
    killer.join().unwrap();
}

#[test]
fn a_child_whose_status_another_collector_took_is_echild() {
    let mut process = std::process::Command::new("/bin/true").spawn().unwrap();
    let mut child = Child::with_pidfd(process.id()).unwrap();
    // Another collector takes the status, and hands it over to no one.
    process.wait().unwrap();
    assert_eq!(
        child.try_wait().unwrap_err().raw_os_error(),
        Some(Errno::ECHILD as i32)
    );
    assert_eq!(
        child.wait().unwrap_err().raw_os_error(),
        Some(Errno::ECHILD as i32)
    );
    assert_eq!(child.status(), None);
}

#[test]
fn ids_that_no_process_can_have_are_refused() {
    for id in [0, i32::MAX.unsigned_abs() + 1, u32::MAX] {
        for result in [Child::new(id), Child::with_pidfd(id)] {
            assert_eq!(
                result.unwrap_err().raw_os_error(),
                Some(Errno::EINVAL as i32),
                "{id}"
            );
        }
    }
}
