//! Tests of the supervisor's stop and reattach decisions, and of launches that fail before their
//! VMM runs. Real VMMs are launched, stopped, and reattached to in `tests/supervisor.rs`.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::time::{Duration, Instant};

use serde_json::json;

use super::launch::launch_reporting;
use super::reattach::reattach_with;
use super::vmm::{Stoppable, stop_process, terminate_process};
use super::*;
use crate::ErrorKind;
use crate::error::{Error, Result};
use crate::host::launch::MAX_TIMEOUT;
use crate::host::platform;
use crate::host::process::{BootId, Process, ProcessRef};
use crate::host::store::{
    ArtifactIdentity, FORMAT, IdentityPart, JOURNAL_NAME, LaunchId, RUNTIME_NAME, Recovery,
    RuntimeIdentity, SandboxStore,
};
use crate::protocol::CAPABILITY_LEN;

/// A launch capability.
const CAPABILITY: [u8; CAPABILITY_LEN] = [7; CAPABILITY_LEN];
/// The launch of the runtime records that [`record`] writes.
const RECORDED_LAUNCH: &str = "5d41402abc4b2a76b9719d911017c592";

/// Returns the identity `sha256:` and 64 copies of `digit`.
fn artifact(digit: char) -> ArtifactIdentity {
    ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap()
}

/// Returns the runtime identity of these tests' VMs.
fn runtime() -> RuntimeIdentity {
    RuntimeIdentity::new(artifact('a'), artifact('b'), artifact('c'))
}

/// Returns a store in a fresh temporary directory.
fn store() -> (tempfile::TempDir, SandboxStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = SandboxStore::create(directory.path().join("sandbox")).unwrap();
    (directory, store)
}

/// Returns the reference of this test process, which stands for a running VMM that reattach
/// holds but never signals.
fn this_process() -> ProcessRef {
    let pid = std::process::id();
    ProcessRef {
        pid,
        start_time: platform::process_start_time(pid).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    }
}

/// Writes the runtime record of a VM whose VMM is `process`, launched with `runtime`, as another
/// orchestrator would have committed it.
fn record(store: &SandboxStore, process: &ProcessRef, runtime: &RuntimeIdentity) {
    record_launch(store, RECORDED_LAUNCH, process, runtime);
}

/// Writes the runtime record of the launch `launch` of a VM whose VMM is `process`, launched with
/// `runtime`, as another orchestrator would have committed it.
fn record_launch(
    store: &SandboxStore,
    launch: &str,
    process: &ProcessRef,
    runtime: &RuntimeIdentity,
) {
    let record = json!({
        "format": FORMAT,
        "launch": launch,
        "process": process,
        "endpoint": "/var/lib/sandboxes/example/control.sock",
        "runtime": runtime,
    });
    fs::write(
        store.dir().join(RUNTIME_NAME),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

/// A VMM whose state drives a stop's decisions without a process: it exits once the graceful
/// step runs, if it heeds that step, or once it is killed, if killing ends it.
#[derive(Default)]
struct Scripted {
    /// Whether the VMM has exited.
    exited: Cell<bool>,
    /// Whether the graceful step ends the VMM, even one that fails.
    heeds_graceful: bool,
    /// Whether killing the VMM ends it.
    dies_when_killed: bool,
    /// Whether checks of the VMM's state fail.
    unreadable: bool,
    /// Whether killing the VMM fails.
    unkillable: bool,
    /// The operations of the stop, in order.
    operations: RefCell<Vec<&'static str>>,
}

impl Scripted {
    /// Records the operation `name`.
    fn note(&self, name: &'static str) {
        self.operations.borrow_mut().push(name);
    }

    /// Returns whether the VMM has exited, or fails if its state is unreadable.
    fn state(&self) -> io::Result<bool> {
        if self.unreadable {
            Err(io::Error::other("unreadable"))
        } else {
            Ok(self.exited.get())
        }
    }

    /// Stops the VMM with `forced`, and with a graceful step that fails with `graceful_error` if
    /// it is given, unless it had exited, as [`Vmm::stop_unless_exited`] does.
    fn stop_unless_exited(
        &self,
        forced: Option<Duration>,
        graceful_error: Option<&'static str>,
    ) -> Result<Option<Stopped<&'static str>>> {
        let deadline = Instant::now() + Duration::from_secs(60);
        stop_process(self, 7, deadline, forced, || {
            self.note("graceful");
            if self.heeds_graceful {
                self.exited.set(true);
            }
            graceful_error.map_or(Ok(()), Err)
        })
    }

    /// Stops the VMM, which runs as the stop begins, as
    /// [`stop_unless_exited`](Self::stop_unless_exited) does.
    fn stop(
        &self,
        forced: Option<Duration>,
        graceful_error: Option<&'static str>,
    ) -> Result<Stopped<&'static str>> {
        let stopped = self.stop_unless_exited(forced, graceful_error)?;
        Ok(stopped.expect("the VMM had exited before the stop began"))
    }

    /// Returns the operations of the stop.
    fn operations(&self) -> Vec<&'static str> {
        self.operations.borrow().clone()
    }

    /// Terminates the VMM, as [`Vmm::terminate`] does.
    fn terminate(&self) -> Result<StopOutcome> {
        terminate_process(self, 7, Duration::from_secs(10))
    }
}

