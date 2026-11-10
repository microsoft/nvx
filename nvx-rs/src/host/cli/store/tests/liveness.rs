use std::error::Error as _;
use std::io;
#[cfg(any(target_os = "linux", windows))]
use std::process::{Child, Command, Stdio};

use super::launch::prepared;
use super::*;
use crate::host::cli::store::liveness::hold_endpoint_server;
use crate::host::config::ConsoleAttachment;
use crate::host::process::{BootId, Opened, Process, ProcessRef};
use crate::host::spawn::{self, VmmCommand};

const HELPER: &str = "host::cli::store::tests::liveness::log_holder_helper";

fn own_process() -> Process {
    let pid = std::process::id();
    let Opened::Running(process) = Process::open(&ProcessRef {
        pid,
        start_time: platform::process_start_time(pid).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    })
    .unwrap() else {
        panic!("the current process runs");
    };
    process
}

struct Running(Option<spawn::Launched>);

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(child) = self.0.take() {
            child.kill_and_confirm_exit().unwrap();
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
struct Other(Child);

#[cfg(any(target_os = "linux", windows))]
impl Drop for Other {
    fn drop(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

#[cfg(any(target_os = "linux", windows))]
fn helper_command(state: &StateDir) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", HELPER, "--ignored", "--nocapture"])
        .current_dir(state.dir())
        .stdin(Stdio::null());
    command
}

#[test]
fn a_claim_identifies_the_opened_log_and_last_close_proves_exit() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let identity = platform::opened_file_identity(prepared.log()).unwrap();
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    drop(locked.begin_launch(&prepared, &runtime).unwrap());
    let journal = locked.journal().unwrap().unwrap();
    assert_eq!(journal.log_identity, identity);
    assert!(locked.claim_held(&journal).unwrap());
    let copy = prepared.log().try_clone().unwrap();
    drop(prepared);
    assert!(locked.claim_held(&journal).unwrap());
    drop(copy);
    assert!(released(|| !locked.claim_held(&journal).unwrap()));
    assert!(locked.journal().unwrap().is_some());
    assert!(locked.capability().unwrap().is_some());
}

#[test]
fn replacing_the_log_or_sandbox_never_proves_the_launch_exited() {
    let (_directory, state) = state();
    let (_other_directory, other) = super::state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    drop(locked.begin_launch(&prepared, &runtime).unwrap());
    let journal = locked.journal().unwrap().unwrap();
    let elsewhere = other.lock(Deadline::after(10.0)).unwrap();
    assert!(elsewhere.claim_held(&journal).is_err());
    drop(prepared);
    fs::rename(state.path(StateFile::Log), state.dir().join("old-log")).unwrap();
    fs::write(state.path(StateFile::Log), b"another launch").unwrap();
    let error = locked.claim_held(&journal).unwrap_err();
    assert!(error.source().is_some());
    assert!(
        error
            .to_string()
            .contains("cannot check the launch log claim")
    );
    assert!(locked.endpoint_owner(&journal).is_err());
    assert!(locked.journal().unwrap().is_some());
}

#[test]
fn the_spawned_process_inherits_the_preparations_claim() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    drop(locked.begin_launch(&prepared, &runtime).unwrap());
    let journal = locked.journal().unwrap().unwrap();
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
    drop(prepared);
    assert!(released(|| state.dir().join("holder-ready").exists()));
    assert!(locked.claim_held(&journal).unwrap());
    #[cfg(target_os = "linux")]
    {
        let owner = locked.endpoint_owner(&journal).unwrap();
        assert_eq!(owner.reference().pid, child.0.as_ref().unwrap().id());
        assert!(!owner.has_exited().unwrap());
    }
    drop(child);
    assert!(released(|| !locked.claim_held(&journal).unwrap()));
}

