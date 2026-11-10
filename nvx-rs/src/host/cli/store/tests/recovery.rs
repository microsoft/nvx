use super::launch::prepared;
use super::*;
use crate::host::cli::store::files;
use crate::host::config::ConsoleAttachment;

#[test]
fn the_next_lock_cleans_a_pre_spawn_journal_only_after_its_claim_is_released() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    drop(locked.begin_launch(&prepared, &runtime).unwrap());
    drop(locked);
    let journal = fs::read(state.path(StateFile::Journal)).unwrap();
    let capability = fs::read(state.path(StateFile::Capability)).unwrap();
    let started = Instant::now();
    assert!(state.lock(Deadline::after(0.05)).is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(fs::read(state.path(StateFile::Journal)).unwrap(), journal);
    assert_eq!(
        fs::read(state.path(StateFile::Capability)).unwrap(),
        capability
    );
    drop(prepared);
    let locked = state.lock(Deadline::after(10.0)).unwrap();
    assert!(locked.journal().unwrap().is_none());
    assert!(locked.capability().unwrap().is_none());
    assert!(state.path(StateFile::Log).exists());
}

#[test]
fn a_transient_inherited_log_descriptor_is_rechecked_before_refusing_recovery() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    drop(locked.begin_launch(&prepared, &runtime).unwrap());
    let inherited = prepared.log().try_clone().unwrap();
    drop(prepared);
    drop(locked);
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(80));
        drop(inherited);
    });
    let locked = state.lock(Deadline::after(10.0)).unwrap();
    release.join().unwrap();
    assert!(locked.journal().unwrap().is_none());
    assert!(locked.capability().unwrap().is_none());
}

#[test]
fn an_interrupted_cleanup_with_no_capability_can_finish_idempotently() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    drop(locked.begin_launch(&prepared, &runtime).unwrap());
    files::remove(&locked, StateFile::Capability).unwrap();
    drop(prepared);
    locked
        .recover(Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(locked.journal().unwrap().is_none());
    locked.recover(Instant::now()).unwrap();
}

#[test]
fn replaced_logs_and_foreign_journal_paths_keep_all_evidence() {
    for foreign in [false, true] {
        let (_directory, state) = state();
        let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
        let mut locked = state.lock(Deadline::after(10.0)).unwrap();
        drop(locked.begin_launch(&prepared, &runtime).unwrap());
        drop(prepared);
        if foreign {
            let mut journal = locked.journal().unwrap().unwrap();
            journal.vm_dir = state.dir().join("another-vm");
            locked.write_record(StateFile::Journal, &journal).unwrap();
        } else {
            fs::rename(state.path(StateFile::Log), state.dir().join("old-log")).unwrap();
            fs::write(state.path(StateFile::Log), b"another log").unwrap();
        }
        let journal = fs::read(state.path(StateFile::Journal)).unwrap();
        let capability = fs::read(state.path(StateFile::Capability)).unwrap();
        assert!(locked.recover(Instant::now()).is_err());
        assert_eq!(fs::read(state.path(StateFile::Journal)).unwrap(), journal);
        assert_eq!(
            fs::read(state.path(StateFile::Capability)).unwrap(),
            capability
        );
        drop(locked);
        let (_, message) = error(state.lock(Deadline::after(0.0)).unwrap_err());
        let cause = if foreign {
            "another sandbox directory"
        } else {
            "is not the claimed log"
        };
        assert!(message.contains(cause), "{message}");
    }
}

#[cfg(any(target_os = "linux", windows))]
mod process {
    #[cfg(target_os = "linux")]
    use std::process::{Child, Command, Stdio};

    use super::*;
    use crate::host::process::{BootId, Process, ProcessRef};
    use crate::host::spawn::{self, VmmCommand};
    use crate::host::vm::{Prepared, SavedVm, VmDir};

    const HELPER: &str = "host::cli::store::tests::recovery::process::launch_helper";

    struct Running(Option<spawn::Launched>);

    impl Running {
        fn process(&self) -> Process {
            self.0.as_ref().unwrap().process().unwrap()
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(child) = self.0.take() {
                child.kill_and_confirm_exit().unwrap();
            }
        }
    }