impl Stoppable for Scripted {
    fn has_exited(&self) -> io::Result<bool> {
        self.note("check");
        self.state()
    }

    fn wait(&self, _deadline: Instant) -> io::Result<bool> {
        self.note("wait");
        self.state()
    }

    fn kill(&self) -> io::Result<()> {
        self.note("kill");
        if self.unkillable {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        if self.dies_when_killed {
            self.exited.set(true);
        }
        Ok(())
    }
}

#[test]
fn a_vmm_that_heeds_the_graceful_step_stops_gracefully() {
    let vmm = Scripted {
        heeds_graceful: true,
        dies_when_killed: true,
        ..Scripted::default()
    };
    let stopped = vmm.stop(Some(Duration::from_secs(10)), None).unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
    assert!(stopped.graceful_error.is_none());
    // The forced step that the caller requested is not taken.
    assert_eq!(vmm.operations(), ["check", "graceful", "wait"]);
}

#[test]
fn without_the_forced_step_a_stop_times_out_and_never_signals() {
    let vmm = Scripted {
        dies_when_killed: true,
        ..Scripted::default()
    };
    let stopped = vmm.stop(None, None).unwrap();
    assert_eq!(stopped.outcome, StopOutcome::TimedOut);
    assert_eq!(vmm.operations(), ["check", "graceful", "wait"]);
    assert!(!vmm.exited.get());
}

#[test]
fn a_requested_forced_step_that_ends_the_vmm_reports_forced() {
    let vmm = Scripted {
        dies_when_killed: true,
        ..Scripted::default()
    };
    let stopped = vmm.stop(Some(Duration::from_secs(10)), None).unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Forced);
    assert_eq!(
        vmm.operations(),
        ["check", "graceful", "wait", "kill", "wait"]
    );

    // A VMM that outlives the forced step's bound still runs.
    let survivor = Scripted::default();
    let stopped = survivor.stop(Some(Duration::from_secs(10)), None).unwrap();
    assert_eq!(stopped.outcome, StopOutcome::TimedOut);
    assert_eq!(
        survivor.operations(),
        ["check", "graceful", "wait", "kill", "wait"]
    );
}

#[test]
fn a_failed_graceful_step_is_kept_and_not_waited_out() {
    // The step may not have reached the guest, so only one check follows it.
    let vmm = Scripted::default();
    let stopped = vmm.stop(None, Some("unreachable")).unwrap();
    assert_eq!(stopped.outcome, StopOutcome::TimedOut);
    assert_eq!(stopped.graceful_error, Some("unreachable"));
    assert_eq!(vmm.operations(), ["check", "graceful", "check"]);

    let forced = Scripted {
        dies_when_killed: true,
        ..Scripted::default()
    };
    let stopped = forced
        .stop(Some(Duration::from_secs(10)), Some("unreachable"))
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Forced);
    assert_eq!(stopped.graceful_error, Some("unreachable"));
    assert_eq!(
        forced.operations(),
        ["check", "graceful", "check", "kill", "wait"]
    );

    // A VMM that exited anyway, as one that closed the caller's transport as it shut down,
    // stopped without the forced step.
    let exited = Scripted {
        heeds_graceful: true,
        dies_when_killed: true,
        ..Scripted::default()
    };
    let stopped = exited
        .stop(Some(Duration::from_secs(10)), Some("closed"))
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
    assert_eq!(stopped.graceful_error, Some("closed"));
    assert_eq!(exited.operations(), ["check", "graceful", "check"]);
}