#[cfg(target_os = "linux")]
#[test]
fn a_conflicting_lock_prevents_publishing_a_launch() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let other = fs::File::open(prepared.outputs().log.clone()).unwrap();
    other.try_lock().unwrap();
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    let error = locked.begin_launch(&prepared, &runtime).unwrap_err();
    assert!(error.source().is_some());
    assert!(locked.journal().unwrap().is_none());
    assert!(locked.capability().unwrap().is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn an_endpoint_owner_without_the_claimed_log_is_rejected_without_signaling_it() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    drop(locked.begin_launch(&prepared, &runtime).unwrap());
    let journal = locked.journal().unwrap().unwrap();
    let mut other = Other(
        helper_command(&state)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(released(|| state.dir().join("holder-ready").exists()));
    assert!(locked.claim_held(&journal).unwrap());
    let error = locked.endpoint_owner(&journal).err().unwrap();
    assert!(error.to_string().contains("does not hold the launch log"));
    assert!(other.0.try_wait().unwrap().is_none());
    drop(prepared);
    assert!(released(|| !locked.claim_held(&journal).unwrap()));
    assert!(locked.endpoint_owner(&journal).is_err());
    assert!(other.0.try_wait().unwrap().is_none());
}

#[cfg(windows)]
#[test]
fn without_a_log_job_inherited_handles_remain_the_claim() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    drop(locked.begin_launch(&prepared, &runtime).unwrap());
    let journal = locked.journal().unwrap().unwrap();
    let mut command = helper_command(&state);
    command
        .stdout(Stdio::from(prepared.log().try_clone().unwrap()))
        .stderr(Stdio::from(prepared.log().try_clone().unwrap()));
    let child = Other(spawn_locked(&mut command).unwrap());
    drop(command);
    drop(prepared);
    assert!(released(|| state.dir().join("holder-ready").exists()));
    assert!(locked.claim_held(&journal).unwrap());
    drop(child);
    assert!(released(|| !locked.claim_held(&journal).unwrap()));
}

#[test]
fn endpoint_ownership_is_rechecked_only_after_the_server_is_held() {
    let pid = std::process::id();
    let held = std::cell::Cell::new(false);
    let mut lookups = 0;
    let owner = hold_endpoint_server(
        || {
            lookups += 1;
            assert_eq!(held.get(), lookups == 2);
            Ok(Some(pid))
        },
        |found| {
            assert_eq!(found, pid);
            held.set(true);
            Ok(Some(own_process()))
        },
    )
    .unwrap();
    assert_eq!(owner.reference().pid, pid);
    assert_eq!(lookups, 2);
    for replacement in [None, Some(pid.wrapping_add(1))] {
        let mut lookups = 0;
        let error = hold_endpoint_server(
            || {
                lookups += 1;
                Ok(if lookups == 1 { Some(pid) } else { replacement })
            },
            |_| Ok(Some(own_process())),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("endpoint changed"));
        assert_eq!(lookups, 2);
    }
}

#[test]
fn missing_owners_and_lookup_or_hold_errors_are_not_exit_proofs() {
    assert!(hold_endpoint_server(|| Ok(None), |_| panic!("no owner was found")).is_err());
    assert!(hold_endpoint_server(|| Ok(Some(1)), |_| Ok(None)).is_err());
    for phase in 0..3 {
        let mut lookups = 0;
        let error = hold_endpoint_server(
            || {
                lookups += 1;
                if (phase == 0 && lookups == 1) || (phase == 2 && lookups == 2) {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    Ok(Some(std::process::id()))
                }
            },
            |_| {
                if phase == 1 {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    Ok(Some(own_process()))
                }
            },
        )
        .err()
        .unwrap();
        let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
    }
}

#[test]
#[ignore = "invoked as an owned subprocess by the launch-claim tests"]
fn log_holder_helper() {
    #[cfg(target_os = "linux")]
    let _listener = std::os::unix::net::UnixListener::bind("control.sock").unwrap();
    fs::write("holder-ready", b"").unwrap();
    thread::sleep(Duration::from_secs(30));
}
