//! The agent's wait between requests.
//!
//! As PID 1, the agent is the parent of every process whose parent ended: the daemons that
//! `/init` started before it, and the descendants of its workloads that outlive their parents.
//! While a workload runs, the execution loop collects their statuses with the workload's. Between
//! requests no workload runs, so the agent collects the status of every child that ends while it
//! waits for the broker's next record, and none stays a zombie until the next execution.

use std::time::Duration;

use crate::guest::sys::{self, ChildSignalWait, Errno, Pollable};

use super::cgroup::reap_orphans;
use super::session::Session;

/// Waits until `session`'s descriptor has input, an error, or a hangup, which the session's next
/// read takes, and as PID 1 collects the status of every child that ends meanwhile. Another
/// process, such as a test's, leaves its children to their own collectors.
pub(super) fn wait_for_request(session: &Session) {
    if sys::current_process().raw() == 1 {
        collect_until_readable(session);
    }
}

/// Collects the status of every child that has ended or that ends until `session`'s descriptor
/// polls with an event of its own.
///
/// `SIGCHLD` is blocked only while the agent waits here, so the processes that a request spawns
/// start with the agent's usual signal mask. A child that ended before the agent blocked it left
/// no signal pending, so the agent collects before each wait. If the signal cannot be blocked, the
/// agent collects once and leaves the wait to the session's read.
fn collect_until_readable(session: &Session) {
    let wait = ChildSignalWait::new();
    loop {
        reap_orphans();
        let Ok(wait) = &wait else {
            return;
        };
        match sys::poll(
            [
                Pollable::File(session.descriptor()),
                Pollable::ChildSignal(wait),
            ],
            None,
        ) {
            // Only a child ended: the agent consumes its signal and collects it.
            Ok([control, child]) if child.readable && !control.readable && !control.error => {
                wait.wait(Duration::ZERO);
            }
            Err(Errno::EINTR) => {}
            // Input, an error, or a hangup, which a poll for input reports as neither; or a
            // failed poll, after which the session's read waits as it did before.
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::process::Command;
    use std::time::Instant;

    use nix::sys::signal::{SigSet, SigmaskHow, Signal};

    use super::*;
    use crate::guest::sys::{FileDescriptor, ProcessId};

    /// Runs the test named `test` in a process of its own, as the variable `marker` tells it, and
    /// returns `true` in the parent once it succeeds. The process starts with `SIGCHLD` blocked,
    /// which each of its threads inherits, so that only the wait's signal descriptor takes the
    /// signal, as in the agent, which has one thread: a test thread that did not block it would
    /// take and ignore it.
    fn isolate_with_child_signal_blocked(marker: &str, test: &str) -> bool {
        if std::env::var_os(marker).is_some() {
            return false;
        }
        let previous = SigSet::from(Signal::SIGCHLD)
            .thread_swap_mask(SigmaskHow::SIG_BLOCK)
            .unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(marker, "1")
            .status();
        previous.thread_set_mask().unwrap();
        assert!(
            status.unwrap().success(),
            "the isolated test process failed"
        );
        true
    }

    #[test]
    fn children_that_end_while_the_agent_waits_are_collected_before_the_request() {
        // The wait collects every child of its process, so the test runs in a process of its own.
        if isolate_with_child_signal_blocked(
            "NVX_IDLE_COLLECTION_TEST_CHILD",
            "guest::managed_agent::idle::tests::children_that_end_while_the_agent_waits_are_collected_before_the_request",
        ) {
            return;
        }
        // One child ends before the wait starts, and one while the agent waits.
        let ended = ProcessId::from_child(&Command::new("/bin/true").spawn().unwrap());
        while !sys::has_exited(ended).unwrap() {
            std::thread::yield_now();
        }
        let ending = ProcessId::from_child(&Command::new("/bin/sleep").arg("0.2").spawn().unwrap());
        let (mut host, guest) = UnixStream::pair().unwrap();
        let session = Session::new(FileDescriptor::from_owned(guest.into()));
        // The request arrives only once the second child is gone, zombie and all, so the wait
        // must have collected it on its signal, before any input woke it.
        let request = std::thread::spawn(move || {
            let process = format!("/proc/{}", ending.raw());
            let deadline = Instant::now() + Duration::from_secs(20);
            while Path::new(&process).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let collected = !Path::new(&process).exists();
            host.write_all(b"x").unwrap();
            (collected, host)
        });
        // This thread waits with the agent's usual mask, which the wait restores.
        SigSet::from(Signal::SIGCHLD).thread_unblock().unwrap();
        collect_until_readable(&session);
        let (collected, host) = request.join().unwrap();
        assert!(
            collected,
            "the child that ended during the wait stayed a zombie"
        );
        assert_eq!(sys::wait(ended), None);
        assert_eq!(sys::wait(ending), None);
        assert!(!SigSet::thread_get_mask().unwrap().contains(Signal::SIGCHLD));
        drop(host);
        // A pipe without a writer polls as a hangup, which a poll for input reports as neither
        // input nor an error: it ends the wait too, for the session's read to report.
        let (reader, writer) = sys::create_pipe().unwrap();
        drop(writer);
        collect_until_readable(&Session::new(reader));
    }
}