#[test]
fn a_vmm_that_exited_already_skips_the_graceful_step() {
    let vmm = Scripted::default();
    vmm.exited.set(true);
    let stopped = vmm
        .stop_unless_exited(Some(Duration::from_secs(10)), None)
        .unwrap();
    assert!(stopped.is_none(), "{stopped:?}");
    assert_eq!(vmm.operations(), ["check"]);
}

#[test]
fn undetermined_states_and_failed_terminations_are_errors() {
    let unreadable = Scripted {
        unreadable: true,
        ..Scripted::default()
    };
    let error = unreadable.stop(None, None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error.message(),
        "cannot determine whether VMM process 7 exited"
    );

    // A host that cannot hold the VMM cannot take the forced step.
    let unkillable = Scripted {
        unkillable: true,
        ..Scripted::default()
    };
    let error = unkillable
        .stop(Some(Duration::from_secs(10)), None)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(error.message(), "cannot terminate VMM process 7");
}

#[test]
fn a_termination_takes_the_forced_step_alone() {
    // No graceful step runs, even for a VMM that would heed one.
    let vmm = Scripted {
        heeds_graceful: true,
        dies_when_killed: true,
        ..Scripted::default()
    };
    assert_eq!(vmm.terminate().unwrap(), StopOutcome::Forced);
    assert_eq!(vmm.operations(), ["check", "kill", "wait"]);

    // A VMM that outlives the bound still runs.
    let survivor = Scripted::default();
    assert_eq!(survivor.terminate().unwrap(), StopOutcome::TimedOut);
    assert_eq!(survivor.operations(), ["check", "kill", "wait"]);

    // A VMM that exited already is not signaled.
    let exited = Scripted::default();
    exited.exited.set(true);
    assert_eq!(exited.terminate().unwrap(), StopOutcome::Graceful);
    assert_eq!(exited.operations(), ["check"]);
}

#[test]
fn terminations_whose_effect_cannot_be_determined_are_errors() {
    // A VMM whose state cannot be read is never signaled.
    let unreadable = Scripted {
        unreadable: true,
        dies_when_killed: true,
        ..Scripted::default()
    };
    let error = unreadable.terminate().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error.message(),
        "cannot determine whether VMM process 7 exited"
    );
    assert_eq!(unreadable.operations(), ["check"]);

    let unkillable = Scripted {
        unkillable: true,
        ..Scripted::default()
    };
    let error = unkillable.terminate().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(error.message(), "cannot terminate VMM process 7");
    assert_eq!(unkillable.operations(), ["check", "kill"]);
}

#[test]
fn a_sandbox_without_a_record_has_no_vm() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let Reattached::Exited { record, recovery } = reattach(&mut locked, &runtime()).unwrap() else {
        panic!("a VM was found in an empty store");
    };
    assert!(record.is_none());
    assert_eq!(recovery, Recovery::Clean);
}

#[test]
fn a_live_vmm_of_the_same_runtime_is_held() {
    let (_directory, store) = store();
    let process = this_process();
    record(&store, &process, &runtime());
    let mut locked = store.lock(None).unwrap();
    let Reattached::Live(vmm) = reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the recorded VMM was not reattached");
    };
    assert_eq!(vmm.record().process(), &process);
    assert_eq!(vmm.process().reference(), &process);
    assert_eq!(vmm.endpoint(), "/var/lib/sandboxes/example/control.sock");
    assert!(!vmm.has_exited().unwrap());
    // The record stays.
    assert_eq!(store.runtime().unwrap().as_ref(), Some(vmm.record()));
}

#[test]
fn a_live_vmm_of_another_runtime_is_mismatched() {
    let (_directory, store) = store();
    let process = this_process();
    record(&store, &process, &runtime());
    let mut current = runtime();
    current.initramfs = artifact('d');
    let mut locked = store.lock(None).unwrap();
    let Reattached::Mismatched { vmm, differences } = reattach(&mut locked, &current).unwrap()
    else {
        panic!("a VMM of another runtime was not reported as mismatched");
    };
    assert_eq!(differences, [IdentityPart::Initramfs]);
    assert_eq!(vmm.process().reference(), &process);
    assert!(store.runtime().unwrap().is_some());
}