    #[cfg(target_os = "linux")]
    struct Other(Child);

    #[cfg(target_os = "linux")]
    impl Drop for Other {
        fn drop(&mut self) {
            if self.0.try_wait().unwrap().is_none() {
                self.0.kill().unwrap();
            }
            self.0.wait().unwrap();
        }
    }

    fn running(state: &StateDir, prepared: &Prepared) -> Running {
        fs::write(
            state.dir().join("recovery-endpoint"),
            &prepared.outputs().endpoint,
        )
        .unwrap();
        let command = VmmCommand::new(
            std::env::current_exe().unwrap(),
            ["--exact", HELPER, "--ignored", "--nocapture"]
                .map(Into::into)
                .to_vec(),
            state.dir(),
        );
        let child = Running(Some(
            spawn::spawn_vm(
                &command,
                prepared.capability().as_bytes(),
                prepared.log().try_clone().unwrap(),
            )
            .unwrap(),
        ));
        assert!(released(|| state.dir().join("recovery-ready").exists()));
        child
    }

    #[test]
    fn an_unrecorded_vm_requires_both_endpoint_and_log_proofs_before_it_is_ended() {
        let (_directory, state) = state();
        let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
        let mut locked = state.lock(Deadline::after(10.0)).unwrap();
        drop(locked.begin_launch(&prepared, &runtime).unwrap());
        let child = running(&state, &prepared);
        let process = child.process();
        drop(prepared);
        drop(locked);
        assert!(!process.has_exited().unwrap());
        let locked = state.lock(Deadline::after(10.0)).unwrap();
        assert!(process.wait(Instant::now()).unwrap().is_some());
        assert!(locked.journal().unwrap().is_none());
        assert!(locked.capability().unwrap().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_endpoint_owner_without_the_log_is_not_signaled_and_its_journal_stays() {
        let (_directory, state) = state();
        let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
        let mut locked = state.lock(Deadline::after(10.0)).unwrap();
        drop(locked.begin_launch(&prepared, &runtime).unwrap());
        fs::write(
            state.dir().join("recovery-endpoint"),
            &prepared.outputs().endpoint,
        )
        .unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", HELPER, "--ignored", "--nocapture"])
            .current_dir(state.dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut other = Other(crate::host::tests::spawn_locked(&mut command).unwrap());
        assert!(released(|| state.dir().join("recovery-ready").exists()));
        let journal = fs::read(state.path(StateFile::Journal)).unwrap();
        let capability = fs::read(state.path(StateFile::Capability)).unwrap();
        assert!(
            locked
                .recover(Instant::now() + Duration::from_secs(2))
                .is_err()
        );
        assert!(other.0.try_wait().unwrap().is_none());
        assert_eq!(fs::read(state.path(StateFile::Journal)).unwrap(), journal);
        assert_eq!(
            fs::read(state.path(StateFile::Capability)).unwrap(),
            capability
        );
    }

    #[test]
    fn a_recorded_process_is_ended_only_after_a_matching_identity_and_confirmed_exit() {
        let (_directory, state) = state();
        let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
        let mut locked = state.lock(Deadline::after(10.0)).unwrap();
        let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
        let child = running(&state, &prepared);
        let process = child.process();
        launch.record_process(process.reference()).unwrap();
        drop(launch);
        drop(prepared);
        assert!(locked.recover(Instant::now()).is_err());
        assert!(!process.has_exited().unwrap());
        assert!(locked.journal().unwrap().is_some());
        assert!(locked.capability().unwrap().is_some());
        locked
            .recover(Instant::now() + Duration::from_secs(10))
            .unwrap();
        assert!(process.wait(Instant::now()).unwrap().is_some());
        assert!(locked.journal().unwrap().is_none());
        assert!(locked.capability().unwrap().is_none());
    }

    #[test]
    fn incorrect_recorded_ids_and_start_times_never_signal_a_live_process_or_drop_its_claim() {
        for mismatch in 0..3 {
            let (_directory, state) = state();
            let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
            let mut locked = state.lock(Deadline::after(10.0)).unwrap();
            let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
            let child = running(&state, &prepared);
            let process = child.process();
            let mut reference = process.reference().clone();
            match mismatch {
                0 => reference.pid = 0,
                1 => {
                    reference.pid = std::process::id();
                    reference.start_time = platform::process_start_time(reference.pid)
                        .unwrap()
                        .unwrap()
                        + 1;
                }
                2 => reference.start_time += 1,
                _ => unreachable!(),
            }
            launch.record_process(&reference).unwrap();
            drop(launch);
            drop(prepared);
            let journal = fs::read(state.path(StateFile::Journal)).unwrap();
            let capability = fs::read(state.path(StateFile::Capability)).unwrap();
            assert!(
                locked
                    .recover(Instant::now() + Duration::from_secs(2))
                    .is_err()
            );
            assert!(!process.has_exited().unwrap());
            assert_eq!(fs::read(state.path(StateFile::Journal)).unwrap(), journal);
            assert_eq!(
                fs::read(state.path(StateFile::Capability)).unwrap(),
                capability
            );
        }
    }

    #[test]
    fn an_earlier_boot_proves_exit_without_signaling_a_process_from_this_boot() {
        let (_directory, state) = state();
        let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
        let mut locked = state.lock(Deadline::after(10.0)).unwrap();
        let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
        let child = running(&state, &prepared);
        let process = child.process();
        let mut boot = *BootId::current().unwrap().as_bytes();
        boot[0] ^= 1;
        launch
            .record_process(&ProcessRef {
                boot: BootId::from_bytes(boot),
                ..process.reference().clone()
            })
            .unwrap();
        drop(launch);
        drop(prepared);
        locked.recover(Instant::now()).unwrap();
        assert!(!process.has_exited().unwrap());
        assert!(locked.journal().unwrap().is_none());
        assert!(locked.capability().unwrap().is_none());
    }

    #[test]
    fn a_committed_runtime_is_not_recovered_through_its_leftover_journal() {
        let (_directory, state) = state();
        let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
        let mut locked = state.lock(Deadline::after(10.0)).unwrap();
        let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
        let child = running(&state, &prepared);
        let process = child.process();
        launch.record_process(process.reference()).unwrap();
        let journal = fs::read(state.path(StateFile::Journal)).unwrap();
        launch
            .commit(&SavedVm {
                vm_dir: VmDir::open(state.dir()).unwrap(),
                endpoint: prepared.outputs().endpoint.clone(),
                capability: *prepared.capability().as_bytes(),
                process: process.reference().clone(),
                runtime,
            })
            .unwrap();
        files::replace(&locked, StateFile::Journal, &journal).unwrap();
        let before = fs::read(state.path(StateFile::Runtime)).unwrap();
        drop(prepared);
        drop(locked);
        let locked = state.lock(Deadline::after(10.0)).unwrap();
        assert!(!process.has_exited().unwrap());
        assert_eq!(fs::read(state.path(StateFile::Runtime)).unwrap(), before);
        assert_eq!(fs::read(state.path(StateFile::Journal)).unwrap(), journal);
        assert!(locked.capability().unwrap().is_some());
    }

    #[test]
    #[ignore = "invoked as an owned subprocess by interrupted-launch recovery tests"]
    fn launch_helper() {
        let endpoint = fs::read_to_string("recovery-endpoint").unwrap();
        #[cfg(target_os = "linux")]
        let _server = std::os::unix::net::UnixListener::bind(endpoint).unwrap();
        #[cfg(windows)]
        let server = platform::tests::control_server(&endpoint);
        fs::write("recovery-ready", b"").unwrap();
        #[cfg(target_os = "linux")]
        thread::sleep(Duration::from_secs(30));
        #[cfg(windows)]
        {
            use std::io::Read;

            loop {
                platform::tests::accept_control(&server);
                match (&server).read(&mut [0]) {
                    Ok(0) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
                    result => panic!("unexpected endpoint probe: {result:?}"),
                }
                platform::tests::disconnect_control(&server);
            }
        }
    }
}
