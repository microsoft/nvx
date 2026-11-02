//! Checks of signal descriptors in this test's thread.

use std::time::Instant;

use super::*;

/// Makes the kernel reap the calling process's children on its own, which the `SIG_IGN`
/// disposition of `SIGCHLD` does when `ignored`, and `SA_NOCLDWAIT` with the default action
/// does otherwise. [`reset_child_signal`] undoes either.
pub(crate) fn reap_children_automatically(ignored: bool) -> Result<(), Errno> {
    use nix::sys::signal::{SaFlags, SigAction};

    let action = if ignored {
        SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty())
    } else {
        SigAction::new(SigHandler::SigDfl, SaFlags::SA_NOCLDWAIT, SigSet::empty())
    };
    // SAFETY: neither action installs a Rust callback.
    unsafe { nix::sys::signal::sigaction(Signal::SIGCHLD, &action) }.map(drop)
}

/// Returns whether the calling process has a child, which it has not reaped, without reaping
/// it, whatever its disposition of `SIGCHLD`.
pub(crate) fn has_child() -> bool {
    use nix::sys::wait::{Id, WaitPidFlag};

    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
    nix::sys::wait::waitid(Id::All, flags) != Err(Errno::ECHILD)
}

/// Makes a `SIGPIPE` end the process, as the default disposition does, for a test that runs in a
/// process of its own.
pub(crate) fn default_sigpipe() {
    // SAFETY: `SIG_DFL` installs no Rust callback.
    unsafe { nix::sys::signal::signal(Signal::SIGPIPE, SigHandler::SigDfl) }
        .expect("SIGPIPE takes its default disposition");
}

#[test]
fn a_blocked_signal_reaches_the_descriptor() {
    let signals = SignalDescriptor::new(&[Signal::SIGUSR1]).unwrap();
    assert_eq!(signals.read(), None);
    // A signal to the whole test process could reach another test thread, which does not
    // block it, so this test raises it in its own thread.
    nix::sys::signal::raise(Signal::SIGUSR1).unwrap();
    assert_eq!(
        signals.read(),
        Some(SignalEvent {
            signal: Some(Signal::SIGUSR1),
            queued: false,
            value: 0,
        })
    );
    assert_eq!(signals.read(), None);
}

/// Takes a pending `signal` of this thread or process, without waiting, and returns whether
/// one was pending.
fn take_pending(signal: Signal) -> bool {
    let set = SigSet::from(signal);
    let immediately = nix::libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the set and the zero timeout are valid, and a null pointer asks for no details.
    unsafe { nix::libc::sigtimedwait(set.as_ref(), std::ptr::null_mut(), &immediately) >= 0 }
}

#[test]
fn a_write_without_a_reader_fails_without_sigpipe() {
    // The default disposition, under which a `SIGPIPE` would end the test process, applies
    // to the whole process, so the test runs in a process of its own.
    if crate::guest::managed_agent::tests::isolate_process_test(
        "NVX_SIGPIPE_WRITE_TEST_CHILD",
        "guest::sys::signal::tests::a_write_without_a_reader_fails_without_sigpipe",
    ) {
        return;
    }
    default_sigpipe();
    let before = SigSet::thread_get_mask().unwrap();
    assert!(!before.contains(Signal::SIGPIPE));
    let (reader, writer) = crate::guest::sys::create_pipe().unwrap();
    assert_eq!(write_without_sigpipe(&writer, b"kept"), Ok(4));
    drop(reader);
    assert_eq!(write_without_sigpipe(&writer, b"lost"), Err(Errno::EPIPE));
    assert_eq!(write_without_sigpipe(&writer, b"lost"), Err(Errno::EPIPE));
    assert_eq!(SigSet::thread_get_mask().unwrap(), before);
    assert!(!take_pending(Signal::SIGPIPE));
}

#[test]
fn a_thread_that_blocks_sigpipe_keeps_only_a_signal_that_was_pending() {
    // The signals are the writing thread's own, so other test threads never see them.
    let pipe = SigSet::from(Signal::SIGPIPE);
    let previous = pipe.thread_swap_mask(SigmaskHow::SIG_BLOCK).unwrap();
    let (reader, writer) = crate::guest::sys::create_pipe().unwrap();
    drop(reader);
    // The write takes back its own signal, which the thread would receive once it unblocks
    // the signal.
    assert_eq!(write_without_sigpipe(&writer, b"lost"), Err(Errno::EPIPE));
    assert!(SigSet::thread_get_mask().unwrap().contains(Signal::SIGPIPE));
    assert!(!take_pending(Signal::SIGPIPE));
    // A signal that was pending before the write stays.
    nix::sys::signal::raise(Signal::SIGPIPE).unwrap();
    assert_eq!(write_without_sigpipe(&writer, b"lost"), Err(Errno::EPIPE));
    assert!(take_pending(Signal::SIGPIPE));
    assert!(!take_pending(Signal::SIGPIPE));
    previous.thread_set_mask().unwrap();
}

#[test]
fn a_write_takes_back_its_own_sigpipe_beside_one_pending_for_the_process() {
    // A signal to the whole process stays pending only while every thread of the process
    // blocks it, so a forked child, whose one thread blocks it, checks the write. Its
    // diagnostic is its exit status.
    // SAFETY: the child makes only async-signal-safe calls, which allocate nothing, until
    // `_exit`, as a child of a multithreaded process may.
    match unsafe { nix::unistd::fork() }.expect("fork") {
        nix::unistd::ForkResult::Child => {
            let check = || -> Result<(), u8> {
                SigSet::from(Signal::SIGPIPE)
                    .thread_block()
                    .map_err(|_| 1)?;
                nix::sys::signal::kill(nix::unistd::getpid(), Signal::SIGPIPE).map_err(|_| 2)?;
                let (reader, writer) = crate::guest::sys::create_pipe().map_err(|_| 3)?;
                drop(reader);
                if write_without_sigpipe(&writer, b"lost") != Err(Errno::EPIPE) {
                    return Err(4);
                }
                // The process's signal stays, and the write's is gone.
                if !take_pending(Signal::SIGPIPE) {
                    return Err(5);
                }
                if take_pending(Signal::SIGPIPE) {
                    return Err(6);
                }
                Ok(())
            };
            crate::guest::sys::exit_immediately(check().err().unwrap_or(0))
        }
        nix::unistd::ForkResult::Parent { child } => {
            let id = child.as_raw().unsigned_abs();
            let status = crate::guest::process::Child::new(id)
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(status.code(), Some(0), "{status:?}");
        }
    }
}

#[test]
fn a_child_signal_wait_times_out_and_restores_the_mask() {
    // A child that another test thread forks would end the wait early with its `SIGCHLD`.
    if crate::guest::managed_agent::tests::isolate_process_test(
        "NVX_CHILD_SIGNAL_WAIT_TEST_CHILD",
        "guest::sys::signal::tests::a_child_signal_wait_times_out_and_restores_the_mask",
    ) {
        return;
    }
    let before = SigSet::thread_get_mask().unwrap();
    let wait = ChildSignalWait::new().unwrap();
    assert!(SigSet::thread_get_mask().unwrap().contains(Signal::SIGCHLD));
    let start = Instant::now();
    wait.wait(Duration::from_millis(5));
    assert!(start.elapsed() >= Duration::from_millis(5));
    wait.restore().unwrap();
    assert_eq!(
        SigSet::thread_get_mask().unwrap().contains(Signal::SIGCHLD),
        before.contains(Signal::SIGCHLD)
    );
}