#[test]
fn records_of_exited_vmms_are_cleared() {
    let (_directory, store) = store();
    let live = this_process();
    let mut earlier_boot = *live.boot.as_bytes();
    earlier_boot[0] ^= 1;
    for exited in [
        // A record of an earlier boot names a VMM that has exited.
        ProcessRef {
            boot: BootId::from_bytes(earlier_boot),
            ..live.clone()
        },
        // The recorded ID now names another process: this one.
        ProcessRef {
            start_time: live.start_time + 1,
            ..live.clone()
        },
    ] {
        record(&store, &exited, &runtime());
        let mut locked = store.lock(None).unwrap();
        let Reattached::Exited { record, recovery } = reattach(&mut locked, &runtime()).unwrap()
        else {
            panic!("{exited:?} names a live VMM");
        };
        assert_eq!(record.unwrap().process(), &exited);
        assert_eq!(recovery, Recovery::Clean);
        assert!(store.runtime().unwrap().is_none());
    }
}

#[test]
fn a_vmm_that_cannot_be_examined_is_indeterminate_and_keeps_its_record() {
    let (_directory, store) = store();
    record(&store, &this_process(), &runtime());
    let mut locked = store.lock(None).unwrap();
    let hidden = |_: &ProcessRef| -> io::Result<Option<Process>> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "hidden"))
    };
    let Reattached::Indeterminate(reason) =
        reattach_with(&mut locked, &runtime(), hidden, |_| Ok(())).unwrap()
    else {
        panic!("a VMM that cannot be examined was classified");
    };
    assert!(reason.ends_with("cannot be examined: hidden"), "{reason}");
    assert!(store.runtime().unwrap().is_some());
}

#[test]
fn an_interrupted_launch_that_cannot_be_ended_is_indeterminate() {
    let (_directory, store) = store();
    // The journal of a launch whose log is gone, so that its claim cannot be checked.
    let journal = json!({
        "format": FORMAT,
        "launch": "5d41402abc4b2a76b9719d911017c592",
        "endpoint": "/var/lib/sandboxes/example/control.sock",
        "runtime": runtime(),
        "log": {"identity": {"device": 2049, "index": 393_219}},
    });
    fs::write(
        store.dir().join(JOURNAL_NAME),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    let mut locked = store.lock(None).unwrap();
    let Reattached::Indeterminate(reason) = reattach(&mut locked, &runtime()).unwrap() else {
        panic!("an interrupted launch was ended without proof");
    };
    assert!(
        reason.starts_with("an interrupted launch of the sandbox cannot be ended yet: "),
        "{reason}"
    );
    assert!(store.journal().unwrap().is_some());
}

#[test]
fn reattach_refuses_an_invalid_runtime() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let mut invalid = runtime();
    invalid.control_contract = "with space".to_owned();
    let error = reattach(&mut locked, &invalid).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
}

#[test]
fn stops_check_their_store_and_forced_bound_before_anything_else() {
    let (_directory, store) = store();
    let (_other_directory, other) = self::store();
    record(&store, &this_process(), &runtime());
    let mut locked = store.lock(None).unwrap();
    let Reattached::Live(vmm) = reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the recorded VMM was not reattached");
    };
    let never = |_: &Vmm, _: Instant| -> std::result::Result<(), ()> {
        panic!("the graceful step of an invalid stop ran")
    };
    // This test process stands for the VMM, and none of these stops reaches it.
    let error = vmm
        .stop(&mut other.lock(None).unwrap(), Instant::now(), None, never)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other);
    let error = vmm
        .stop_unless_exited(&mut other.lock(None).unwrap(), Instant::now(), None, never)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other);
    for bound in [Duration::ZERO, MAX_TIMEOUT + Duration::from_secs(1)] {
        let error = vmm
            .stop(&mut locked, Instant::now(), Some(bound), never)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{bound:?}");
        let error = vmm
            .stop_unless_exited(&mut locked, Instant::now(), Some(bound), never)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{bound:?}");
    }
    assert!(store.runtime().unwrap().is_some());
}

#[test]
fn a_launch_whose_command_fails_ends_before_anything_runs() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let error = launch(
        &mut locked,
        &LaunchId::generate().unwrap(),
        &runtime(),
        &CAPABILITY,
        |endpoint| {
            assert!(!endpoint.is_empty());
            Err(Error::unsupported("no command"))
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert_eq!(error.message(), "no command");
    // The launch ended: its journal and capability are gone, and its fresh log stays.
    assert!(store.journal().unwrap().is_none());
    assert!(store.read_capability().is_err());
    assert!(store.log_path().exists());
    assert_eq!(locked.recover().unwrap(), Recovery::Clean);
}

#[test]
fn a_launch_whose_caller_cannot_record_its_failure_keeps_its_journal_for_recovery() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let failure = launch_reporting(
        &mut locked,
        &LaunchId::generate().unwrap(),
        &runtime(),
        &CAPABILITY,
        |_| Err(Error::unsupported("no command")),
        |error, spawned| {
            assert_eq!((error.message(), spawned), ("no command", false));
            Err(Error::io("the test cannot record the failure"))
        },
    )
    .unwrap_err();
    let reason = "the test cannot record the failure";
    assert_eq!(failure.error.kind(), ErrorKind::Unsupported);
    assert_eq!(
        failure.error.message(),
        format!("no command; the launch could not be ended, and remains for recovery: {reason}")
    );
    // The journal remains, as a crash leaves it, until a recovery ends the launch, of which
    // nothing runs.
    assert!(store.journal().unwrap().is_some());
    assert_eq!(locked.recover().unwrap(), Recovery::Exited);
    assert!(store.journal().unwrap().is_none());
}

#[test]
fn a_reattach_whose_caller_cannot_record_an_exit_keeps_the_record_until_one_can() {
    let (_directory, store) = store();
    record(&store, &this_process(), &runtime());
    let mut locked = store.lock(None).unwrap();
    // The recorded VMM exited, but the caller cannot record what ended it.
    let exited = |_: &ProcessRef| -> io::Result<Option<Process>> { Ok(None) };
    let error = reattach_with(&mut locked, &runtime(), exited, |_| {
        Err(Error::io("the test cannot record the exit"))
    })
    .unwrap_err();
    assert_eq!(error.message(), "the test cannot record the exit");
    // The record remains for a later reattach.
    assert!(locked.runtime().unwrap().is_some());
    let Reattached::Exited { record, recovery } =
        reattach_with(&mut locked, &runtime(), exited, |_| Ok(())).unwrap()
    else {
        panic!("a VMM that exited was not reported as exited");
    };
    assert_eq!(record.unwrap().launch().as_str(), RECORDED_LAUNCH);
    assert_eq!(recovery, Recovery::Clean);
    assert!(locked.runtime().unwrap().is_none());
}

#[test]
fn a_launch_whose_vmm_cannot_start_ends() {
    let (directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let missing = directory.path().join("missing-openvmm");
    let error = launch(
        &mut locked,
        &LaunchId::generate().unwrap(),
        &runtime(),
        &CAPABILITY,
        |_| Ok(VmmCommand::new(&missing, Vec::new(), directory.path())),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error.message(),
        format!("cannot launch {}", missing.display())
    );
    assert!(store.journal().unwrap().is_none());
    // The next launch begins at once.
    let next = launch(
        &mut locked,
        &LaunchId::generate().unwrap(),
        &runtime(),
        &CAPABILITY,
        |_| Err(Error::other("the next launch began")),
    );
    assert_eq!(next.unwrap_err().message(), "the next launch began");
}

#[test]
fn a_forced_teardown_that_fails_keeps_the_record_until_one_ends_the_vmm() {
    let (_directory, store) = store();
    record(&store, &this_process(), &runtime());
    let mut locked = store.lock(None).unwrap();
    let Reattached::Live(vmm) = reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the recorded VMM was not reattached");
    };
    // This test process stands for the VMM, which these teardowns reach only through stand-ins.
    let bound = Duration::from_secs(10);
    // A teardown that cannot terminate the VMM fails, and keeps its record.
    let unkillable = Scripted {
        unkillable: true,
        ..Scripted::default()
    };
    let error = vmm
        .terminate_with(&mut locked, bound, &unkillable)
        .unwrap_err();
    assert_eq!(
        error.message(),
        format!("cannot terminate VMM process {}", std::process::id())
    );
    assert_eq!(locked.runtime().unwrap().as_ref(), Some(vmm.record()));
    // One that does not end the VMM within its bound keeps it too.
    let survivor = Scripted::default();
    assert_eq!(
        vmm.terminate_with(&mut locked, bound, &survivor).unwrap(),
        StopOutcome::TimedOut
    );
    assert_eq!(locked.runtime().unwrap().as_ref(), Some(vmm.record()));
    // One that ends it clears the record.
    let dying = Scripted {
        dies_when_killed: true,
        ..Scripted::default()
    };
    assert_eq!(
        vmm.terminate_with(&mut locked, bound, &dying).unwrap(),
        StopOutcome::Forced
    );
    assert!(locked.runtime().unwrap().is_none());
}

#[test]
fn only_a_stop_unless_exited_keeps_the_record_of_a_vmm_that_had_exited() {
    let (_directory, store) = store();
    record(&store, &this_process(), &runtime());
    let mut locked = store.lock(None).unwrap();
    let Reattached::Live(vmm) = reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the recorded VMM was not reattached");
    };
    // This test process stands for the VMM, which these stops reach only through a stand-in,
    // which exited once its holder found it running, before the stops began.
    let deadline = Instant::now() + Duration::from_secs(60);
    let forced = Some(Duration::from_secs(10));
    let never = |_: &Vmm, _: Instant| -> std::result::Result<(), ()> {
        panic!("a stop ran the graceful step of a VMM that had exited")
    };
    let exited = Scripted::default();
    exited.exited.set(true);
    // The stop takes no step, and keeps the record, which tells a reattach that the VM exited
    // unheld.
    let stopped = vmm
        .stop_unless_exited_with(&mut locked, deadline, forced, never, &exited)
        .unwrap();
    assert!(stopped.is_none(), "{stopped:?}");
    assert_eq!(exited.operations(), ["check"]);
    assert_eq!(locked.runtime().unwrap().as_ref(), Some(vmm.record()));
    // A stop that ends such a VM as one that it stopped clears the record.
    let stopped = vmm
        .stop_with(&mut locked, deadline, forced, never, &exited)
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
    assert!(stopped.graceful_error.is_none());
    assert_eq!(exited.operations(), ["check", "check"]);
    assert!(locked.runtime().unwrap().is_none());
}

#[test]
fn a_stop_unless_exited_stops_a_running_vmm_as_a_stop_does() {
    let (_directory, store) = store();
    record(&store, &this_process(), &runtime());
    let mut locked = store.lock(None).unwrap();
    let Reattached::Live(vmm) = reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the recorded VMM was not reattached");
    };
    // This test process stands for the VMM, which these stops reach only through stand-ins.
    let deadline = Instant::now() + Duration::from_secs(60);
    // One that outlives the stop keeps its record.
    let survivor = Scripted::default();
    let stopped = vmm
        .stop_unless_exited_with(
            &mut locked,
            deadline,
            None,
            |_, _| Ok::<_, ()>(()),
            &survivor,
        )
        .unwrap()
        .expect("the stop of a VMM that runs");
    assert_eq!(stopped.outcome, StopOutcome::TimedOut);
    assert_eq!(survivor.operations(), ["check", "wait"]);
    assert_eq!(locked.runtime().unwrap().as_ref(), Some(vmm.record()));
    // One that heeds the graceful step exits through the stop, which clears its record.
    let heeding = Scripted::default();
    let shut_down = |_: &Vmm, _: Instant| {
        heeding.exited.set(true);
        Ok::<_, ()>(())
    };
    let stopped = vmm
        .stop_unless_exited_with(&mut locked, deadline, None, shut_down, &heeding)
        .unwrap()
        .expect("the stop of a VMM that runs");
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
    assert_eq!(heeding.operations(), ["check", "wait"]);
    assert!(locked.runtime().unwrap().is_none());
}

#[test]
fn a_stop_keeps_the_record_of_another_vmm_under_the_same_launch_identity() {
    let (_directory, store) = store();
    let process = this_process();
    record(&store, &process, &runtime());
    let mut locked = store.lock(None).unwrap();
    let Reattached::Live(vmm) = reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the recorded VMM was not reattached");
    };
    // Another VMM's record replaced it, under the launch identity that its caller used again.
    let other = ProcessRef {
        start_time: process.start_time + 1,
        ..process.clone()
    };
    record(&store, &other, &runtime());
    let exited = Scripted::default();
    exited.exited.set(true);
    assert_eq!(
        vmm.terminate_with(&mut locked, Duration::from_secs(10), &exited)
            .unwrap(),
        StopOutcome::Graceful
    );
    let kept = store.runtime().unwrap().unwrap();
    assert_eq!(kept.launch().as_str(), RECORDED_LAUNCH);
    assert_eq!(kept.process(), &other);
}
